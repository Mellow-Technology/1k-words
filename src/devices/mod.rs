//! Removable storage — SD cards, USB drives and cameras that mount as disks —
//! listed in the sidebar, as in Apple's Photos.
//!
//! The list is app-wide state, kept up to date as volumes mount and unmount.
//! Only macOS can list devices so far; elsewhere the list stays empty.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as platform;

#[cfg(not(target_os = "macos"))]
mod other;
#[cfg(not(target_os = "macos"))]
use other as platform;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, anyhow};
use futures::StreamExt as _;
use gpui_kit::{App, AppContext as _, AsyncApp, Global, SharedString, Task};

/// A mounted removable volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub name: SharedString,
    /// Where it's mounted, e.g. `/Volumes/EOS_DIGITAL`.
    pub path: PathBuf,
}

/// What the platform reports about the mounted volumes.
enum Event {
    /// A volume mounted, unmounted or was renamed.
    Changed,
    /// The volume mounted at this path is about to unmount.
    WillUnmount(PathBuf),
}

/// How a volume describes itself, for deciding whether it's a device.
#[derive(Clone, Copy, Debug, Default)]
struct VolumeFlags {
    local: bool,
    root: bool,
    removable: bool,
    ejectable: bool,
}

/// Local volumes that can be removed or ejected count as devices, which
/// leaves out network shares and the startup disk. Mounted disk images are
/// ejectable too, but they're files, not devices.
fn is_device(flags: VolumeFlags, disk_image: bool) -> bool {
    flags.local && !flags.root && (flags.removable || flags.ejectable) && !disk_image
}

/// If an unmount fails (e.g. another app is using the volume) there's no
/// unmount notification, so the list is read again after this long.
const UNMOUNT_RECHECK_DELAY: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct Devices {
    list: Vec<Device>,
}

impl Global for Devices {}

pub fn init(cx: &mut App) {
    cx.set_global(Devices {
        list: platform::mounted_devices(),
    });
    let (watcher, mut events) = platform::watch();
    cx.spawn(async move |cx| {
        let _watcher = watcher;
        while let Some(event) = events.next().await {
            match event {
                Event::Changed => refresh(cx).await,
                Event::WillUnmount(path) => {
                    // Drop the device straight away, so the gallery lets go
                    // of the files it has open on it before it unmounts.
                    cx.update(|cx| {
                        let mut list = list(cx).to_vec();
                        list.retain(|device| device.path != path);
                        set_list(list, cx);
                    });
                    cx.spawn(async |cx| {
                        cx.background_executor().timer(UNMOUNT_RECHECK_DELAY).await;
                        refresh(cx).await;
                    })
                    .detach();
                }
            }
        }
    })
    .detach();
}

async fn refresh(cx: &mut AsyncApp) {
    let list = cx
        .background_spawn(async { platform::mounted_devices() })
        .await;
    cx.update(|cx| set_list(list, cx));
}

pub(crate) fn set_list(list: Vec<Device>, cx: &mut App) {
    if list != self::list(cx) {
        cx.set_global(Devices { list });
        cx.refresh_windows();
    }
}

/// The mounted devices, in the order the system lists them.
pub fn list(cx: &App) -> &[Device] {
    cx.try_global::<Devices>()
        .map_or(&[], |devices| devices.list.as_slice())
}

/// Unmounts the device and ejects its disk (or card).
pub fn eject(device: &Device, cx: &App) -> Task<Result<()>> {
    let done = platform::eject(&device.path);
    let name = device.name.clone();
    cx.spawn(async move |_| match done.await {
        Ok(result) => result.map_err(|reason| anyhow!("{reason}")),
        Err(_) => Err(anyhow!("couldn't eject {name}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_are_local_removable_or_ejectable_volumes() {
        let card = VolumeFlags {
            local: true,
            removable: true,
            ejectable: true,
            ..Default::default()
        };
        let external_ssd = VolumeFlags {
            local: true,
            ejectable: true,
            ..Default::default()
        };
        let startup_disk = VolumeFlags {
            local: true,
            root: true,
            ..Default::default()
        };
        let internal_data = VolumeFlags {
            local: true,
            ..Default::default()
        };
        let network_share = VolumeFlags {
            ejectable: true,
            ..Default::default()
        };
        assert!(is_device(card, false));
        assert!(is_device(external_ssd, false));
        assert!(!is_device(startup_disk, false));
        assert!(!is_device(internal_data, false));
        assert!(!is_device(network_share, false));
        // A mounted .dmg looks like a removable, ejectable disk.
        assert!(!is_device(card, true));
    }
}

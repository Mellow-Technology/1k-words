//! Volumes from `NSFileManager`, mount notifications from `NSWorkspace`,
//! and disk details from DiskArbitration.

use std::path::{Path, PathBuf};
use std::ptr::NonNull;
use std::sync::Mutex;

use block2::RcBlock;
use futures::channel::{mpsc, oneshot};
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObjectProtocol, ProtocolObject};
use objc2_app_kit::{
    NSWorkspace, NSWorkspaceDidMountNotification, NSWorkspaceDidRenameVolumeNotification,
    NSWorkspaceDidUnmountNotification, NSWorkspaceVolumeURLKey, NSWorkspaceWillUnmountNotification,
};
use objc2_core_foundation::{CFDictionary, CFRetained, CFString, CFType, CFURL};
use objc2_disk_arbitration::{DADisk, DASession, kDADiskDescriptionDeviceModelKey};
use objc2_foundation::{
    NSArray, NSDictionary, NSError, NSFileManager, NSFileManagerUnmountOptions, NSNotification,
    NSNotificationCenter, NSNotificationName, NSNumber, NSString, NSURL, NSURLResourceKey,
    NSURLVolumeIsEjectableKey, NSURLVolumeIsLocalKey, NSURLVolumeIsRemovableKey,
    NSURLVolumeIsRootFileSystemKey, NSURLVolumeLocalizedNameKey, NSVolumeEnumerationOptions,
};

use super::{Device, Event, VolumeFlags, is_device};

pub(super) fn mounted_devices() -> Vec<Device> {
    // SAFETY: the keys are immutable constants from Foundation.
    let keys = unsafe {
        NSArray::from_slice(&[
            NSURLVolumeIsLocalKey,
            NSURLVolumeIsRootFileSystemKey,
            NSURLVolumeIsRemovableKey,
            NSURLVolumeIsEjectableKey,
            NSURLVolumeLocalizedNameKey,
        ])
    };
    let Some(urls) = NSFileManager::defaultManager()
        .mountedVolumeURLsIncludingResourceValuesForKeys_options(
            Some(&keys),
            NSVolumeEnumerationOptions::SkipHiddenVolumes,
        )
    else {
        return Vec::new();
    };
    // SAFETY: called with the default allocator.
    let session = unsafe { DASession::new(None) };
    urls.iter()
        .filter_map(|url| device(&url, &keys, session.as_deref()))
        .collect()
}

fn device(
    url: &NSURL,
    keys: &NSArray<NSURLResourceKey>,
    session: Option<&DASession>,
) -> Option<Device> {
    let values = url.resourceValuesForKeys_error(keys).ok()?;
    // SAFETY: as above.
    let flags = unsafe {
        VolumeFlags {
            local: flag(&values, NSURLVolumeIsLocalKey),
            root: flag(&values, NSURLVolumeIsRootFileSystemKey),
            removable: flag(&values, NSURLVolumeIsRemovableKey),
            ejectable: flag(&values, NSURLVolumeIsEjectableKey),
        }
    };
    let path = PathBuf::from(url.path()?.to_string());
    let disk_image = session.is_some_and(|session| is_disk_image(&path, session));
    if !is_device(flags, disk_image) {
        return None;
    }
    // SAFETY: as above.
    let name = unsafe { values.objectForKey(NSURLVolumeLocalizedNameKey) }
        .and_then(|name| name.downcast::<NSString>().ok())
        .map(|name| name.to_string())
        .or_else(|| Some(path.file_name()?.to_string_lossy().into_owned()))
        .unwrap_or_else(|| path.display().to_string());
    Some(Device {
        name: name.into(),
        path,
    })
}

fn flag(values: &NSDictionary<NSURLResourceKey, AnyObject>, key: &NSURLResourceKey) -> bool {
    values
        .objectForKey(key)
        .and_then(|value| value.downcast::<NSNumber>().ok())
        .is_some_and(|value| value.boolValue())
}

/// Whether the volume mounted at `path` is a disk image (e.g. a `.dmg`),
/// which DiskArbitration reports as the disk's model.
fn is_disk_image(path: &Path, session: &DASession) -> bool {
    let Some(url) = CFURL::from_directory_path(path) else {
        return false;
    };
    // SAFETY: the session and URL are valid; the default allocator is used.
    let Some(description) = (unsafe { DADisk::from_volume_path(None, session, &url) })
        .and_then(|disk| unsafe { disk.description() })
    else {
        return false;
    };
    // SAFETY: a disk description has string keys.
    let description: CFRetained<CFDictionary<CFString, CFType>> =
        unsafe { CFRetained::cast_unchecked(description) };
    // SAFETY: the key is an immutable constant from DiskArbitration.
    let model = description.get(unsafe { kDADiskDescriptionDeviceModelKey });
    model
        .and_then(|model| model.downcast::<CFString>().ok())
        .is_some_and(|model| model.to_string() == "Disk Image")
}

/// Observes `NSWorkspace`'s volume notifications until dropped.
pub(super) struct Watcher {
    center: Retained<NSNotificationCenter>,
    observers: Vec<Retained<ProtocolObject<dyn NSObjectProtocol>>>,
}

impl Drop for Watcher {
    fn drop(&mut self) {
        for observer in &self.observers {
            // SAFETY: each observer was added to this center.
            unsafe { self.center.removeObserver(observer.as_ref()) };
        }
    }
}

pub(super) fn watch() -> (Watcher, mpsc::UnboundedReceiver<Event>) {
    let (tx, rx) = mpsc::unbounded();
    let center = NSWorkspace::sharedWorkspace().notificationCenter();
    let observe = |name: &NSNotificationName, event: fn(&NSNotification) -> Event| {
        let tx = tx.clone();
        let block = RcBlock::new(move |notification: NonNull<NSNotification>| {
            // SAFETY: the notification is valid for the block's call.
            let event = event(unsafe { notification.as_ref() });
            tx.unbounded_send(event).ok();
        });
        // SAFETY: the block only sends on a channel, which is thread-safe.
        unsafe { center.addObserverForName_object_queue_usingBlock(Some(name), None, None, &block) }
    };
    // SAFETY: the names are immutable constants from AppKit.
    let observers = unsafe {
        vec![
            observe(NSWorkspaceDidMountNotification, |_| Event::Changed),
            observe(NSWorkspaceDidUnmountNotification, |_| Event::Changed),
            observe(NSWorkspaceDidRenameVolumeNotification, |_| Event::Changed),
            observe(NSWorkspaceWillUnmountNotification, |notification| {
                volume_path(notification).map_or(Event::Changed, Event::WillUnmount)
            }),
        ]
    };
    (Watcher { center, observers }, rx)
}

/// The mount point a workspace notification is about.
fn volume_path(notification: &NSNotification) -> Option<PathBuf> {
    let info = notification.userInfo()?;
    // SAFETY: the key is an immutable constant from AppKit.
    let key: &AnyObject = unsafe { NSWorkspaceVolumeURLKey }.as_ref();
    let url = info.objectForKey(key)?.downcast::<NSURL>().ok()?;
    Some(PathBuf::from(url.path()?.to_string()))
}

/// Unmounts the volume at `path` and ejects its disk. Resolves with the
/// system's reason if it couldn't, e.g. because the volume is in use.
pub(super) fn eject(path: &Path) -> oneshot::Receiver<Result<(), String>> {
    let (tx, rx) = oneshot::channel();
    let url =
        NSURL::fileURLWithPath_isDirectory(&NSString::from_str(&path.to_string_lossy()), true);
    let tx = Mutex::new(Some(tx));
    let block = RcBlock::new(move |error: *mut NSError| {
        // SAFETY: the error, if any, is valid for the block's call.
        let result = match unsafe { error.as_ref() } {
            None => Ok(()),
            Some(error) => Err(error.localizedDescription().to_string()),
        };
        if let Some(tx) = tx.lock().unwrap().take() {
            tx.send(result).ok();
        }
    });
    // SAFETY: the block only sends on a channel, which is thread-safe.
    unsafe {
        NSFileManager::defaultManager().unmountVolumeAtURL_options_completionHandler(
            &url,
            NSFileManagerUnmountOptions::AllPartitionsAndEjectDisk,
            &block,
        );
    }
    rx
}

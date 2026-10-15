//! Platforms without device support yet: no devices, no events.
//!
//! Linux could use UDisks2 and Windows `GetLogicalDrives` with
//! `WM_DEVICECHANGE`.

use std::path::Path;

use futures::channel::{mpsc, oneshot};

use super::{Device, Event};

pub(super) fn mounted_devices() -> Vec<Device> {
    Vec::new()
}

/// Holds the sender, so the event stream stays open (and empty).
pub(super) struct Watcher(#[allow(dead_code)] mpsc::UnboundedSender<Event>);

pub(super) fn watch() -> (Watcher, mpsc::UnboundedReceiver<Event>) {
    let (tx, rx) = mpsc::unbounded();
    (Watcher(tx), rx)
}

pub(super) fn eject(_path: &Path) -> oneshot::Receiver<Result<(), String>> {
    let (tx, rx) = oneshot::channel();
    tx.send(Err("Ejecting isn't supported on this platform.".into()))
        .ok();
    rx
}

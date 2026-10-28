//! This device, as the catalogs know it: an id made the first time the app
//! runs, and a name to show, kept in `device.json` in the app's config
//! directory. Each change to a catalog records the device it was made on
//! (see `catalog`), so a conflict between devices can say where each side
//! came from.

use std::path::Path;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use super::id::Id;

const DEVICE_FILE: &str = "device.json";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub id: Id,
    pub name: String,
}

#[derive(Serialize, Deserialize)]
struct DeviceFile {
    id: String,
}

impl Device {
    /// This device, making its id if it hasn't one yet.
    pub fn load(config_dir: &Path) -> Result<Self> {
        let path = config_dir.join(DEVICE_FILE);
        let saved = std::fs::read(&path)
            .ok()
            .and_then(|json| serde_json::from_slice::<DeviceFile>(&json).ok())
            .and_then(|file| Id::parse(&file.id));
        let id = match saved {
            Some(id) => id,
            None => {
                let id = Id::new();
                std::fs::create_dir_all(config_dir)?;
                let file = DeviceFile { id: id.to_string() };
                std::fs::write(&path, serde_json::to_vec_pretty(&file)?)?;
                id
            }
        };
        Ok(Self {
            id,
            name: device_name(),
        })
    }
}

/// The name the user gave this computer, e.g. "Kyle's MacBook Pro".
fn device_name() -> String {
    #[cfg(target_os = "macos")]
    let name = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string());
    #[cfg(not(target_os = "macos"))]
    let name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|name| name.trim().to_string())
        });
    name.filter(|name| !name.is_empty())
        .unwrap_or_else(|| "This device".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_its_id() {
        let dir = std::env::temp_dir().join(format!("1k-words-device-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let device = Device::load(&dir).unwrap();
        assert_eq!(Device::load(&dir).unwrap().id, device.id);
        assert!(!device.name.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }
}

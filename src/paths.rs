//! Where the app keeps its files on each platform.

use std::path::PathBuf;

/// The app's own files: settings and each library's catalog.
///
/// - macOS: `~/Library/Application Support/1k Words`
/// - Windows: `%APPDATA%\1k Words`
/// - Linux: `~/.1k-words`
pub fn config_dir() -> Option<PathBuf> {
    if cfg!(any(target_os = "macos", target_os = "windows")) {
        Some(dirs::data_dir()?.join("1k Words"))
    } else {
        Some(dirs::home_dir()?.join(".1k-words"))
    }
}

/// Where a new library goes unless the user picks somewhere else: a
/// `1K Words` folder in the user's Pictures folder.
pub fn default_library_dir() -> Option<PathBuf> {
    let pictures = dirs::picture_dir().or_else(|| Some(dirs::home_dir()?.join("Pictures")))?;
    Some(pictures.join(DEFAULT_LIBRARY_NAME))
}

pub const DEFAULT_LIBRARY_NAME: &str = "1K Words";

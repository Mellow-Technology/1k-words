//! Read-only protection for a library's files, so they aren't changed or
//! deleted by accident outside the app.
//!
//! On macOS and Linux, deleting or renaming a file needs write permission on
//! its folder rather than the file, so folders are made read-only as well as
//! files. Windows ignores the read-only attribute on folders, so only files
//! are protected there.

use std::io;
use std::path::{Path, PathBuf};

/// Makes `path` read-only, or writable again, for its owner.
pub fn set_writable(path: &Path, writable: bool) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = permissions.mode();
        permissions.set_mode(if writable {
            mode | 0o200
        } else {
            mode & !0o222
        });
    }
    #[cfg(not(unix))]
    {
        if path.is_dir() {
            return Ok(());
        }
        permissions.set_readonly(!writable);
    }
    std::fs::set_permissions(path, permissions)
}

/// Makes `path` and everything in it read-only, or writable again.
pub fn set_tree_writable(path: &Path, writable: bool) -> io::Result<()> {
    // A folder must be readable to list it, which read-only doesn't change.
    if path.is_dir() {
        for entry in std::fs::read_dir(path)? {
            set_tree_writable(&entry?.path(), writable)?;
        }
    }
    // Symbolic links' permissions are their targets'; leave them be.
    if !std::fs::symlink_metadata(path)?.is_symlink() {
        set_writable(path, writable)?;
    }
    Ok(())
}

fn is_writable(path: &Path) -> io::Result<bool> {
    Ok(!std::fs::metadata(path)?.permissions().readonly())
}

/// Folders made writable (or created) for a while, made read-only again
/// when this is dropped. Does nothing when protection is off.
pub struct Unlocked {
    protect: bool,
    dirs: Vec<PathBuf>,
}

impl Unlocked {
    pub fn new(protect: bool) -> Self {
        Self {
            protect,
            dirs: Vec::new(),
        }
    }

    /// Makes `dir` writable until this is dropped.
    pub fn unlock(&mut self, dir: &Path) -> io::Result<()> {
        if self.protect && !self.dirs.iter().any(|d| d == dir) {
            if !is_writable(dir)? {
                set_writable(dir, true)?;
            }
            self.dirs.push(dir.to_path_buf());
        }
        Ok(())
    }

    /// Creates `base/components…` as needed and makes each folder along the
    /// way writable until this is dropped, so files can be added to the last.
    /// `base` must exist.
    pub fn create_dirs(&mut self, base: &Path, components: &[&str]) -> io::Result<PathBuf> {
        self.unlock(base)?;
        let mut dir = base.to_path_buf();
        for component in components {
            dir.push(component);
            if !dir.is_dir() {
                std::fs::create_dir(&dir)?;
            }
            self.unlock(&dir)?;
        }
        Ok(dir)
    }

    /// Makes the folders read-only again now, reporting the first failure.
    pub fn relock(mut self) -> io::Result<()> {
        self.relock_all()
    }

    fn relock_all(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        // Children first, though the order doesn't matter for permissions.
        for dir in self.dirs.drain(..).rev() {
            if let Err(error) = set_writable(&dir, false)
                && result.is_ok()
            {
                result = Err(error);
            }
        }
        result
    }
}

impl Drop for Unlocked {
    fn drop(&mut self) {
        self.relock_all().ok();
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("1k-words-protect-{}-{name}", std::process::id()));
        if dir.exists() {
            unlock_all(&dir);
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Makes everything under `dir` writable, so tests can clean up.
    pub(crate) fn unlock_all(dir: &Path) {
        set_tree_writable(dir, true).ok();
    }

    #[test]
    fn creates_folders_and_relocks_them() {
        let base = temp_dir("create");
        set_writable(&base, false).unwrap();

        let mut unlocked = Unlocked::new(true);
        let day = unlocked.create_dirs(&base, &["2026", "10", "15"]).unwrap();
        std::fs::write(day.join("photo.jpg"), b"jpeg").unwrap();
        assert!(is_writable(&base).unwrap());
        unlocked.relock().unwrap();

        for dir in [&base, &base.join("2026"), &base.join("2026/10"), &day] {
            assert!(!is_writable(dir).unwrap(), "{} is writable", dir.display());
        }
        #[cfg(unix)]
        {
            // A file in a read-only folder can't be deleted.
            assert!(std::fs::remove_file(day.join("photo.jpg")).is_err());
        }

        unlock_all(&base);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn leaves_folders_alone_when_protection_is_off() {
        let base = temp_dir("off");
        let mut unlocked = Unlocked::new(false);
        let dir = unlocked.create_dirs(&base, &["a"]).unwrap();
        drop(unlocked);
        assert!(is_writable(&dir).unwrap());
        std::fs::remove_dir_all(&base).unwrap();
    }
}

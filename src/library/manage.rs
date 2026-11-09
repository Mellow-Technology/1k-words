//! Looking after whole libraries: how big they are, moving them and
//! deleting them. These block; run them in the background.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail};

use super::catalog::{Catalog, DeviceSyncRecord};
use super::device::Device;
use super::sync::{LibraryTarget, SyncStatus};
use super::{Library, LibraryInfo, ORIGINALS_DIR, marker_id, protect, sync};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryStats {
    /// A raw pair or Live Photo counts as one.
    pub photos: usize,
    /// Of every file in the library's folder.
    pub bytes: u64,
}

/// How far a move has got.
#[derive(Debug, Default)]
pub struct MoveProgress {
    /// Bytes copied so far, of `total`. Both stay 0 if the folder could
    /// just be renamed.
    pub copied: AtomicU64,
    pub total: AtomicU64,
}

/// A library, open or not, to work on in the background.
#[derive(Clone)]
pub struct LibraryHandle {
    pub info: LibraryInfo,
    pub(super) catalog_path: PathBuf,
    /// The library, if it's open.
    pub(super) open: Option<Arc<Library>>,
}

impl LibraryHandle {
    /// Runs `f` with the library's catalog: the open library's, or opened
    /// for the purpose. `None` if it has no catalog yet.
    fn with_catalog<T>(&self, f: impl FnOnce(&Catalog) -> Result<T>) -> Result<Option<T>> {
        if let Some(library) = &self.open {
            return f(&library.catalog).map(Some);
        }
        if !self.catalog_path.exists() {
            return Ok(None);
        }
        // Catalogs are kept in `<config>/libraries`.
        let config_dir = self
            .catalog_path
            .parent()
            .and_then(Path::parent)
            .context("a catalog outside the config directory")?;
        let device = Device::load(config_dir)?;
        f(&Catalog::open(&self.catalog_path, &device)?).map(Some)
    }

    pub fn stats(&self) -> Result<LibraryStats> {
        let path = &self.info.path;
        if !path.is_dir() {
            bail!("{} isn't there", path.display());
        }
        Ok(LibraryStats {
            photos: self.with_catalog(|c| c.photo_count())?.unwrap_or(0),
            bytes: folder_size(path)?,
        })
    }

    /// The targets the library syncs to, and how its files stand with each.
    pub fn sync_overview(&self) -> Result<Vec<(LibraryTarget, SyncStatus)>> {
        let overview = self.with_catalog(|catalog| {
            sync::sync_targets(catalog)?
                .into_iter()
                .map(|target| {
                    let status = sync::sync_status(catalog, target.id)?;
                    Ok((target, status))
                })
                .collect()
        })?;
        Ok(overview.unwrap_or_default())
    }

    /// Every device that has opened the library's catalog, and how its
    /// syncs last went (see `Catalog::device_sync`).
    pub fn device_sync(&self) -> Result<Vec<DeviceSyncRecord>> {
        Ok(self
            .with_catalog(|catalog| catalog.device_sync())?
            .unwrap_or_default())
    }

    /// Stops syncing to a target (by its id in `sync_targets`), as when
    /// it's deleted.
    pub fn forget_sync_target(&self, target: &str) -> Result<()> {
        self.with_catalog(|catalog| {
            for link in sync::sync_targets(catalog)? {
                if link.target == target {
                    catalog.remove_sync_target(link.id)?;
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    /// Moves the library's folder to `dest`, which mustn't exist yet: by
    /// renaming it if it can, otherwise by copying it and then deleting the
    /// original. Returns a warning if the move worked but the original
    /// couldn't be deleted.
    pub fn move_to(&self, dest: &Path, progress: &MoveProgress) -> Result<Option<String>> {
        let source = &self.info.path;
        self.check_marker()?;
        if dest.exists() {
            bail!("There's already something at {}.", dest.display());
        }
        if dest.starts_with(source) {
            bail!("A library can't be moved into itself.");
        }
        // Moving a folder to another needs write permission on it (to
        // update its `..`), which a protected library's may not have.
        let was_writable = !std::fs::metadata(source)?.permissions().readonly();
        protect::set_writable(source, true)?;
        let relock = |path: &Path| {
            if !was_writable {
                protect::set_writable(path, false).ok();
            }
        };
        match std::fs::rename(source, dest) {
            Ok(()) => {
                relock(dest);
                return Ok(None);
            }
            Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {}
            Err(error) => {
                relock(source);
                return Err(error).with_context(|| format!("moving to {}", dest.display()));
            }
        }

        // To another disk.
        progress
            .total
            .store(folder_size(source)?, Ordering::Relaxed);
        let copied = copy_folder(source, dest, &progress.copied).and_then(|()| {
            if self.info.read_only {
                protect::set_tree_writable(&dest.join(ORIGINALS_DIR), false)?;
            }
            Ok(())
        });
        if let Err(error) = copied {
            protect::set_tree_writable(dest, true).ok();
            std::fs::remove_dir_all(dest).ok();
            relock(source);
            return Err(error).with_context(|| format!("copying to {}", dest.display()));
        }
        relock(dest);
        let removed =
            protect::set_tree_writable(source, true).and_then(|()| std::fs::remove_dir_all(source));
        Ok(removed.err().map(|error| {
            format!(
                "The library was copied, but the original at {} couldn't be deleted: {error}",
                source.display()
            )
        }))
    }

    /// Moves the library's folder to the Trash (if it's still there), and
    /// deletes its catalog.
    pub fn delete(&self) -> Result<()> {
        let path = &self.info.path;
        if path.exists() {
            self.check_marker()?;
            // Writable, so it can be moved and the Trash emptied without
            // complaint.
            protect::set_tree_writable(path, true)?;
            if let Err(error) = trash(path) {
                if self.info.read_only {
                    protect::set_tree_writable(&path.join(ORIGINALS_DIR), false).ok();
                }
                return Err(error).context("moving the library to the Trash");
            }
        }
        super::catalog::remove_database(&self.catalog_path)
            .context("deleting the library's catalog")?;
        super::catalog::remove_database(&self.catalog_path.with_extension("db.before-sync"))
            .context("deleting the library's catalog")?;
        Ok(())
    }

    /// Makes sure the folder is this library's, before moving or deleting
    /// it.
    fn check_marker(&self) -> Result<()> {
        if marker_id(&self.info.path).as_deref() != Some(self.info.id.as_str()) {
            bail!("{} isn't this library's folder", self.info.path.display());
        }
        Ok(())
    }
}

/// The total size of the files in `dir`.
fn folder_size(dir: &Path) -> Result<u64> {
    let mut size = 0;
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            size += folder_size(&entry.path())?;
        } else if file_type.is_file() {
            size += entry.metadata()?.len();
        }
    }
    Ok(size)
}

/// Copies `source` to `dest` (which mustn't exist), keeping files'
/// permissions and modification times.
fn copy_folder(source: &Path, dest: &Path, copied: &AtomicU64) -> Result<()> {
    std::fs::create_dir(dest).with_context(|| format!("creating {}", dest.display()))?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let (from, to) = (entry.path(), dest.join(entry.file_name()));
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            copy_folder(&from, &to, copied)?;
        } else if file_type.is_file() {
            let copy = || -> std::io::Result<u64> {
                let metadata = entry.metadata()?;
                let mut writer = File::create(&to)?;
                let size = std::io::copy(&mut File::open(&from)?, &mut writer)?;
                writer.set_modified(metadata.modified()?)?;
                drop(writer);
                // Last, as it may make the file read-only.
                std::fs::set_permissions(&to, metadata.permissions())?;
                Ok(size)
            };
            let size = copy().with_context(|| format!("copying {}", from.display()))?;
            copied.fetch_add(size, Ordering::Relaxed);
        }
    }
    Ok(())
}

/// Moves `path` to the Trash. Tests delete it instead.
#[cfg(test)]
fn trash(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir_all(path)
}

#[cfg(not(test))]
fn trash(path: &Path) -> Result<(), trash::Error> {
    #[allow(unused_mut)]
    let mut context = trash::TrashContext::default();
    // The default asks the Finder, which needs permission to automate it.
    #[cfg(target_os = "macos")]
    trash::macos::TrashContextExtMacos::set_delete_method(
        &mut context,
        trash::macos::DeleteMethod::NsFileManager,
    );
    context.delete(path)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::super::{self as library, test_support};
    use super::*;

    fn jpeg(path: &Path, shade: u8) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([shade, 90, 160]))
            .save(path)
            .unwrap();
    }

    #[gpui_kit::test]
    fn sizes_moves_and_deletes_libraries(cx: &mut gpui_kit::TestAppContext) {
        let (config_dir, default_dir) = test_support::temp_dirs("manage");
        let root = config_dir.parent().unwrap().to_path_buf();
        let source = root.join("card");
        jpeg(&source.join("IMG_0001.JPG"), 10);
        jpeg(&source.join("IMG_0002.JPG"), 20);
        cx.update(|cx| library::init_at(config_dir.clone(), default_dir.clone(), cx));
        let other_dir = root.join("Family");
        cx.update(|cx| library::add(other_dir.clone(), cx)).unwrap();
        let first_id = cx.update(|cx| library::list(cx)[0].id.clone());
        cx.update(|cx| library::open(&first_id, cx)).unwrap();
        let library = cx.update(|cx| library::current(cx)).unwrap();
        library.import(std::slice::from_ref(&source)).unwrap();

        let handle = cx.update(|cx| library::handle(&first_id, cx)).unwrap();
        let stats = handle.stats().unwrap();
        assert_eq!(stats.photos, 2);
        let photo = &library.photos().unwrap()[0].path;
        assert!(stats.bytes > 2 * std::fs::metadata(photo).unwrap().len());
        let other = cx
            .update(|cx| library::handle(&library::list(cx)[1].id, cx))
            .unwrap();
        assert_eq!(other.stats().unwrap().photos, 0);

        // Moving: the folder's renamed and the library reopened there.
        let dest = root.join("Moved").join("Holidays");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        let progress = MoveProgress::default();
        assert_eq!(handle.move_to(&dest, &progress).unwrap(), None);
        drop((library, handle));
        cx.update(|cx| library::relocate(&first_id, dest.clone(), cx))
            .unwrap();
        let library = cx.update(|cx| library::current(cx)).unwrap();
        assert_eq!(library.info.path, dest);
        assert_eq!(library.info.name, "Holidays");
        assert!(!default_dir.exists());
        let photos = library.photos().unwrap();
        assert_eq!(photos.len(), 2);
        assert!(
            photos
                .iter()
                .all(|p| p.path.starts_with(&dest) && p.path.exists())
        );

        // Not into itself, nor onto something that's there.
        let handle = cx.update(|cx| library::handle(&first_id, cx)).unwrap();
        assert!(handle.move_to(&dest.join("inside"), &progress).is_err());
        assert!(handle.move_to(&other_dir, &progress).is_err());

        // Deleting: the folder and catalog go, and another library opens.
        let catalog = handle.catalog_path.clone();
        assert!(catalog.exists());
        drop(library);
        handle.delete().unwrap();
        drop(handle);
        assert!(!dest.exists());
        assert!(!catalog.exists());
        cx.update(|cx| library::forget(&first_id, cx)).unwrap();
        assert_eq!(cx.update(|cx| library::list(cx).len()), 1);
        let current = cx.update(|cx| library::current(cx)).unwrap();
        assert_eq!(current.info.path, other_dir);

        test_support::remove(&root);
    }

    #[test]
    fn copies_folders_keeping_times_and_permissions() {
        let (config_dir, _) = test_support::temp_dirs("copy");
        let root = config_dir.parent().unwrap().to_path_buf();
        let source = root.join("source");
        let file = source.join("Originals/2026/a.jpg");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, b"photo").unwrap();
        let modified = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(modified)
            .unwrap();
        protect::set_writable(&file, false).unwrap();

        let dest = root.join("dest");
        let copied = AtomicU64::new(0);
        copy_folder(&source, &dest, &copied).unwrap();
        assert_eq!(copied.load(Ordering::Relaxed), 5);
        assert_eq!(folder_size(&dest).unwrap(), 5);
        let copy = dest.join("Originals/2026/a.jpg");
        let metadata = std::fs::metadata(&copy).unwrap();
        assert_eq!(std::fs::read(&copy).unwrap(), b"photo");
        assert_eq!(metadata.modified().unwrap(), modified);
        assert!(metadata.permissions().readonly());
        test_support::remove(&root);
    }

    #[test]
    fn leaves_folders_that_arent_the_librarys_alone() {
        let (config_dir, library_dir) = test_support::temp_dirs("not-library");
        let root = config_dir.parent().unwrap().to_path_buf();
        std::fs::create_dir_all(&library_dir).unwrap();
        let handle = LibraryHandle {
            info: LibraryInfo {
                id: "someone-else".into(),
                name: "Photos".into(),
                path: library_dir.clone(),
                read_only: true,
            },
            catalog_path: config_dir.join("libraries/someone-else.db"),
            open: None,
        };
        assert!(handle.delete().is_err());
        assert!(
            handle
                .move_to(&root.join("elsewhere"), &MoveProgress::default())
                .is_err()
        );
        assert!(library_dir.exists());
        test_support::remove(&root);
    }
}

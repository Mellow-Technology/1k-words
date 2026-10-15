//! Photo libraries: folders that photos are imported (copied) into, each
//! with a catalog of the photos in it.
//!
//! A library's folder holds a marker, a README and the originals, filed by
//! the day they were taken:
//!
//! ```text
//! 1K Words/
//!   .1k-words-library
//!   README.txt
//!   Originals/2026/10/15/IMG_0001.JPG
//! ```
//!
//! The originals are read-only by default (see `protect`). Each library's
//! catalog lives in the app's config directory, as `libraries/<id>.db`, and
//! `libraries.json` there lists the libraries.

mod catalog;
mod import;
mod protect;

pub use import::{ImportSession, ImportSummary, Imported, PreparedImport, prepare, source_drive};

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, anyhow, bail};
use gpui_kit::{App, Global, SharedString};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::photos::Photo;
use catalog::Catalog;

/// Marks a folder as a library, and holds its id, so it can be recognised
/// if the app's list of libraries is lost.
const MARKER_FILE: &str = ".1k-words-library";
const ORIGINALS_DIR: &str = "Originals";
const README: &str = "\
This folder is a 1k Words photo library.

The photos in Originals are managed by 1k Words, which keeps a catalog of
them. Please add and remove photos with the app rather than here: files
changed, moved or deleted here won't be reflected in the app.

The photos are read-only so they aren't changed by accident.
";

/// A library as listed in `libraries.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryInfo {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    /// Whether imported files are made read-only.
    #[serde(default = "default_read_only")]
    pub read_only: bool,
}

fn default_read_only() -> bool {
    true
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Marker {
    id: String,
}

/// An open library.
pub struct Library {
    pub info: LibraryInfo,
    catalog: Catalog,
    /// Held while changing the originals, so imports running at the same
    /// time take turns (see `ImportSession`).
    placing: Mutex<()>,
}

impl Library {
    /// Creates a library at `info.path` (which may already exist), or opens
    /// it if it's already a library.
    fn create_or_open(info: LibraryInfo, config_dir: &Path) -> Result<Self> {
        let path = &info.path;
        std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
        let marker_path = path.join(MARKER_FILE);
        if !marker_path.exists() {
            let marker = Marker {
                id: info.id.clone(),
            };
            std::fs::write(&marker_path, serde_json::to_string_pretty(&marker)?)?;
            std::fs::write(path.join("README.txt"), README)?;
        }
        let originals = path.join(ORIGINALS_DIR);
        if !originals.exists() {
            std::fs::create_dir(&originals)?;
            if info.read_only {
                protect::set_writable(&originals, false)?;
            }
        }
        let catalog = Catalog::open(&catalog_path(config_dir, &info.id))?;
        Ok(Self {
            info,
            catalog,
            placing: Mutex::new(()),
        })
    }

    fn placing(&self) -> MutexGuard<'_, ()> {
        self.placing.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The library's photos, oldest first.
    pub fn photos(&self) -> Result<Vec<Photo>> {
        let records = self.catalog.photos()?;
        let file = |relative: &str| self.info.path.join(relative);
        Ok(records
            .into_iter()
            .map(|record| Photo {
                path: file(&record.path),
                raw_pair: record.raw_path.as_deref().map(file),
                live_video: record.live_video_path.as_deref().map(file),
                width: record.width,
                height: record.height,
                meta: record.meta,
            })
            .collect())
    }

    /// Copies the photos among `paths` (files or folders, read recursively)
    /// into the library. Blocks until done. The app imports through a queue
    /// instead (see `ImportSession`).
    #[cfg(test)]
    pub fn import(self: &Arc<Self>, paths: &[PathBuf]) -> Result<ImportSummary> {
        import::import_all(self, crate::photos::images_in(paths))
    }
}

fn catalog_path(config_dir: &Path, id: &str) -> PathBuf {
    config_dir.join("libraries").join(format!("{id}.db"))
}

/// The id in a library folder's marker, if it's a library.
fn marker_id(path: &Path) -> Option<String> {
    let json = std::fs::read_to_string(path.join(MARKER_FILE)).ok()?;
    let marker: Marker = serde_json::from_str(&json).ok()?;
    Some(marker.id).filter(|id| !id.is_empty())
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub(crate) fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// `libraries.json`: the libraries, and which is open.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Registry {
    libraries: Vec<LibraryInfo>,
    current: Option<String>,
}

/// The app's libraries, and the one that's open.
pub struct Libraries {
    config_dir: PathBuf,
    registry: Registry,
    current: Option<Arc<Library>>,
    /// Why no library could be opened, if none is.
    error: Option<SharedString>,
}

impl Global for Libraries {}

impl Libraries {
    fn registry_path(&self) -> PathBuf {
        self.config_dir.join("libraries.json")
    }

    fn save(&self) -> Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        let json = serde_json::to_string_pretty(&self.registry)?;
        std::fs::write(self.registry_path(), json)?;
        Ok(())
    }

    /// Opens the library with `id`, making it the current one.
    fn open(&mut self, id: &str) -> Result<()> {
        let info = self
            .registry
            .libraries
            .iter()
            .find(|info| info.id == id)
            .cloned()
            .ok_or_else(|| anyhow!("no library with id {id}"))?;
        if marker_id(&info.path).is_some_and(|marker| marker != info.id) {
            bail!("{} belongs to a different library", info.path.display());
        }
        let library = Library::create_or_open(info, &self.config_dir)?;
        self.current = Some(Arc::new(library));
        self.registry.current = Some(id.to_owned());
        self.error = None;
        self.save()
    }

    /// Adds the library at `path` (creating it, or adopting an existing
    /// library folder) and opens it.
    fn add(&mut self, path: PathBuf) -> Result<()> {
        if let Some(existing) = self.registry.libraries.iter().find(|i| i.path == path) {
            let id = existing.id.clone();
            return self.open(&id);
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| paths::DEFAULT_LIBRARY_NAME.to_owned());
        let id = marker_id(&path).unwrap_or_else(new_id);
        if self.registry.libraries.iter().any(|info| info.id == id) {
            bail!(
                "{} is a copy of a library that's already listed",
                path.display()
            );
        }
        self.registry.libraries.push(LibraryInfo {
            id: id.clone(),
            name,
            path,
            read_only: true,
        });
        self.open(&id)
    }
}

/// Opens the last library, creating the default one (in the Pictures
/// folder) the first time.
pub fn init(cx: &mut App) {
    let config_dir = paths::config_dir();
    let default_dir = paths::default_library_dir();
    match (config_dir, default_dir) {
        (Some(config_dir), Some(default_dir)) => init_at(config_dir, default_dir, cx),
        _ => cx.set_global(Libraries {
            config_dir: PathBuf::new(),
            registry: Registry::default(),
            current: None,
            error: Some("Couldn't find the home folder.".into()),
        }),
    }
}

/// `init`, with the config directory and default library location given.
pub fn init_at(config_dir: PathBuf, default_dir: PathBuf, cx: &mut App) {
    let mut libraries = Libraries {
        registry: std::fs::read_to_string(config_dir.join("libraries.json"))
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
        config_dir,
        current: None,
        error: None,
    };
    let result = match libraries.registry.current.clone() {
        Some(id) => libraries.open(&id),
        None => match libraries.registry.libraries.first() {
            Some(info) => {
                let id = info.id.clone();
                libraries.open(&id)
            }
            None => libraries.add(default_dir),
        },
    };
    if let Err(error) = result {
        libraries.error = Some(format!("Couldn't open the library: {error:#}").into());
    }
    cx.set_global(libraries);
}

/// The open library, if any.
pub fn current(cx: &App) -> Option<Arc<Library>> {
    cx.try_global::<Libraries>()?.current.clone()
}

/// Why no library is open, if none is.
pub fn error(cx: &App) -> Option<SharedString> {
    cx.try_global::<Libraries>()?.error.clone()
}

/// Every library, for the sidebar.
pub fn list(cx: &App) -> &[LibraryInfo] {
    cx.try_global::<Libraries>()
        .map_or(&[], |libraries| libraries.registry.libraries.as_slice())
}

/// Opens the library with `id`.
pub fn open(id: &str, cx: &mut App) -> Result<()> {
    let libraries = cx.global_mut::<Libraries>();
    if libraries.current.as_ref().is_some_and(|l| l.info.id == id) {
        return Ok(());
    }
    libraries.open(id)
}

/// Creates a library at `path`, or adds the library already there, and
/// opens it.
pub fn add(path: PathBuf, cx: &mut App) -> Result<()> {
    cx.global_mut::<Libraries>().add(path)
}

/// Where the New Library dialog starts: the folder the default library
/// goes in.
pub fn new_library_dir() -> Option<PathBuf> {
    paths::default_library_dir()?
        .parent()
        .map(Path::to_path_buf)
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::path::PathBuf;

    /// A fresh config directory and default library location under a
    /// temporary folder.
    pub fn temp_dirs(name: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("1k-words-{name}-{}", std::process::id()));
        remove(&root);
        (root.join("config"), root.join("Pictures").join("1K Words"))
    }

    /// Removes a temporary folder, read-only library files and all.
    pub fn remove(root: &std::path::Path) {
        if root.exists() {
            super::protect::tests::unlock_all(root);
            std::fs::remove_dir_all(root).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn creates_the_default_library_and_reopens_it(cx: &mut gpui_kit::TestAppContext) {
        let (config_dir, default_dir) = test_support::temp_dirs("libraries");
        cx.update(|cx| init_at(config_dir.clone(), default_dir.clone(), cx));

        let first = cx.update(|cx| current(cx)).expect("a library");
        assert_eq!(first.info.name, "1K Words");
        assert_eq!(first.info.path, default_dir);
        assert!(first.info.read_only);
        assert!(default_dir.join(MARKER_FILE).exists());
        assert!(default_dir.join("README.txt").exists());
        assert!(default_dir.join(ORIGINALS_DIR).is_dir());
        assert!(catalog_path(&config_dir, &first.info.id).exists());

        // A second library becomes the current one.
        let other_dir = default_dir.with_file_name("Family");
        cx.update(|cx| add(other_dir.clone(), cx)).unwrap();
        let second = cx.update(|cx| current(cx)).unwrap();
        assert_eq!(second.info.name, "Family");
        assert_eq!(cx.update(|cx| list(cx).len()), 2);

        // Starting again opens the last library used.
        drop((first, second));
        cx.update(|cx| init_at(config_dir.clone(), default_dir.clone(), cx));
        let reopened = cx.update(|cx| current(cx)).unwrap();
        assert_eq!(reopened.info.path, other_dir);

        // A library folder whose listing was lost is recognised by its marker.
        let id = reopened.info.id.clone();
        drop(reopened);
        std::fs::remove_file(config_dir.join("libraries.json")).unwrap();
        cx.update(|cx| init_at(config_dir.clone(), default_dir.clone(), cx));
        cx.update(|cx| add(other_dir.clone(), cx)).unwrap();
        assert_eq!(cx.update(|cx| current(cx)).unwrap().info.id, id);

        test_support::remove(config_dir.parent().unwrap());
    }
}

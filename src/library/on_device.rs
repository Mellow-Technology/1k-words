//! What's true of a library on this device alone: which of its originals
//! are here and how they got here, when photos were last opened, and what's
//! pinned to stay. A device can't hold every original of a big library, so
//! originals that are safely synced can be removed and fetched again as
//! they're wanted.
//!
//! None of this may reach another device, and the catalog syncs, so it's
//! kept in a database of its own beside the catalog, as
//! `libraries/<id>.device.db`. It's never uploaded, and can be lost: what's
//! here is found again from the disk (see `Library::refresh_originals`),
//! though pins, and which originals were fetched, are forgotten.
//!
//! Files are the library's relative paths, as in the catalog. Originals are
//! recorded one file each, as a photo's raw file can go while its JPEG
//! stays; last opened and pins are recorded by photo (its displayed file),
//! and cover all its files.
//!
//! An original's file is what says it's here: an original nothing has
//! recorded yet, as in a library made before this was, is still here. The
//! records add how it came and its size, without reading the whole disk.
//!
//! Like the catalog, this blocks; call it from a background thread.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context as _, Result, bail};
use futures::executor::block_on;
use turso::{Connection, Database, Value};

use super::albums::{AlbumId, shown_albums};
use super::catalog::{FileRole, PhotoRecord};
use super::id::Id;
use super::{Library, marker_id, now};
use crate::photos::Photo;

/// Bumped with each change to the schema.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
-- The originals known to be on this device: a file in the library each.
CREATE TABLE originals (
    path TEXT PRIMARY KEY,
    size INTEGER NOT NULL,
    -- When it was downloaded (seconds since the Unix epoch); NULL if it was
    -- imported here. Only fetched originals are in the recently fetched
    -- pool.
    fetched_at REAL
);
-- When photos were last opened here, by their displayed file.
CREATE TABLE opened (
    path TEXT PRIMARY KEY,
    opened_at REAL NOT NULL
);
CREATE TABLE pinned_photos (
    path TEXT PRIMARY KEY,
    pinned_at REAL NOT NULL
);
-- A rule rather than a list: whatever the album shows is pinned, so photos
-- added to it later are too.
CREATE TABLE pinned_albums (
    album BLOB PRIMARY KEY,
    pinned_at REAL NOT NULL
);
";

/// How an original came to be on this device.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum OriginalSource {
    Imported,
    /// Downloaded, so it can be removed again unless pinned.
    Fetched,
}

/// An original in the recently fetched pool.
#[derive(Clone, Debug, PartialEq)]
pub struct FetchedOriginal {
    pub path: String,
    pub size: u64,
    pub fetched_at: f64,
    /// When its photo was last opened here, if it has been.
    pub opened_at: Option<f64>,
}

impl FetchedOriginal {
    /// When it was last wanted, which the pool's sorted by.
    fn last_used(&self) -> f64 {
        self.opened_at.unwrap_or(0.0).max(self.fetched_at)
    }
}

/// The space originals take on this device, as of the last records.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OriginalsUsage {
    pub files: usize,
    pub bytes: u64,
    /// Of `bytes`, those of pinned photos. They count toward any budget but
    /// can't be removed to meet it.
    pub pinned_bytes: u64,
}

/// A library's database on this device.
pub(super) struct OnDevice {
    connection: Mutex<Connection>,
    _database: Database,
}

/// Where the database of the library with its catalog at `catalog` is.
fn database_path(catalog: &Path) -> PathBuf {
    catalog.with_extension("device.db")
}

/// Deletes the database of the library with its catalog at `catalog`.
pub(super) fn delete_database(catalog: &Path) -> Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = database_path(catalog).into_os_string();
        file.push(suffix);
        match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).context("deleting the library's device database");
            }
            _ => {}
        }
    }
    Ok(())
}

impl OnDevice {
    /// Opens the database of the library with its catalog at `catalog`,
    /// creating it if needed.
    pub(super) fn open(catalog: &Path) -> Result<Self> {
        let path = database_path(catalog);
        let path_str = path.to_str().context("catalog path isn't valid UTF-8")?;
        block_on(async {
            let database = turso::Builder::new_local(path_str).build().await?;
            let connection = database.connect()?;
            let mut rows = connection.query("PRAGMA user_version", ()).await?;
            let version = match rows.next().await? {
                Some(row) => row.get::<i64>(0)?,
                None => 0,
            };
            drop(rows);
            if version > SCHEMA_VERSION {
                bail!("it's from a newer version of 1k Words (schema {version})");
            }
            if version == 0 {
                connection
                    .execute_batch(&format!(
                        "BEGIN; {SCHEMA} PRAGMA user_version = {SCHEMA_VERSION}; COMMIT;"
                    ))
                    .await?;
            }
            Ok(Self {
                connection: Mutex::new(connection),
                _database: database,
            })
        })
        .with_context(|| format!("opening {}", path.display()))
    }

    fn connection(&self) -> MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records an original as here. One that's recorded keeps how it came
    /// if it's imported again, as when refreshing; a fetch replaces it.
    fn set_original(&self, path: &str, size: u64, fetched_at: Option<f64>) -> Result<()> {
        let connection = self.connection();
        block_on(set_original(&connection, path, size, fetched_at))
    }

    fn remove_original(&self, path: &str) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            connection
                .execute(
                    "DELETE FROM originals WHERE path = ?1",
                    turso::params![path],
                )
                .await?;
            Ok(())
        })
    }

    /// Every original recorded as here, with its size and when it was
    /// fetched, if it was.
    fn originals(&self) -> Result<HashMap<String, (u64, Option<f64>)>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query("SELECT path, size, fetched_at FROM originals", ())
                .await?;
            let mut originals = HashMap::new();
            while let Some(row) = rows.next().await? {
                let fetched_at = match row.get_value(2)? {
                    Value::Real(at) => Some(at),
                    _ => None,
                };
                originals.insert(row.get(0)?, (row.get::<i64>(1)? as u64, fetched_at));
            }
            Ok(originals)
        })
    }

    /// Brings the records in line: `found` are here, `gone` aren't.
    fn update_originals(&self, found: &[(String, u64)], gone: &[String]) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            connection.execute("BEGIN", ()).await?;
            let result = async {
                for (path, size) in found {
                    set_original(&connection, path, *size, None).await?;
                }
                for path in gone {
                    connection
                        .execute(
                            "DELETE FROM originals WHERE path = ?1",
                            turso::params![path.as_str()],
                        )
                        .await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            match result {
                Ok(()) => connection.execute("COMMIT", ()).await?,
                Err(error) => {
                    connection.execute("ROLLBACK", ()).await.ok();
                    return Err(error);
                }
            };
            Ok(())
        })
    }

    fn set_opened(&self, path: &str, at: f64) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            connection
                .execute(
                    "INSERT INTO opened (path, opened_at) VALUES (?1, ?2)
                    ON CONFLICT (path) DO UPDATE SET opened_at = excluded.opened_at",
                    turso::params![path, at],
                )
                .await?;
            Ok(())
        })
    }

    /// When each photo was last opened here.
    fn opened(&self) -> Result<HashMap<String, f64>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query("SELECT path, opened_at FROM opened", ())
                .await?;
            let mut opened = HashMap::new();
            while let Some(row) = rows.next().await? {
                opened.insert(row.get(0)?, row.get(1)?);
            }
            Ok(opened)
        })
    }

    fn set_photo_pinned(&self, path: &str, pinned: bool) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            if pinned {
                connection
                    .execute(
                        "INSERT INTO pinned_photos (path, pinned_at) VALUES (?1, ?2)
                        ON CONFLICT (path) DO NOTHING",
                        turso::params![path, now()],
                    )
                    .await?;
            } else {
                connection
                    .execute(
                        "DELETE FROM pinned_photos WHERE path = ?1",
                        turso::params![path],
                    )
                    .await?;
            }
            Ok(())
        })
    }

    fn pinned_photos(&self) -> Result<HashSet<String>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query("SELECT path FROM pinned_photos", ())
                .await?;
            let mut pinned = HashSet::new();
            while let Some(row) = rows.next().await? {
                pinned.insert(row.get(0)?);
            }
            Ok(pinned)
        })
    }

    fn set_album_pinned(&self, album: AlbumId, pinned: bool) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            if pinned {
                connection
                    .execute(
                        "INSERT INTO pinned_albums (album, pinned_at) VALUES (?1, ?2)
                        ON CONFLICT (album) DO NOTHING",
                        turso::params![album, now()],
                    )
                    .await?;
            } else {
                connection
                    .execute(
                        "DELETE FROM pinned_albums WHERE album = ?1",
                        turso::params![album],
                    )
                    .await?;
            }
            Ok(())
        })
    }

    fn pinned_albums(&self) -> Result<Vec<AlbumId>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query("SELECT album FROM pinned_albums ORDER BY pinned_at", ())
                .await?;
            let mut pinned = Vec::new();
            while let Some(row) = rows.next().await? {
                pinned.push(Id::from_value(row.get_value(0)?)?);
            }
            Ok(pinned)
        })
    }
}

async fn set_original(
    connection: &Connection,
    path: &str,
    size: u64,
    fetched_at: Option<f64>,
) -> Result<()> {
    let fetched = fetched_at.map_or(Value::Null, Value::Real);
    connection
        .execute(
            "INSERT INTO originals (path, size, fetched_at) VALUES (?1, ?2, ?3)
            ON CONFLICT (path) DO UPDATE SET
                size = excluded.size,
                fetched_at = coalesce(excluded.fetched_at, originals.fetched_at)",
            turso::params![path, size as i64, fetched],
        )
        .await?;
    Ok(())
}

/// A photo's originals.
fn files_of(record: &PhotoRecord) -> impl Iterator<Item = &String> {
    std::iter::once(&record.path)
        .chain(&record.raw_path)
        .chain(&record.live_video_path)
}

impl Library {
    /// Brings the records of what's here in line with the disk: originals
    /// the catalog has that are here but unrecorded (which count as
    /// imported), and ones recorded that have gone. Reads every original's
    /// metadata; run it now and then, not on every change.
    ///
    /// Fails, recording nothing, if the library's folder isn't there, so a
    /// disk that's been unplugged doesn't look like originals removed.
    pub fn refresh_originals(&self) -> Result<()> {
        self.check_folder()?;
        let known = self.on_device.originals()?;
        let mut wanted = HashSet::new();
        let (mut found, mut gone, mut here) = (Vec::new(), Vec::new(), 0);
        for file in self.catalog.files()? {
            if file.role != FileRole::Original {
                continue;
            }
            let recorded = known.get(&file.path);
            match std::fs::metadata(self.info.path.join(&file.path)) {
                Ok(metadata) => {
                    here += 1;
                    if recorded.is_none_or(|(size, _)| *size != metadata.len()) {
                        found.push((file.path.clone(), metadata.len()));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if recorded.is_some() {
                        gone.push(file.path.clone());
                    }
                }
                // Can't tell, so it stays as it is.
                Err(error) => eprintln!("couldn't read {}: {error}", file.path),
            }
            wanted.insert(file.path);
        }
        // Photos deleted from the library.
        gone.extend(known.keys().filter(|path| !wanted.contains(*path)).cloned());
        // It may have gone part way through, or been replaced by an empty
        // folder of the same name.
        self.check_folder()?;
        if here == 0 && !known.is_empty() {
            bail!("none of the originals are in {}", self.info.path.display());
        }
        self.on_device.update_originals(&found, &gone)
    }

    fn check_folder(&self) -> Result<()> {
        if marker_id(&self.info.path).as_deref() != Some(self.info.id.as_str()) {
            bail!("{} isn't there", self.info.path.display());
        }
        Ok(())
    }

    /// Records originals just imported, at these paths. They're in the
    /// catalog by now, so failing here doesn't fail the import: the next
    /// refresh finds them.
    pub(super) fn note_imported(&self, files: &[PathBuf]) {
        for file in files {
            let noted = self
                .relative(file)
                .and_then(|path| self.set_on_device(&path, OriginalSource::Imported));
            if let Err(error) = noted {
                eprintln!("couldn't record {}: {error:#}", file.display());
            }
        }
    }

    /// Records that a photo's been opened here.
    pub fn touch(&self, photo: &Photo) -> Result<()> {
        self.on_device
            .set_opened(&self.relative(&photo.path)?, now())
    }
}

/// For what's built on this: fetching originals, and removing them.
#[allow(dead_code)]
impl Library {
    /// Whether the file the photo displays is on this device. Goes by the
    /// disk, so it needs no records, and is right for any photo.
    pub fn is_on_device(&self, photo: &Photo) -> bool {
        photo.path.is_file()
    }

    /// The originals the catalog has that aren't recorded as here, oldest
    /// photos first. Only as good as the records: call `refresh_originals`
    /// first if the disk may have changed.
    pub fn originals_not_here(&self) -> Result<Vec<String>> {
        let known = self.on_device.originals()?;
        Ok(self
            .catalog
            .files()?
            .into_iter()
            .filter(|file| file.role == FileRole::Original && !known.contains_key(&file.path))
            .map(|file| file.path)
            .collect())
    }

    /// Records that the original at `path` (in the library) has just
    /// arrived, once it's in place.
    pub fn set_on_device(&self, path: &str, source: OriginalSource) -> Result<()> {
        let file = self.info.path.join(path);
        let size = std::fs::metadata(&file)
            .with_context(|| format!("reading {path}"))?
            .len();
        let fetched_at = (source == OriginalSource::Fetched).then(now);
        self.on_device.set_original(path, size, fetched_at)
    }

    /// Records that the original at `path` (in the library) has been
    /// removed from this device. This doesn't remove it.
    pub fn set_evicted(&self, path: &str) -> Result<()> {
        self.on_device.remove_original(path)
    }

    /// When each photo (by its displayed file) was last opened here.
    pub fn last_opened(&self) -> Result<HashMap<String, f64>> {
        self.on_device.opened()
    }

    /// The recently fetched pool: originals that were fetched and aren't
    /// pinned, the one wanted longest ago first. Originals that were
    /// imported never join it, and a fetched one rejoins it when unpinned.
    pub fn fetched_originals(&self) -> Result<Vec<FetchedOriginal>> {
        let pinned = self.pinned_originals()?;
        let opened = self.on_device.opened()?;
        let mut photo_of = HashMap::new();
        for record in self.catalog.photos()? {
            for file in files_of(&record) {
                photo_of.insert(file.clone(), record.path.clone());
            }
        }
        let mut pool: Vec<FetchedOriginal> = self
            .on_device
            .originals()?
            .into_iter()
            .filter(|(path, _)| !pinned.contains(path))
            .filter_map(|(path, (size, fetched_at))| {
                let opened_at = photo_of.get(&path).and_then(|photo| opened.get(photo));
                Some(FetchedOriginal {
                    opened_at: opened_at.copied(),
                    fetched_at: fetched_at?,
                    size,
                    path,
                })
            })
            .collect();
        pool.sort_by(|a, b| {
            a.last_used()
                .total_cmp(&b.last_used())
                .then_with(|| a.path.cmp(&b.path))
        });
        Ok(pool)
    }

    pub fn pin_photo(&self, photo: &Photo) -> Result<()> {
        let path = self.relative(&photo.path)?;
        self.on_device.set_photo_pinned(&path, true)
    }

    /// Takes the photo's own pin off. It stays pinned if a pinned album
    /// shows it.
    pub fn unpin_photo(&self, photo: &Photo) -> Result<()> {
        let path = self.relative(&photo.path)?;
        self.on_device.set_photo_pinned(&path, false)
    }

    /// Pins whatever the album shows, now and as it changes (see
    /// `album_photos`).
    pub fn pin_album(&self, album: AlbumId) -> Result<()> {
        self.on_device.set_album_pinned(album, true)
    }

    pub fn unpin_album(&self, album: AlbumId) -> Result<()> {
        self.on_device.set_album_pinned(album, false)
    }

    pub fn pinned_albums(&self) -> Result<Vec<AlbumId>> {
        self.on_device.pinned_albums()
    }

    /// Whether the photo is pinned, itself or by an album.
    pub fn is_pinned(&self, photo: &Photo) -> Result<bool> {
        Ok(self
            .pinned_originals()?
            .contains(&self.relative(&photo.path)?))
    }

    /// Every original of every pinned photo, whether or not it's here.
    /// These must not be removed, and should be kept downloaded. A deleted
    /// album pins nothing, though its pin comes back with it if it's
    /// restored.
    pub fn pinned_originals(&self) -> Result<HashSet<String>> {
        let photos = self.on_device.pinned_photos()?;
        let mut pinned = HashSet::new();
        if !photos.is_empty() {
            for record in self.catalog.photos()? {
                if photos.contains(&record.path) {
                    pinned.extend(files_of(&record).cloned());
                }
            }
        }
        let album_pins = self.on_device.pinned_albums()?;
        if !album_pins.is_empty() {
            let albums = self.catalog.albums()?;
            for album in album_pins {
                if albums.iter().any(|known| known.id == album) {
                    let shown = shown_albums(&albums, album);
                    for record in self.catalog.album_photos(&shown)? {
                        pinned.extend(files_of(&record).cloned());
                    }
                }
            }
        }
        Ok(pinned)
    }

    /// The space originals take here.
    pub fn originals_usage(&self) -> Result<OriginalsUsage> {
        let pinned = self.pinned_originals()?;
        let mut usage = OriginalsUsage::default();
        for (path, (size, _)) in self.on_device.originals()? {
            usage.files += 1;
            usage.bytes += size;
            if pinned.contains(&path) {
                usage.pinned_bytes += size;
            }
        }
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{LibraryHandle, LibraryInfo, catalog_path, protect, test_support};
    use super::*;

    /// A library of `count` photos, with its temporary folder.
    fn library(name: &str, count: u8) -> (Arc<Library>, PathBuf) {
        let (config_dir, library_dir) = test_support::temp_dirs(name);
        let root = config_dir.parent().unwrap().to_path_buf();
        let source = root.join("card");
        std::fs::create_dir_all(&source).unwrap();
        for i in 0..count {
            image::RgbImage::from_pixel(16, 16, image::Rgb([i * 10, 90, 160]))
                .save(source.join(format!("IMG_{i:04}.JPG")))
                .unwrap();
        }
        let library = Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: name.into(),
                    name: "Test".into(),
                    path: library_dir,
                    read_only: true,
                },
                &config_dir,
            )
            .unwrap(),
        );
        assert_eq!(library.import(&[source]).unwrap().imported, count as usize);
        (library, root)
    }

    fn relative(library: &Library, photo: &Photo) -> String {
        library.relative(&photo.path).unwrap()
    }

    /// Takes a photo's file off the disk, as removing it would.
    fn remove_file(photo: &Photo) {
        let dir = photo.path.parent().unwrap();
        protect::set_writable(dir, true).unwrap();
        std::fs::remove_file(&photo.path).unwrap();
        protect::set_writable(dir, false).unwrap();
    }

    #[test]
    fn imports_are_recorded_and_stay_out_of_the_pool() {
        let (library, root) = library("ondevimport", 2);
        let usage = library.originals_usage().unwrap();
        assert_eq!(usage.files, 2);
        let photos = library.photos().unwrap();
        let bytes: u64 = photos
            .iter()
            .map(|photo| std::fs::metadata(&photo.path).unwrap().len())
            .sum();
        assert_eq!(usage.bytes, bytes);
        assert_eq!(usage.pinned_bytes, 0);
        assert!(library.originals_not_here().unwrap().is_empty());
        assert!(library.fetched_originals().unwrap().is_empty());
        test_support::remove(&root);
    }

    #[test]
    fn refreshing_finds_what_the_records_lack() {
        let (library, root) = library("ondevrefresh", 3);
        let photos = library.photos().unwrap();
        let paths: Vec<String> = photos.iter().map(|p| relative(&library, p)).collect();

        // As in a library made before there were records: the files are
        // here, whatever's recorded.
        for path in &paths {
            library.set_evicted(path).unwrap();
        }
        assert!(library.is_on_device(&photos[0]));
        assert_eq!(library.originals_not_here().unwrap(), paths);
        library.refresh_originals().unwrap();
        assert!(library.originals_not_here().unwrap().is_empty());
        assert_eq!(library.originals_usage().unwrap().files, 3);
        // They count as imported, so nothing would remove them on its own.
        assert!(library.fetched_originals().unwrap().is_empty());

        // A file that's gone is noticed.
        remove_file(&photos[1]);
        assert!(!library.is_on_device(&photos[1]));
        library.refresh_originals().unwrap();
        assert_eq!(library.originals_not_here().unwrap(), [paths[1].clone()]);
        assert_eq!(library.originals_usage().unwrap().files, 2);
        test_support::remove(&root);
    }

    #[test]
    fn refreshing_keeps_how_an_original_came() {
        let (library, root) = library("ondevkeep", 1);
        let photo = &library.photos().unwrap()[0];
        let path = relative(&library, photo);
        library
            .set_on_device(&path, OriginalSource::Fetched)
            .unwrap();
        let fetched = library.fetched_originals().unwrap();
        assert_eq!(fetched.len(), 1);

        // Even if its size changes, which rewrites its record.
        protect::set_writable(&photo.path, true).unwrap();
        std::fs::write(&photo.path, b"longer or shorter").unwrap();
        protect::set_writable(&photo.path, false).unwrap();
        library.refresh_originals().unwrap();
        let after = library.fetched_originals().unwrap();
        assert_eq!(after[0].fetched_at, fetched[0].fetched_at);
        assert_eq!(after[0].size, 17);
        test_support::remove(&root);
    }

    #[test]
    fn a_missing_disk_doesnt_look_like_removed_originals() {
        let (library, root) = library("ondevdisk", 2);
        let folder = library.info.path.clone();
        let away = folder.with_file_name("Away");

        // Gone altogether.
        protect::set_writable(&folder, true).unwrap();
        std::fs::rename(&folder, &away).unwrap();
        assert!(library.refresh_originals().is_err());
        assert_eq!(library.originals_usage().unwrap().files, 2);

        // Or with an empty library in its place, as opening one makes.
        std::fs::create_dir_all(folder.join("Originals")).unwrap();
        std::fs::copy(away.join(super::super::MARKER_FILE), {
            folder.join(super::super::MARKER_FILE)
        })
        .unwrap();
        assert!(library.refresh_originals().is_err());
        assert_eq!(library.originals_usage().unwrap().files, 2);
        test_support::remove(&root);
    }

    #[test]
    fn the_pool_goes_by_when_photos_were_last_wanted() {
        let (library, root) = library("ondevpool", 3);
        let photos = library.photos().unwrap();
        let paths: Vec<String> = photos.iter().map(|p| relative(&library, p)).collect();
        for path in &paths {
            library
                .set_on_device(path, OriginalSource::Fetched)
                .unwrap();
        }
        let pool_paths = || -> Vec<String> {
            let pool = library.fetched_originals().unwrap();
            pool.into_iter().map(|original| original.path).collect()
        };
        assert_eq!(pool_paths(), paths);

        // Opening one puts it last.
        library.touch(&photos[0]).unwrap();
        assert_eq!(
            pool_paths(),
            [paths[1].clone(), paths[2].clone(), paths[0].clone()]
        );
        assert!(library.last_opened().unwrap().contains_key(&paths[0]));

        // Pinning takes it out, and unpinning puts it back.
        library.pin_photo(&photos[1]).unwrap();
        assert!(library.is_pinned(&photos[1]).unwrap());
        assert!(!pool_paths().contains(&paths[1]));
        library.unpin_photo(&photos[1]).unwrap();
        assert!(!library.is_pinned(&photos[1]).unwrap());
        assert!(pool_paths().contains(&paths[1]));

        // Once it's removed, it's out of the pool, and not here.
        library.set_evicted(&paths[2]).unwrap();
        assert!(!pool_paths().contains(&paths[2]));
        assert_eq!(library.originals_not_here().unwrap(), [paths[2].clone()]);
        test_support::remove(&root);
    }

    #[test]
    fn an_album_pin_is_a_live_rule() {
        let (library, root) = library("ondevalbum", 4);
        let photos = library.photos().unwrap();
        let path = |ix: usize| relative(&library, &photos[ix]);
        let album = library.create_album("Trip", None).unwrap();
        let nested = library.create_album("Day 1", Some(album)).unwrap();
        library.add_to_album(album, &photos[..1]).unwrap();
        library.add_to_album(nested, &photos[3..]).unwrap();

        library.pin_album(album).unwrap();
        assert_eq!(library.pinned_albums().unwrap(), [album]);
        assert!(library.is_pinned(&photos[0]).unwrap());
        // It's the album's own photos, as it doesn't show the nested ones.
        assert!(!library.is_pinned(&photos[3]).unwrap());

        // Photos added later are pinned too, and ones taken out aren't.
        library.add_to_album(album, &photos[1..2]).unwrap();
        assert!(library.is_pinned(&photos[1]).unwrap());
        library.remove_from_album(album, &photos[..1]).unwrap();
        assert!(!library.is_pinned(&photos[0]).unwrap());

        // It follows what the album shows.
        library.set_album_show_nested(album, true).unwrap();
        assert!(library.is_pinned(&photos[3]).unwrap());

        // A photo's own pin outlives its album's, and counts in the usage.
        library.pin_photo(&photos[2]).unwrap();
        library.add_to_album(album, &photos[2..3]).unwrap();
        library.unpin_album(album).unwrap();
        assert_eq!(
            library.pinned_originals().unwrap(),
            HashSet::from([path(2)])
        );
        let usage = library.originals_usage().unwrap();
        assert_eq!(
            usage.pinned_bytes,
            std::fs::metadata(&photos[2].path).unwrap().len()
        );

        // A deleted album pins nothing.
        library.pin_album(album).unwrap();
        library.delete_album(nested).unwrap();
        library.delete_album(album).unwrap();
        assert_eq!(
            library.pinned_originals().unwrap(),
            HashSet::from([path(2)])
        );
        test_support::remove(&root);
    }

    #[test]
    fn its_kept_apart_from_the_catalog_and_deleted_with_the_library() {
        let (library, root) = library("ondevdelete", 1);
        let config_dir = root.join("config");
        let catalog = catalog_path(&config_dir, &library.info.id);
        let database = database_path(&catalog);
        assert_ne!(catalog, database);
        assert!(database.is_file());

        // It lasts.
        let photo = library.photos().unwrap()[0].clone();
        library.pin_photo(&photo).unwrap();
        let info = library.info.clone();
        drop(library);
        let library = Arc::new(Library::create_or_open(info.clone(), &config_dir).unwrap());
        assert!(library.is_pinned(&photo).unwrap());

        drop(library);
        LibraryHandle {
            info,
            catalog_path: catalog,
            open: None,
        }
        .delete()
        .unwrap();
        assert!(!database.exists());
        test_support::remove(&root);
    }
}

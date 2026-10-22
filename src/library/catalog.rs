//! A library's catalog: a Turso database of its photos' files and metadata,
//! the derivatives made from them (see `derivatives`), the objects found in
//! them (see `objects`), the albums they're in (see `albums`), and what's
//! been synced where.
//!
//! Files are stored relative to the library folder, with `/` separators, so
//! a library can move. The metadata is what `photos::read_shots` extracts,
//! stored in columns for fast filtering. All of it can be read again from
//! the originals, so a lost catalog could be rebuilt.
//!
//! Turso's API is async but does its I/O synchronously, so each call blocks
//! on it; call these from a background thread.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use futures::executor::block_on;
use turso::{Connection, Database, Row, Value};

use super::albums::Album;
use super::derivatives::DerivativeKind;
use super::objects::ObjectKind;
use crate::detect::DetectedObject;
use crate::location::Location;
use crate::photos::CaptureMeta;

/// Bumped with each change to the schema, which `migrate` brings old
/// catalogs up to.
const SCHEMA_VERSION: i64 = 5;

const SCHEMA_V1: &str = "
CREATE TABLE photos (
    id INTEGER PRIMARY KEY,
    -- The displayed file, its raw pair (RAW+JPEG) and Live Photo video.
    path TEXT NOT NULL UNIQUE,
    raw_path TEXT,
    live_video_path TEXT,
    -- BLAKE3 of the displayed file, to skip duplicates when importing.
    content_hash TEXT NOT NULL,
    file_size INTEGER NOT NULL,
    -- Where it was imported from, and when (seconds since the Unix epoch).
    original_path TEXT NOT NULL,
    imported_at REAL NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    -- Seconds by the camera's clock, as if it were UTC.
    captured_at REAL,
    modified_at REAL NOT NULL,
    camera TEXT,
    exposure_time REAL,
    f_number REAL,
    iso INTEGER,
    exposure_bias REAL,
    auto_bracket INTEGER NOT NULL DEFAULT 0,
    manual_exposure INTEGER NOT NULL DEFAULT 0,
    latitude REAL,
    longitude REAL
);
CREATE INDEX photos_content_hash ON photos (content_hash);
CREATE INDEX photos_captured_at ON photos (captured_at);
CREATE INDEX photos_camera ON photos (camera);
";

/// Syncing (see `library::sync`). The catalog is the canonical record of
/// what's on each target: services needn't be able to list what they hold.
const SCHEMA_V2: &str = "
-- The targets the library syncs to. They're listed, with their settings,
-- in the app's `sync-targets.json`.
CREATE TABLE sync_targets (
    id INTEGER PRIMARY KEY,
    -- The target's id in `sync-targets.json`.
    target TEXT NOT NULL UNIQUE,
    -- The library's folder in the target, so libraries sharing it don't mix.
    folder TEXT NOT NULL,
    created_at REAL NOT NULL,
    -- Why syncing stopped (such as bad credentials), until it next works.
    error TEXT
);
CREATE TABLE sync_items (
    id INTEGER PRIMARY KEY,
    -- The row in sync_targets.
    target_id INTEGER NOT NULL,
    -- The file in the library. NULL for files only on the service.
    path TEXT,
    -- 'synced'; 'failed', to try again; or 'rejected' by the service, not
    -- retried.
    state TEXT NOT NULL,
    -- How the service identifies the file, and its version there.
    remote_key TEXT,
    remote_version TEXT,
    size INTEGER,
    attempts INTEGER NOT NULL DEFAULT 0,
    error TEXT,
    synced_at REAL,
    updated_at REAL NOT NULL,
    UNIQUE (target_id, path)
);
CREATE INDEX sync_items_remote_key ON sync_items (target_id, remote_key);
";

/// Derivatives (see `library::derivatives`): files made from a photo's
/// originals, such as its thumbnail. The originals are the photos table's
/// files; these can be made again from them.
const SCHEMA_V3: &str = "
CREATE TABLE derivatives (
    id INTEGER PRIMARY KEY,
    -- The row in photos.
    photo_id INTEGER NOT NULL,
    -- 'thumbnail'.
    kind TEXT NOT NULL,
    path TEXT NOT NULL UNIQUE,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    file_size INTEGER NOT NULL,
    -- How it was made (see `DerivativeKind::version`).
    version INTEGER NOT NULL,
    created_at REAL NOT NULL,
    UNIQUE (photo_id, kind)
);
";

/// Object detection (see `library::objects`): what was found in each photo.
/// Like the metadata, it can be found again from the originals.
const SCHEMA_V4: &str = "
-- The photos that have been looked at, and by which model, so photos where
-- nothing was found aren't looked at again.
CREATE TABLE object_scans (
    -- The row in photos.
    photo_id INTEGER PRIMARY KEY,
    -- See `detect::MODEL`.
    model TEXT NOT NULL,
    scanned_at REAL NOT NULL
);
CREATE TABLE objects (
    id INTEGER PRIMARY KEY,
    -- The row in photos.
    photo_id INTEGER NOT NULL,
    -- The model's word for it, e.g. 'person' or 'dog'.
    label TEXT NOT NULL,
    confidence REAL NOT NULL,
    -- Its bounds, as fractions of the photo's width and height.
    x1 REAL NOT NULL,
    y1 REAL NOT NULL,
    x2 REAL NOT NULL,
    y2 REAL NOT NULL
);
CREATE INDEX objects_photo_id ON objects (photo_id);
CREATE INDEX objects_label ON objects (label);
";

/// Albums (see `library::albums`), which hold photos and other albums.
/// Unlike everything else here, they can't be found again from the
/// originals.
const SCHEMA_V5: &str = "
CREATE TABLE albums (
    id INTEGER PRIMARY KEY,
    -- The album it's in; NULL at the top level.
    parent_id INTEGER,
    name TEXT NOT NULL,
    created_at REAL NOT NULL
);
CREATE INDEX albums_parent_id ON albums (parent_id);
CREATE TABLE album_photos (
    id INTEGER PRIMARY KEY,
    -- The rows in albums and photos.
    album_id INTEGER NOT NULL,
    photo_id INTEGER NOT NULL,
    added_at REAL NOT NULL,
    UNIQUE (album_id, photo_id)
);
CREATE INDEX album_photos_photo_id ON album_photos (photo_id);
";

/// The columns `record_from_row` reads, from `photos`.
const PHOTO_COLUMNS: &str = "photos.path, photos.raw_path, photos.live_video_path,
    photos.content_hash, photos.file_size, photos.original_path, photos.imported_at,
    photos.width, photos.height, photos.captured_at, photos.modified_at, photos.camera,
    photos.exposure_time, photos.f_number, photos.iso, photos.exposure_bias,
    photos.auto_bracket, photos.manual_exposure, photos.latitude, photos.longitude";

/// A photo as the catalog stores it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PhotoRecord {
    pub path: String,
    pub raw_path: Option<String>,
    pub live_video_path: Option<String>,
    pub content_hash: String,
    pub file_size: u64,
    pub original_path: String,
    pub imported_at: f64,
    pub width: u32,
    pub height: u32,
    pub meta: CaptureMeta,
    pub location: Option<Location>,
}

/// A derivative of a photo, as the catalog stores it.
#[derive(Clone, Debug, PartialEq)]
pub struct DerivativeRecord {
    /// The photo's displayed file.
    pub photo_path: String,
    pub kind: DerivativeKind,
    pub path: String,
    pub width: u32,
    pub height: u32,
    pub file_size: u64,
    pub version: u32,
}

/// What a file in the library is to its photo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileRole {
    /// The displayed file, raw pair or Live Photo video, as imported.
    Original,
    Derivative(DerivativeKind),
}

/// A file in the library.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryFile {
    pub path: String,
    pub role: FileRole,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SyncTargetRecord {
    pub id: i64,
    /// The target's id in `sync-targets.json`.
    pub target: String,
    pub folder: String,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncState {
    Synced,
    Failed,
    Rejected,
}

impl SyncState {
    fn key(self) -> &'static str {
        match self {
            SyncState::Synced => "synced",
            SyncState::Failed => "failed",
            SyncState::Rejected => "rejected",
        }
    }

    fn from_key(key: &str) -> Option<Self> {
        [SyncState::Synced, SyncState::Failed, SyncState::Rejected]
            .into_iter()
            .find(|state| state.key() == key)
    }
}

/// A library file's sync to a target.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncItemRecord {
    pub path: String,
    pub state: SyncState,
    pub remote_key: Option<String>,
    pub remote_version: Option<String>,
    pub size: Option<u64>,
    pub attempts: u32,
    pub error: Option<String>,
}

pub struct Catalog {
    connection: Mutex<Connection>,
    _database: Database,
}

impl Catalog {
    /// Opens the catalog at `path`, creating it if needed.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let path_str = path.to_str().context("catalog path isn't valid UTF-8")?;
        block_on(async {
            let database = turso::Builder::new_local(path_str).build().await?;
            let connection = database.connect()?;
            migrate(&connection).await?;
            Ok::<_, anyhow::Error>(Self {
                connection: Mutex::new(connection),
                _database: database,
            })
        })
        .with_context(|| format!("opening catalog {}", path.display()))
    }

    fn connection(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.connection.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Every photo, oldest first.
    pub fn photos(&self) -> Result<Vec<PhotoRecord>> {
        self.query_photos(
            &format!(
                "SELECT {PHOTO_COLUMNS} FROM photos
                ORDER BY coalesce(photos.captured_at, photos.modified_at), photos.path"
            ),
            (),
        )
    }

    /// The photos in an album (not the albums in it), oldest first.
    pub fn album_photos(&self, album: i64) -> Result<Vec<PhotoRecord>> {
        self.query_photos(
            &format!(
                "SELECT {PHOTO_COLUMNS} FROM photos
                JOIN album_photos ON album_photos.photo_id = photos.id
                WHERE album_photos.album_id = ?1
                ORDER BY coalesce(photos.captured_at, photos.modified_at), photos.path"
            ),
            turso::params![album],
        )
    }

    fn query_photos(&self, sql: &str, params: impl turso::IntoParams) -> Result<Vec<PhotoRecord>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection.query(sql, params).await?;
            let mut photos = Vec::new();
            while let Some(row) = rows.next().await? {
                photos.push(record_from_row(&row)?);
            }
            Ok(photos)
        })
    }

    /// Every album, by name.
    pub fn albums(&self) -> Result<Vec<Album>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT albums.id, albums.parent_id, albums.name,
                        (SELECT count(*) FROM album_photos
                            WHERE album_photos.album_id = albums.id)
                    FROM albums
                    ORDER BY albums.name COLLATE NOCASE, albums.id",
                    (),
                )
                .await?;
            let mut albums = Vec::new();
            while let Some(row) = rows.next().await? {
                albums.push(Album {
                    id: row.get(0)?,
                    parent: match row.get_value(1)? {
                        Value::Integer(parent) => Some(parent),
                        _ => None,
                    },
                    name: row.get(2)?,
                    photos: row.get::<i64>(3)? as usize,
                });
            }
            Ok(albums)
        })
    }

    /// Makes an album, in `parent` if given, returning its id.
    pub fn create_album(&self, name: &str, parent: Option<i64>) -> Result<i64> {
        let connection = self.connection();
        block_on(async {
            if let Some(parent) = parent {
                let mut rows = connection
                    .query("SELECT 1 FROM albums WHERE id = ?1", turso::params![parent])
                    .await?;
                if rows.next().await?.is_none() {
                    bail!("there's no album {parent} to put it in");
                }
            }
            connection
                .execute(
                    "INSERT INTO albums (parent_id, name, created_at) VALUES (?1, ?2, ?3)",
                    turso::params![parent, name, super::now()],
                )
                .await?;
            Ok(connection.last_insert_rowid())
        })
    }

    pub fn rename_album(&self, album: i64, name: &str) -> Result<()> {
        let connection = self.connection();
        block_on(connection.execute(
            "UPDATE albums SET name = ?2 WHERE id = ?1",
            turso::params![album, name],
        ))?;
        Ok(())
    }

    /// Deletes albums, given with every album in them. Their photos stay in
    /// the library.
    pub fn delete_albums(&self, albums: &[i64]) -> Result<()> {
        if albums.is_empty() {
            return Ok(());
        }
        let ids = albums
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        let connection = self.connection();
        block_on(connection.execute_batch(&format!(
            "BEGIN;
            DELETE FROM album_photos WHERE album_id IN ({ids});
            DELETE FROM albums WHERE id IN ({ids});
            COMMIT;"
        )))?;
        Ok(())
    }

    /// Adds photos, by their displayed files, to an album, returning how
    /// many weren't in it already.
    pub fn add_paths_to_album(&self, album: i64, paths: &[String]) -> Result<usize> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query("SELECT 1 FROM albums WHERE id = ?1", turso::params![album])
                .await?;
            if rows.next().await?.is_none() {
                bail!("the album isn't there any more");
            }
            drop(rows);
            let mut added = 0;
            for path in paths {
                let mut rows = connection
                    .query(
                        "SELECT id FROM photos WHERE path = ?1",
                        turso::params![path.as_str()],
                    )
                    .await?;
                let photo: i64 = match rows.next().await? {
                    Some(row) => row.get(0)?,
                    None => bail!("{path} isn't in the catalog"),
                };
                drop(rows);
                if insert_album_photo(&connection, album, photo).await? {
                    added += 1;
                }
            }
            Ok(added)
        })
    }

    /// Takes photos, by their displayed files, out of an album (not out of
    /// the library), returning how many were in it.
    pub fn remove_paths_from_album(&self, album: i64, paths: &[String]) -> Result<usize> {
        let connection = self.connection();
        block_on(async {
            let mut removed = 0;
            for path in paths {
                removed += connection
                    .execute(
                        "DELETE FROM album_photos WHERE album_id = ?1
                            AND photo_id IN (SELECT id FROM photos WHERE path = ?2)",
                        turso::params![album, path.as_str()],
                    )
                    .await? as usize;
            }
            Ok(removed)
        })
    }

    /// The albums a photo, by its displayed file, is in.
    pub fn albums_with(&self, path: &str) -> Result<Vec<i64>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT album_photos.album_id FROM album_photos
                    JOIN photos ON photos.id = album_photos.photo_id
                    WHERE photos.path = ?1
                    ORDER BY album_photos.album_id",
                    turso::params![path],
                )
                .await?;
            let mut albums = Vec::new();
            while let Some(row) = rows.next().await? {
                albums.push(row.get(0)?);
            }
            Ok(albums)
        })
    }

    /// Adds the photo whose displayed file has this content to an album,
    /// unless it's there already. Returns whether there was such a photo.
    pub fn add_to_album(&self, album: i64, content_hash: &str, file_size: u64) -> Result<bool> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT id FROM photos WHERE content_hash = ?1 AND file_size = ?2 LIMIT 1",
                    turso::params![content_hash, file_size as i64],
                )
                .await?;
            let photo: i64 = match rows.next().await? {
                Some(row) => row.get(0)?,
                None => return Ok(false),
            };
            drop(rows);
            insert_album_photo(&connection, album, photo).await?;
            Ok(true)
        })
    }

    /// How many photos there are (counting a raw pair or Live Photo as one).
    pub fn photo_count(&self) -> Result<usize> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection.query("SELECT count(*) FROM photos", ()).await?;
            let row = rows.next().await?.context("no count")?;
            Ok(row.get::<i64>(0)? as usize)
        })
    }

    /// Whether a photo with this content is already in the catalog.
    pub fn contains(&self, content_hash: &str, file_size: u64) -> Result<bool> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT 1 FROM photos WHERE content_hash = ?1 AND file_size = ?2 LIMIT 1",
                    turso::params![content_hash, file_size as i64],
                )
                .await?;
            Ok(rows.next().await?.is_some())
        })
    }

    pub fn insert(&self, record: &PhotoRecord) -> Result<()> {
        let meta = &record.meta;
        let connection = self.connection();
        block_on(connection.execute(
            "INSERT INTO photos (
                path, raw_path, live_video_path, content_hash, file_size,
                original_path, imported_at, width, height, captured_at,
                modified_at, camera, exposure_time, f_number, iso,
                exposure_bias, auto_bracket, manual_exposure, latitude,
                longitude
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19, ?20)",
            turso::params![
                record.path.as_str(),
                record.raw_path.as_deref(),
                record.live_video_path.as_deref(),
                record.content_hash.as_str(),
                record.file_size as i64,
                record.original_path.as_str(),
                record.imported_at,
                record.width as i64,
                record.height as i64,
                meta.captured_at,
                meta.modified_at,
                meta.camera.as_deref(),
                meta.exposure_time,
                meta.f_number,
                meta.iso.map(i64::from),
                meta.exposure_bias,
                meta.auto_bracket as i64,
                meta.manual_exposure as i64,
                record.location.map(|l| l.latitude),
                record.location.map(|l| l.longitude),
            ],
        ))?;
        Ok(())
    }

    /// Every file in the library, originals and derivatives, in the order
    /// their photos were taken.
    pub fn files(&self) -> Result<Vec<LibraryFile>> {
        Ok(self.photo_files()?.into_iter().flatten().collect())
    }

    /// Each photo's files, in the order they were taken: its originals
    /// (displayed, raw and video), then its derivatives.
    pub fn photo_files(&self) -> Result<Vec<Vec<LibraryFile>>> {
        let mut derivatives: HashMap<i64, Vec<LibraryFile>> = HashMap::new();
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT photo_id, kind, path FROM derivatives ORDER BY id",
                    (),
                )
                .await?;
            while let Some(row) = rows.next().await? {
                let kind: String = row.get(1)?;
                let Some(kind) = DerivativeKind::from_key(&kind) else {
                    // From a newer version of the app.
                    continue;
                };
                derivatives
                    .entry(row.get(0)?)
                    .or_default()
                    .push(LibraryFile {
                        path: row.get(2)?,
                        role: FileRole::Derivative(kind),
                    });
            }
            let mut rows = connection
                .query(
                    "SELECT id, path, raw_path, live_video_path FROM photos
                    ORDER BY coalesce(captured_at, modified_at), path",
                    (),
                )
                .await?;
            let mut photos = Vec::new();
            while let Some(row) = rows.next().await? {
                let mut files = Vec::new();
                for ix in 1..4 {
                    if let Value::Text(path) = row.get_value(ix)? {
                        files.push(LibraryFile {
                            path,
                            role: FileRole::Original,
                        });
                    }
                }
                files.extend(derivatives.remove(&row.get::<i64>(0)?).unwrap_or_default());
                photos.push(files);
            }
            Ok(photos)
        })
    }

    /// Every derivative.
    pub fn derivatives(&self) -> Result<Vec<DerivativeRecord>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT photos.path, derivatives.kind, derivatives.path,
                        derivatives.width, derivatives.height,
                        derivatives.file_size, derivatives.version
                    FROM derivatives JOIN photos ON photos.id = derivatives.photo_id
                    ORDER BY coalesce(photos.captured_at, photos.modified_at), photos.path",
                    (),
                )
                .await?;
            let mut records = Vec::new();
            while let Some(row) = rows.next().await? {
                let kind: String = row.get(1)?;
                let Some(kind) = DerivativeKind::from_key(&kind) else {
                    continue;
                };
                records.push(DerivativeRecord {
                    photo_path: row.get(0)?,
                    kind,
                    path: row.get(2)?,
                    width: row.get::<i64>(3)? as u32,
                    height: row.get::<i64>(4)? as u32,
                    file_size: row.get::<i64>(5)? as u64,
                    version: row.get::<i64>(6)? as u32,
                });
            }
            Ok(records)
        })
    }

    /// Records a derivative that's been made, replacing the photo's last
    /// one of its kind. As it's new, it's to be synced again.
    pub fn set_derivative(&self, record: &DerivativeRecord) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT id FROM photos WHERE path = ?1",
                    turso::params![record.photo_path.as_str()],
                )
                .await?;
            let photo_id: i64 = match rows.next().await? {
                Some(row) => row.get(0)?,
                None => bail!("{} isn't in the catalog", record.photo_path),
            };
            drop(rows);
            connection.execute("BEGIN", ()).await?;
            let result = async {
                connection
                    .execute(
                        "DELETE FROM derivatives WHERE (photo_id = ?1 AND kind = ?2) OR path = ?3",
                        turso::params![photo_id, record.kind.key(), record.path.as_str()],
                    )
                    .await?;
                connection
                    .execute(
                        "DELETE FROM sync_items WHERE path = ?1",
                        turso::params![record.path.as_str()],
                    )
                    .await?;
                connection
                    .execute(
                        "INSERT INTO derivatives (photo_id, kind, path, width, height,
                            file_size, version, created_at)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        turso::params![
                            photo_id,
                            record.kind.key(),
                            record.path.as_str(),
                            record.width as i64,
                            record.height as i64,
                            record.file_size as i64,
                            record.version as i64,
                            super::now(),
                        ],
                    )
                    .await?;
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

    /// The photos that `model` has looked at for objects.
    pub fn object_scans(&self, model: &str) -> Result<HashSet<String>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT photos.path FROM object_scans
                    JOIN photos ON photos.id = object_scans.photo_id
                    WHERE object_scans.model = ?1",
                    turso::params![model],
                )
                .await?;
            let mut paths = HashSet::new();
            while let Some(row) = rows.next().await? {
                paths.insert(row.get(0)?);
            }
            Ok(paths)
        })
    }

    /// The objects `model` found in a photo, most certain first. None if it
    /// hasn't looked at the photo.
    pub fn objects(&self, photo_path: &str, model: &str) -> Result<Option<Vec<DetectedObject>>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT object_scans.model FROM object_scans
                    JOIN photos ON photos.id = object_scans.photo_id
                    WHERE photos.path = ?1",
                    turso::params![photo_path],
                )
                .await?;
            match rows.next().await? {
                Some(row) if row.get::<String>(0)? == model => {}
                _ => return Ok(None),
            }
            drop(rows);
            let mut rows = connection
                .query(
                    "SELECT objects.label, objects.confidence, objects.x1, objects.y1,
                        objects.x2, objects.y2
                    FROM objects JOIN photos ON photos.id = objects.photo_id
                    WHERE photos.path = ?1
                    ORDER BY objects.confidence DESC",
                    turso::params![photo_path],
                )
                .await?;
            let mut objects = Vec::new();
            while let Some(row) = rows.next().await? {
                let real = |ix| row.get::<f64>(ix).map(|value| value as f32);
                objects.push(DetectedObject {
                    label: row.get(0)?,
                    confidence: real(1)?,
                    bounds: [real(2)?, real(3)?, real(4)?, real(5)?],
                });
            }
            Ok(Some(objects))
        })
    }

    /// Every kind of object `model` has found in the library, those in the
    /// most photos first.
    pub fn object_kinds(&self, model: &str) -> Result<Vec<ObjectKind>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT objects.label, count(DISTINCT objects.photo_id) AS photos
                    FROM objects
                    JOIN object_scans ON object_scans.photo_id = objects.photo_id
                    WHERE object_scans.model = ?1
                    GROUP BY objects.label
                    ORDER BY photos DESC, objects.label",
                    turso::params![model],
                )
                .await?;
            let mut kinds = Vec::new();
            while let Some(row) = rows.next().await? {
                kinds.push(ObjectKind {
                    label: row.get(0)?,
                    photos: row.get::<i64>(1)? as usize,
                });
            }
            Ok(kinds)
        })
    }

    /// The photos in which `model` found an object with one of `labels`.
    pub fn photos_with_objects(&self, labels: &[String], model: &str) -> Result<HashSet<String>> {
        let connection = self.connection();
        block_on(async {
            let mut paths = HashSet::new();
            for label in labels {
                let mut rows = connection
                    .query(
                        "SELECT DISTINCT photos.path FROM objects
                        JOIN photos ON photos.id = objects.photo_id
                        JOIN object_scans ON object_scans.photo_id = objects.photo_id
                        WHERE objects.label = ?1 AND object_scans.model = ?2",
                        turso::params![label.as_str(), model],
                    )
                    .await?;
                while let Some(row) = rows.next().await? {
                    paths.insert(row.get(0)?);
                }
            }
            Ok(paths)
        })
    }

    /// Records the objects `model` found in a photo (perhaps none),
    /// replacing what was found before.
    pub fn set_objects(
        &self,
        photo_path: &str,
        model: &str,
        objects: &[DetectedObject],
    ) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT id FROM photos WHERE path = ?1",
                    turso::params![photo_path],
                )
                .await?;
            let photo_id: i64 = match rows.next().await? {
                Some(row) => row.get(0)?,
                None => bail!("{photo_path} isn't in the catalog"),
            };
            drop(rows);
            connection.execute("BEGIN", ()).await?;
            let result = async {
                connection
                    .execute(
                        "DELETE FROM objects WHERE photo_id = ?1",
                        turso::params![photo_id],
                    )
                    .await?;
                connection
                    .execute(
                        "DELETE FROM object_scans WHERE photo_id = ?1",
                        turso::params![photo_id],
                    )
                    .await?;
                connection
                    .execute(
                        "INSERT INTO object_scans (photo_id, model, scanned_at)
                        VALUES (?1, ?2, ?3)",
                        turso::params![photo_id, model, super::now()],
                    )
                    .await?;
                for object in objects {
                    let [x1, y1, x2, y2] = object.bounds.map(f64::from);
                    connection
                        .execute(
                            "INSERT INTO objects (photo_id, label, confidence, x1, y1, x2, y2)
                            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                            turso::params![
                                photo_id,
                                object.label.as_str(),
                                f64::from(object.confidence),
                                x1,
                                y1,
                                x2,
                                y2,
                            ],
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

    pub fn sync_targets(&self) -> Result<Vec<SyncTargetRecord>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT id, target, folder, error FROM sync_targets ORDER BY id",
                    (),
                )
                .await?;
            let mut targets = Vec::new();
            while let Some(row) = rows.next().await? {
                targets.push(SyncTargetRecord {
                    id: row.get(0)?,
                    target: row.get(1)?,
                    folder: row.get(2)?,
                    error: optional_text(&row, 3)?,
                });
            }
            Ok(targets)
        })
    }

    /// Adds a target, returning the catalog's id for it.
    pub fn add_sync_target(&self, target: &str, folder: &str) -> Result<i64> {
        let connection = self.connection();
        block_on(connection.execute(
            "INSERT INTO sync_targets (target, folder, created_at) VALUES (?1, ?2, ?3)",
            turso::params![target, folder, super::now()],
        ))?;
        Ok(connection.last_insert_rowid())
    }

    pub fn set_sync_target_error(&self, id: i64, error: Option<&str>) -> Result<()> {
        let connection = self.connection();
        block_on(connection.execute(
            "UPDATE sync_targets SET error = ?2 WHERE id = ?1",
            turso::params![id, error],
        ))?;
        Ok(())
    }

    /// Removes a target, and the record of what was synced to it.
    pub fn remove_sync_target(&self, id: i64) -> Result<()> {
        let connection = self.connection();
        block_on(connection.execute_batch(&format!(
            "BEGIN;
            DELETE FROM sync_items WHERE target_id = {id};
            DELETE FROM sync_targets WHERE id = {id};
            COMMIT;"
        )))?;
        Ok(())
    }

    /// The library's files' syncs to a target.
    pub fn sync_items(&self, target_id: i64) -> Result<Vec<SyncItemRecord>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT path, state, remote_key, remote_version, size, attempts, error
                    FROM sync_items WHERE target_id = ?1 AND path IS NOT NULL",
                    turso::params![target_id],
                )
                .await?;
            let mut items = Vec::new();
            while let Some(row) = rows.next().await? {
                let state: String = row.get(1)?;
                items.push(SyncItemRecord {
                    path: row.get(0)?,
                    state: SyncState::from_key(&state)
                        .with_context(|| format!("unknown sync state {state}"))?,
                    remote_key: optional_text(&row, 2)?,
                    remote_version: optional_text(&row, 3)?,
                    size: match row.get_value(4)? {
                        Value::Integer(size) => Some(size as u64),
                        _ => None,
                    },
                    attempts: row.get::<i64>(5)? as u32,
                    error: optional_text(&row, 6)?,
                });
            }
            Ok(items)
        })
    }

    /// Records an attempt to sync a file. Failed attempts are counted;
    /// success resets the count.
    pub fn record_sync(&self, target_id: i64, item: &SyncItemRecord) -> Result<()> {
        let connection = self.connection();
        let now = super::now();
        let synced_at = (item.state == SyncState::Synced).then_some(now);
        block_on(async {
            let updated = connection
                .execute(
                    "UPDATE sync_items SET state = ?3, remote_key = ?4,
                        remote_version = ?5, size = ?6,
                        attempts = CASE WHEN ?3 = 'synced' THEN 0 ELSE attempts + 1 END,
                        error = ?7, synced_at = coalesce(?8, synced_at), updated_at = ?9
                    WHERE target_id = ?1 AND path = ?2",
                    turso::params![
                        target_id,
                        item.path.as_str(),
                        item.state.key(),
                        item.remote_key.as_deref(),
                        item.remote_version.as_deref(),
                        item.size.map(|size| size as i64),
                        item.error.as_deref(),
                        synced_at,
                        now,
                    ],
                )
                .await?;
            if updated == 0 {
                connection
                    .execute(
                        "INSERT INTO sync_items (target_id, path, state, remote_key,
                            remote_version, size, attempts, error, synced_at, updated_at)
                        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                        turso::params![
                            target_id,
                            item.path.as_str(),
                            item.state.key(),
                            item.remote_key.as_deref(),
                            item.remote_version.as_deref(),
                            item.size.map(|size| size as i64),
                            i64::from(item.state != SyncState::Synced),
                            item.error.as_deref(),
                            synced_at,
                            now,
                        ],
                    )
                    .await?;
            }
            Ok(())
        })
    }
}

/// Puts a photo in an album, unless it's there already. Returns whether it
/// wasn't.
async fn insert_album_photo(connection: &Connection, album: i64, photo: i64) -> Result<bool> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM album_photos WHERE album_id = ?1 AND photo_id = ?2",
            turso::params![album, photo],
        )
        .await?;
    if rows.next().await?.is_some() {
        return Ok(false);
    }
    drop(rows);
    connection
        .execute(
            "INSERT INTO album_photos (album_id, photo_id, added_at) VALUES (?1, ?2, ?3)",
            turso::params![album, photo, super::now()],
        )
        .await?;
    Ok(true)
}

fn optional_text(row: &Row, ix: usize) -> Result<Option<String>> {
    Ok(match row.get_value(ix)? {
        Value::Text(text) => Some(text),
        _ => None,
    })
}

async fn migrate(connection: &Connection) -> Result<()> {
    let mut rows = connection.query("PRAGMA user_version", ()).await?;
    let version = match rows.next().await? {
        Some(row) => row.get::<i64>(0)?,
        None => 0,
    };
    drop(rows);
    if version > SCHEMA_VERSION {
        bail!("the catalog is from a newer version of 1k Words (schema {version})");
    }
    for (to, schema) in [
        (1, SCHEMA_V1),
        (2, SCHEMA_V2),
        (3, SCHEMA_V3),
        (4, SCHEMA_V4),
        (5, SCHEMA_V5),
    ] {
        if version < to {
            connection
                .execute_batch(&format!(
                    "BEGIN; {schema} PRAGMA user_version = {to}; COMMIT;"
                ))
                .await?;
        }
    }
    Ok(())
}

fn record_from_row(row: &Row) -> Result<PhotoRecord> {
    let text = |ix| -> Result<Option<String>> {
        Ok(match row.get_value(ix)? {
            Value::Text(text) => Some(text),
            _ => None,
        })
    };
    let real = |ix| -> Result<Option<f64>> {
        Ok(match row.get_value(ix)? {
            Value::Real(value) => Some(value),
            Value::Integer(value) => Some(value as f64),
            _ => None,
        })
    };
    let integer = |ix| -> Result<Option<i64>> {
        Ok(match row.get_value(ix)? {
            Value::Integer(value) => Some(value),
            _ => None,
        })
    };
    let location = match (real(18)?, real(19)?) {
        (Some(latitude), Some(longitude)) => Some(Location {
            latitude,
            longitude,
            altitude: None,
            accuracy: None,
        }),
        _ => None,
    };
    Ok(PhotoRecord {
        path: text(0)?.context("photo without a path")?,
        raw_path: text(1)?,
        live_video_path: text(2)?,
        content_hash: text(3)?.unwrap_or_default(),
        file_size: integer(4)?.unwrap_or(0) as u64,
        original_path: text(5)?.unwrap_or_default(),
        imported_at: real(6)?.unwrap_or(0.0),
        width: integer(7)?.unwrap_or(0) as u32,
        height: integer(8)?.unwrap_or(0) as u32,
        meta: CaptureMeta {
            captured_at: real(9)?,
            modified_at: real(10)?.unwrap_or(0.0),
            camera: text(11)?,
            exposure_time: real(12)?,
            f_number: real(13)?,
            iso: integer(14)?.map(|iso| iso as u32),
            exposure_bias: real(15)?,
            auto_bracket: integer(16)? == Some(1),
            manual_exposure: integer(17)? == Some(1),
        },
        location,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads_back_photos() {
        let path = std::env::temp_dir().join(format!("1k-words-catalog-{}.db", std::process::id()));
        std::fs::remove_file(&path).ok();
        let record = PhotoRecord {
            path: "Originals/2026/10/15/IMG_0001.JPG".into(),
            raw_path: Some("Originals/2026/10/15/IMG_0001.CR3".into()),
            live_video_path: None,
            content_hash: "abc".into(),
            file_size: 1234,
            original_path: "/Volumes/EOS_DIGITAL/DCIM/100CANON/IMG_0001.JPG".into(),
            imported_at: 1_800_000_000.5,
            width: 6000,
            height: 4000,
            meta: CaptureMeta {
                captured_at: Some(1_790_000_000.25),
                modified_at: 1_790_000_001.0,
                camera: Some("Canon EOS R5".into()),
                exposure_time: Some(1.0 / 250.0),
                f_number: Some(2.8),
                iso: Some(400),
                exposure_bias: Some(-0.7),
                auto_bracket: true,
                manual_exposure: false,
            },
            location: Some(Location {
                latitude: 38.6,
                longitude: -90.2,
                altitude: None,
                accuracy: None,
            }),
        };
        let older = PhotoRecord {
            path: "Originals/2020/01/01/old.jpg".into(),
            content_hash: "def".into(),
            original_path: "/tmp/old.jpg".into(),
            meta: CaptureMeta {
                modified_at: 1_577_836_800.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let catalog = Catalog::open(&path).unwrap();
        catalog.insert(&record).unwrap();
        catalog.insert(&older).unwrap();
        // Paths are unique.
        assert!(catalog.insert(&record).is_err());
        assert!(catalog.contains("abc", 1234).unwrap());
        assert!(!catalog.contains("abc", 99).unwrap());
        assert_eq!(catalog.photo_count().unwrap(), 2);
        drop(catalog);

        // Reopening keeps the photos, oldest first.
        let catalog = Catalog::open(&path).unwrap();
        let photos = catalog.photos().unwrap();
        assert_eq!(photos, [older, record.clone()]);
        let original = |path: &str| LibraryFile {
            path: path.into(),
            role: FileRole::Original,
        };
        assert_eq!(
            catalog.files().unwrap(),
            [
                original("Originals/2020/01/01/old.jpg"),
                original("Originals/2026/10/15/IMG_0001.JPG"),
                original("Originals/2026/10/15/IMG_0001.CR3"),
            ]
        );

        // Derivatives are kept apart from the originals, one of each kind
        // per photo.
        let thumbnail = DerivativeRecord {
            photo_path: record.path.clone(),
            kind: DerivativeKind::Thumbnail,
            path: "Thumbnails/2026/10/15/IMG_0001.JPG.jpg".into(),
            width: 560,
            height: 373,
            file_size: 50_000,
            version: 1,
        };
        catalog.set_derivative(&thumbnail).unwrap();
        let target = catalog.add_sync_target("target", "Photos").unwrap();
        catalog
            .record_sync(
                target,
                &SyncItemRecord {
                    path: thumbnail.path.clone(),
                    state: SyncState::Synced,
                    remote_key: None,
                    remote_version: None,
                    size: Some(50_000),
                    attempts: 0,
                    error: None,
                },
            )
            .unwrap();
        let remade = DerivativeRecord {
            file_size: 40_000,
            version: 2,
            ..thumbnail.clone()
        };
        catalog.set_derivative(&remade).unwrap();
        assert_eq!(
            catalog.derivatives().unwrap(),
            std::slice::from_ref(&remade)
        );
        // Made again, it's to be synced again.
        assert!(catalog.sync_items(target).unwrap().is_empty());
        assert_eq!(
            catalog.photo_files().unwrap()[1],
            [
                original("Originals/2026/10/15/IMG_0001.JPG"),
                original("Originals/2026/10/15/IMG_0001.CR3"),
                LibraryFile {
                    path: remade.path.clone(),
                    role: FileRole::Derivative(DerivativeKind::Thumbnail),
                },
            ]
        );
        assert!(
            catalog
                .set_derivative(&DerivativeRecord {
                    photo_path: "Originals/nope.jpg".into(),
                    ..thumbnail
                })
                .is_err()
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stores_objects() {
        let path = std::env::temp_dir().join(format!(
            "1k-words-catalog-objects-{}.db",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        let catalog = Catalog::open(&path).unwrap();
        let photo = PhotoRecord {
            path: "Originals/2026/10/15/IMG_0001.JPG".into(),
            ..Default::default()
        };
        catalog.insert(&photo).unwrap();
        assert_eq!(catalog.objects(&photo.path, "model").unwrap(), None);
        assert!(catalog.object_scans("model").unwrap().is_empty());

        let dog = DetectedObject {
            label: "dog".into(),
            confidence: 0.5,
            bounds: [0.5, 0.5, 1.0, 0.75],
        };
        let person = DetectedObject {
            label: "person".into(),
            confidence: 0.875,
            bounds: [0.0, 0.25, 0.5, 1.0],
        };
        catalog
            .set_objects(&photo.path, "model", &[dog.clone(), person.clone()])
            .unwrap();
        // Most certain first.
        assert_eq!(
            catalog.objects(&photo.path, "model").unwrap(),
            Some(vec![person.clone(), dog.clone()])
        );
        assert_eq!(
            catalog.object_scans("model").unwrap(),
            HashSet::from([photo.path.clone()])
        );
        // Another model hasn't looked yet.
        assert_eq!(catalog.objects(&photo.path, "newer").unwrap(), None);
        assert!(catalog.object_scans("newer").unwrap().is_empty());

        // Looking again replaces what was found, even with nothing.
        catalog.set_objects(&photo.path, "newer", &[]).unwrap();
        assert_eq!(
            catalog.objects(&photo.path, "newer").unwrap(),
            Some(Vec::new())
        );
        assert!(catalog.object_scans("model").unwrap().is_empty());
        assert!(
            catalog
                .set_objects("Originals/nope.jpg", "model", &[])
                .is_err()
        );

        // Searching by kind of object.
        let other = PhotoRecord {
            path: "Originals/2026/10/16/IMG_0002.JPG".into(),
            ..Default::default()
        };
        catalog.insert(&other).unwrap();
        catalog
            .set_objects(&photo.path, "model", &[dog.clone()])
            .unwrap();
        let kind = |label: &str, photos| ObjectKind {
            label: label.into(),
            photos,
        };
        // Counting photos, not objects.
        catalog
            .set_objects(
                &other.path,
                "model",
                &[dog.clone(), dog.clone(), person.clone()],
            )
            .unwrap();
        assert_eq!(
            catalog.object_kinds("model").unwrap(),
            [kind("dog", 2), kind("person", 1)]
        );
        assert!(catalog.object_kinds("newer").unwrap().is_empty());
        assert_eq!(
            catalog
                .photos_with_objects(&["person".into()], "model")
                .unwrap(),
            HashSet::from([other.path.clone()])
        );
        assert_eq!(
            catalog
                .photos_with_objects(&["person".into(), "dog".into()], "model")
                .unwrap(),
            HashSet::from([photo.path.clone(), other.path.clone()])
        );
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn stores_albums() {
        let path =
            std::env::temp_dir().join(format!("1k-words-catalog-albums-{}.db", std::process::id()));
        std::fs::remove_file(&path).ok();
        let catalog = Catalog::open(&path).unwrap();
        let photo = |path: &str, hash: &str, captured_at| PhotoRecord {
            path: path.into(),
            content_hash: hash.into(),
            file_size: 10,
            meta: CaptureMeta {
                captured_at: Some(captured_at),
                ..Default::default()
            },
            ..Default::default()
        };
        let later = photo("Originals/b.jpg", "b", 2.);
        let earlier = photo("Originals/a.jpg", "a", 1.);
        catalog.insert(&later).unwrap();
        catalog.insert(&earlier).unwrap();

        let trips = catalog.create_album("Trips", None).unwrap();
        let japan = catalog.create_album("Japan", Some(trips)).unwrap();
        let family = catalog.create_album("family", None).unwrap();
        assert!(catalog.create_album("Lost", Some(999)).is_err());
        let album = |id, parent, name: &str, photos| Album {
            id,
            parent,
            name: name.into(),
            photos,
        };

        // Added by content, once each; oldest first.
        assert!(catalog.add_to_album(japan, "b", 10).unwrap());
        assert!(catalog.add_to_album(japan, "a", 10).unwrap());
        assert!(catalog.add_to_album(japan, "a", 10).unwrap());
        assert!(!catalog.add_to_album(japan, "nope", 10).unwrap());
        assert!(catalog.add_to_album(family, "a", 10).unwrap());
        assert_eq!(
            catalog.album_photos(japan).unwrap(),
            [earlier.clone(), later.clone()]
        );
        assert!(catalog.album_photos(trips).unwrap().is_empty());
        // By name, ignoring case.
        assert_eq!(
            catalog.albums().unwrap(),
            [
                album(family, None, "family", 1),
                album(japan, Some(trips), "Japan", 2),
                album(trips, None, "Trips", 0),
            ]
        );

        // By path, too, counting those that weren't there.
        let paths = [earlier.path.clone(), later.path.clone()];
        assert_eq!(catalog.add_paths_to_album(family, &paths).unwrap(), 1);
        assert_eq!(catalog.add_paths_to_album(family, &paths).unwrap(), 0);
        assert!(catalog.add_paths_to_album(999, &paths).is_err());
        assert!(
            catalog
                .add_paths_to_album(family, &["Originals/nope.jpg".into()])
                .is_err()
        );
        assert_eq!(catalog.album_photos(family).unwrap().len(), 2);
        assert_eq!(catalog.albums_with(&earlier.path).unwrap(), [japan, family]);

        // Taking them out again.
        assert_eq!(
            catalog
                .remove_paths_from_album(family, &paths[..1])
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .remove_paths_from_album(family, &paths[..1])
                .unwrap(),
            0
        );
        assert_eq!(catalog.albums_with(&earlier.path).unwrap(), [japan]);
        assert_eq!(catalog.add_paths_to_album(family, &paths).unwrap(), 1);

        catalog.rename_album(family, "Family").unwrap();
        catalog.delete_albums(&[trips, japan]).unwrap();
        assert_eq!(
            catalog.albums().unwrap(),
            [album(family, None, "Family", 2)]
        );
        // The photos stay in the library.
        assert_eq!(catalog.photo_count().unwrap(), 2);
        assert!(catalog.album_photos(japan).unwrap().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn records_syncs() {
        let path =
            std::env::temp_dir().join(format!("1k-words-catalog-sync-{}.db", std::process::id()));
        std::fs::remove_file(&path).ok();
        let catalog = Catalog::open(&path).unwrap();
        let id = catalog.add_sync_target("target-a", "Photos-1234").unwrap();
        let other = catalog.add_sync_target("target-b", "Photos-1234").unwrap();
        // A target's added once.
        assert!(catalog.add_sync_target("target-a", "Again").is_err());
        catalog
            .set_sync_target_error(id, Some("Access denied."))
            .unwrap();
        let targets = catalog.sync_targets().unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].id, id);
        assert_eq!(targets[0].target, "target-a");
        assert_eq!(targets[0].folder, "Photos-1234");
        assert_eq!(targets[0].error.as_deref(), Some("Access denied."));

        let failed = SyncItemRecord {
            path: "Originals/a.jpg".into(),
            state: SyncState::Failed,
            remote_key: None,
            remote_version: None,
            size: Some(10),
            attempts: 0,
            error: Some("timed out".into()),
        };
        catalog.record_sync(id, &failed).unwrap();
        catalog.record_sync(id, &failed).unwrap();
        assert_eq!(
            catalog.sync_items(id).unwrap(),
            [SyncItemRecord {
                attempts: 2,
                ..failed.clone()
            }]
        );
        let synced = SyncItemRecord {
            state: SyncState::Synced,
            remote_key: Some("photos/Originals/a.jpg".into()),
            remote_version: Some("etag".into()),
            error: None,
            ..failed.clone()
        };
        catalog.record_sync(id, &synced).unwrap();
        catalog.record_sync(other, &failed).unwrap();
        assert_eq!(catalog.sync_items(id).unwrap(), [synced]);

        // Removing a target forgets what was synced to it.
        catalog.remove_sync_target(id).unwrap();
        assert!(catalog.sync_items(id).unwrap().is_empty());
        assert_eq!(catalog.sync_items(other).unwrap().len(), 1);
        assert_eq!(catalog.sync_targets().unwrap().len(), 1);
        std::fs::remove_file(&path).ok();
    }
}

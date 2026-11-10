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
//!
//! A catalog can be synced between one person's devices, each changing it
//! while offline, so:
//!
//! - Rows have UUIDv7 ids (see `id`), never counted up from one, and rows
//!   two devices could both make, such as a photo's place in an album, are
//!   keyed by what they're about, so making one twice merges.
//! - Albums and photos aren't deleted straight away but marked deleted (a
//!   tombstone), with the device that did it and the `version` it saw. Every
//!   change to one, including what's in an album, gives it a new `version`.
//!   Syncing applies only the columns a change made, so a deletion on one
//!   device and a change on another both arrive; a tombstone whose
//!   `version` isn't the one its deletion saw was changed somewhere the
//!   deletion hadn't, and the user is asked which they meant (see
//!   `album_conflicts`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context as _, Result, bail};
use futures::executor::block_on;
use turso::{Connection, Database, Row, Value};

use super::albums::Album;
use super::derivatives::DerivativeKind;
use super::device::Device;
use super::id::{self, Id};
use super::objects::ObjectKind;
use crate::detect::DetectedObject;
use crate::location::Location;
use crate::photos::CaptureMeta;

/// Bumped with each change to the schema, which `migrate` brings old
/// catalogs up to.
const SCHEMA_VERSION: i64 = 10;

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

/// The schema version, kept in a table: syncing carries tables but not
/// `PRAGMA user_version`, and a catalog synced from a server needs to say
/// which version it is.
const SCHEMA_V9: &str = "
CREATE TABLE catalog_info (
    name TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
INSERT INTO catalog_info (name, value) VALUES ('schema_version', 9);
";

/// How syncing last went on each device, so each can show how the others
/// stand. A device writes only its own rows, so syncing them never
/// conflicts. `target_id` is a row in `sync_targets`, for uploading there,
/// or empty for the catalog's own sync with the catalog server.
///
/// Errors are per device, as targets' settings and credentials are, so
/// `sync_targets.error` is no longer used; what it held is dropped, and
/// found again on the next upload.
const SCHEMA_V10: &str = "
CREATE TABLE device_sync (
    device_id BLOB NOT NULL,
    target_id BLOB NOT NULL,
    -- When it last tried, and last went all the way through.
    attempted_at REAL NOT NULL,
    synced_at REAL,
    -- Why it stopped, if it stopped for something only the user can fix
    -- (bad credentials), or the catalog's sync failed.
    error TEXT,
    -- For uploads: the files on the device still to go, as of the last try.
    pending INTEGER,
    PRIMARY KEY (device_id, target_id)
);
";

/// The tables holding the catalog's contents, copied when it moves to a
/// server (see `Catalog::sync`). Not `catalog_info`, which the copy makes
/// for itself.
const TABLES: &[&str] = &[
    "devices",
    "device_sync",
    "photos",
    "sync_targets",
    "sync_items",
    "derivatives",
    "detector_scans",
    "objects",
    "albums",
    "album_photos",
];

/// The files a database is kept in: Turso's write-ahead log, and with
/// syncing, what's been synced and the changes not yet sent.
pub(super) const DATABASE_SUFFIXES: &[&str] =
    &["", "-wal", "-shm", "-info", "-changes", "-wal-revert"];

/// Whether an album shows the photos of the albums in it as well as its
/// own.
const SCHEMA_V6: &str = "
ALTER TABLE albums ADD COLUMN show_nested INTEGER NOT NULL DEFAULT 0;
";

/// Objects found by more than one detector (see `detect::Detector`): each
/// one's findings are kept, so switching between them doesn't look at
/// photos again. Some detectors don't say where objects are, so `boxed`
/// says whether the bounds mean anything.
const SCHEMA_V7: &str = "
CREATE TABLE detector_scans (
    -- The row in photos.
    photo_id INTEGER NOT NULL,
    -- See `detect::Detector::key`.
    detector TEXT NOT NULL,
    scanned_at REAL NOT NULL,
    PRIMARY KEY (photo_id, detector)
);
INSERT INTO detector_scans (photo_id, detector, scanned_at)
    SELECT photo_id, model, scanned_at FROM object_scans;
ALTER TABLE objects ADD COLUMN detector TEXT NOT NULL DEFAULT '';
UPDATE objects SET detector = coalesce(
    (SELECT model FROM object_scans WHERE object_scans.photo_id = objects.photo_id), '');
ALTER TABLE objects ADD COLUMN boxed INTEGER NOT NULL DEFAULT 1;
CREATE INDEX objects_detector_label ON objects (detector, label);
DROP TABLE object_scans;
";

/// Syncing between devices (see the module's description). Ids become
/// UUIDv7s; rows that two devices could both make are keyed by what they're
/// about; albums and photos gain versions and tombstones; and the devices
/// that change the catalog are listed, to name them. This is the whole
/// schema: new catalogs start here, and older ones are converted to it (see
/// `convert_to_v8`).
const SCHEMA_V8: &str = "
CREATE TABLE devices (
    id BLOB PRIMARY KEY,
    name TEXT NOT NULL,
    seen_at REAL NOT NULL
);
CREATE TABLE photos (
    id BLOB PRIMARY KEY,
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
    longitude REAL,
    -- A new id with each change, and the device that made it.
    version BLOB NOT NULL,
    changed_by BLOB,
    -- Set when it's deleted: when, where, and the version it had there.
    deleted_at REAL,
    deleted_by BLOB,
    deleted_seen BLOB
);
CREATE INDEX photos_content_hash ON photos (content_hash);
CREATE INDEX photos_captured_at ON photos (captured_at);
CREATE INDEX photos_camera ON photos (camera);
-- The targets the library syncs to. They're listed, with their settings,
-- in the app's `sync-targets.json`.
CREATE TABLE sync_targets (
    id BLOB PRIMARY KEY,
    -- The target's id in `sync-targets.json`.
    target TEXT NOT NULL UNIQUE,
    -- The library's folder in the target, so libraries sharing it don't mix.
    folder TEXT NOT NULL,
    created_at REAL NOT NULL,
    -- Why syncing stopped (such as bad credentials), until it next works.
    error TEXT
);
CREATE TABLE sync_items (
    -- The row in sync_targets.
    target_id BLOB NOT NULL,
    -- The file in the library.
    path TEXT NOT NULL,
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
    PRIMARY KEY (target_id, path)
);
CREATE INDEX sync_items_remote_key ON sync_items (target_id, remote_key);
CREATE INDEX sync_items_path ON sync_items (path);
CREATE TABLE derivatives (
    -- The row in photos.
    photo_id BLOB NOT NULL,
    -- 'thumbnail'.
    kind TEXT NOT NULL,
    path TEXT NOT NULL,
    width INTEGER NOT NULL,
    height INTEGER NOT NULL,
    file_size INTEGER NOT NULL,
    -- How it was made (see `DerivativeKind::version`).
    version INTEGER NOT NULL,
    created_at REAL NOT NULL,
    PRIMARY KEY (photo_id, kind)
);
CREATE INDEX derivatives_path ON derivatives (path);
-- The photos each detector has looked at, so photos where nothing was
-- found aren't looked at again.
CREATE TABLE detector_scans (
    -- The row in photos.
    photo_id BLOB NOT NULL,
    -- See `detect::Detector::key`.
    detector TEXT NOT NULL,
    scanned_at REAL NOT NULL,
    PRIMARY KEY (photo_id, detector)
);
CREATE TABLE objects (
    -- The row in photos.
    photo_id BLOB NOT NULL,
    detector TEXT NOT NULL,
    -- Its place among what the detector found in the photo.
    ix INTEGER NOT NULL,
    -- The model's word for it, e.g. 'person' or 'dog'.
    label TEXT NOT NULL,
    confidence REAL NOT NULL,
    -- Its bounds, as fractions of the photo's width and height, if `boxed`.
    x1 REAL NOT NULL,
    y1 REAL NOT NULL,
    x2 REAL NOT NULL,
    y2 REAL NOT NULL,
    boxed INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (photo_id, detector, ix)
);
CREATE INDEX objects_label ON objects (label);
CREATE INDEX objects_detector_label ON objects (detector, label);
CREATE TABLE albums (
    id BLOB PRIMARY KEY,
    -- The album it's in; NULL at the top level.
    parent_id BLOB,
    name TEXT NOT NULL,
    created_at REAL NOT NULL,
    show_nested INTEGER NOT NULL DEFAULT 0,
    -- As for photos.
    version BLOB NOT NULL,
    changed_by BLOB,
    deleted_at REAL,
    deleted_by BLOB,
    deleted_seen BLOB
);
CREATE INDEX albums_parent_id ON albums (parent_id);
CREATE TABLE album_photos (
    -- The rows in albums and photos.
    album_id BLOB NOT NULL,
    photo_id BLOB NOT NULL,
    added_at REAL NOT NULL,
    added_by BLOB,
    PRIMARY KEY (album_id, photo_id)
);
CREATE INDEX album_photos_photo_id ON album_photos (photo_id);
";

/// The tables `SCHEMA_V8` replaces, and their indexes.
const V7_TABLES: &[&str] = &[
    "photos",
    "sync_targets",
    "sync_items",
    "derivatives",
    "detector_scans",
    "objects",
    "albums",
    "album_photos",
];
const V7_INDEXES: &[&str] = &[
    "photos_content_hash",
    "photos_captured_at",
    "photos_camera",
    "sync_items_remote_key",
    "objects_photo_id",
    "objects_label",
    "objects_detector_label",
    "albums_parent_id",
    "album_photos_photo_id",
];

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
    pub id: Id,
    /// The target's id in `sync-targets.json`.
    pub target: String,
    pub folder: String,
    pub error: Option<String>,
}

/// A device, and how its syncs last went.
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceSyncRecord {
    pub id: Id,
    pub name: String,
    pub this_device: bool,
    /// When it last opened the catalog (seconds since the Unix epoch).
    pub seen_at: f64,
    /// The catalog's own sync with the catalog server.
    pub catalog: Option<SyncRun>,
    /// Uploads, by the row in `sync_targets`.
    pub uploads: Vec<(Id, SyncRun)>,
}

/// How a sync last went on a device. Times are seconds since the Unix
/// epoch, by that device's clock.
#[derive(Clone, Debug, PartialEq)]
pub struct SyncRun {
    pub attempted_at: f64,
    /// When it last went all the way through.
    pub synced_at: Option<f64>,
    pub error: Option<String>,
    /// For uploads: the files on the device still to go.
    pub pending: Option<usize>,
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

/// An album deleted on one device but changed on another since, so it's
/// not clear which was meant (see the module's description).
#[derive(Clone, Debug, PartialEq)]
pub struct AlbumConflict {
    pub album: Id,
    pub name: String,
    /// The devices it was deleted on, and changed on since.
    pub deleted_on: String,
    pub changed_on: String,
    /// Photos added to it since it was deleted.
    pub photos_added: usize,
}

pub struct Catalog {
    state: Mutex<State>,
    path: PathBuf,
    /// This device, which changes are recorded as made on.
    device: Id,
    device_name: String,
    /// Whether the catalog is from a newer version of the app, so it's only
    /// read. Kept outside `state` so it can be read during a sync.
    newer: AtomicBool,
}

/// The open database, swapped for another when the catalog moves to a
/// server (see `Catalog::sync`).
struct State {
    connection: Connection,
    store: Store,
}

impl std::ops::Deref for State {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        &self.connection
    }
}

enum Store {
    Local {
        _database: Database,
    },
    /// Synced with a server, at `url`, with the token to reach it, which is
    /// set before each sync.
    Synced {
        database: turso::sync::Database,
        url: String,
        token: Arc<Mutex<Option<String>>>,
    },
}

impl Catalog {
    /// Opens the catalog at `path`, creating it if needed, to change on
    /// `device`. Doesn't need the server a synced catalog syncs with.
    pub fn open(path: &Path, device: &Device) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let (state, newer) = block_on(open_state(path, device))
            .with_context(|| format!("opening catalog {}", path.display()))?;
        Ok(Self {
            state: Mutex::new(state),
            newer: AtomicBool::new(newer),
            path: path.to_owned(),
            device: device.id,
            device_name: device.name.clone(),
        })
    }

    fn connection(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the catalog is from a newer version of the app, so it can't
    /// be changed.
    pub fn is_newer(&self) -> bool {
        self.newer.load(Ordering::Relaxed)
    }

    /// The server the catalog syncs with, if it does: the URL of its copy
    /// there.
    #[cfg(test)]
    pub fn server_url(&self) -> Option<String> {
        match &self.connection().store {
            Store::Local { .. } => None,
            Store::Synced { url, .. } => Some(url.clone()),
        }
    }

    /// Syncs the catalog with its copy at `url`, on a server (see the
    /// module's description): sends the changes made here, then fetches
    /// those made elsewhere, returning whether any changed its photos or
    /// albums. Blocks on the network, and on other calls meanwhile.
    ///
    /// The first time, or if the catalog synced somewhere else before, it
    /// moves into a new database synced with `url` (see `move_to_server`).
    pub fn sync(&self, url: &str, token: Option<&str>) -> Result<bool> {
        let mut state = self.connection();
        let current = match &state.store {
            Store::Synced {
                url: synced,
                token: slot,
                database,
            } if synced == url => {
                *lock(slot) = token.map(str::to_owned);
                Some(database.clone())
            }
            _ => None,
        };
        let Some(database) = current else {
            self.move_to_server(&mut state, url, token)?;
            return Ok(true);
        };
        let (before, pulled) = block_on(async {
            let before = fingerprint(&state.connection).await?;
            // A catalog from a newer version may have changes the push
            // would undo, and has none of its own.
            if !self.is_newer() {
                database.push().await.context("sending changes")?;
            }
            let pulled = database.pull().await.context("fetching changes")?;
            Ok::<_, anyhow::Error>((before, pulled))
        })?;
        // What's fetched includes what was just sent.
        if !pulled {
            return Ok(false);
        }
        if !self.is_newer() && !block_on(migrate(&state.connection))? {
            block_on(state.connection.execute("PRAGMA query_only = 1", ()))?;
            self.newer.store(true, Ordering::Relaxed);
            return Ok(true);
        }
        Ok(block_on(fingerprint(&state.connection))? != before)
    }

    /// Moves the catalog into a new database synced with `url`. If the
    /// server has no copy of it yet, this one is copied there; if it has,
    /// as when another device has synced it already, that's used, and this
    /// one kept beside it as `<id>.db.before-sync`. Either way, it's only
    /// moved once the server has it, so the catalog stays as it was if the
    /// server can't be reached.
    fn move_to_server(&self, state: &mut State, url: &str, token: Option<&str>) -> Result<()> {
        if self.is_newer() {
            bail!("the catalog is from a newer version of 1k Words");
        }
        let new_path = with_suffix(&self.path, ".new");
        remove_database(&new_path)?;
        let slot = Arc::new(Mutex::new(token.map(str::to_owned)));
        let copied = block_on(async {
            let database = open_synced(&new_path, url, slot.clone()).await?;
            let connection = database.connect().await?;
            let server_has_it = table_exists(&connection, "photos").await?;
            if !migrate(&connection).await? {
                bail!("the server's copy is from a newer version of 1k Words");
            }
            if !server_has_it {
                copy_tables(&state.connection, &connection).await?;
            }
            database.push().await.context("sending the catalog")?;
            Ok(())
        });
        if let Err(error) = copied {
            remove_database(&new_path).ok();
            return Err(error.context("moving the catalog to the server"));
        }

        // Closed, to move its files.
        let placeholder = block_on(turso::Builder::new_local(":memory:").build())?;
        state.connection = placeholder.connect()?;
        state.store = Store::Local {
            _database: placeholder,
        };
        let backup = with_suffix(&self.path, ".before-sync");
        remove_database(&backup)?;
        rename_database(&self.path, &backup)?;
        rename_database(&new_path, &self.path)?;
        (*state, _) = block_on(open_state(&self.path, &self.device()))
            .context("opening the catalog moved to the server")?;
        if let Store::Synced {
            token: new_slot, ..
        } = &state.store
        {
            *lock(new_slot) = token.map(str::to_owned);
        }
        Ok(())
    }

    fn device(&self) -> Device {
        Device {
            id: self.device,
            name: self.device_name.clone(),
        }
    }

    /// Every photo, oldest first.
    pub fn photos(&self) -> Result<Vec<PhotoRecord>> {
        self.query_photos(
            &format!(
                "SELECT {PHOTO_COLUMNS} FROM photos
                WHERE photos.deleted_at IS NULL
                ORDER BY coalesce(photos.captured_at, photos.modified_at), photos.path"
            ),
            (),
        )
    }

    /// The photos in any of `albums` (not the albums in them), each once,
    /// oldest first.
    pub fn album_photos(&self, albums: &[Id]) -> Result<Vec<PhotoRecord>> {
        let ids = id::sql_list(albums);
        self.query_photos(
            &format!(
                "SELECT {PHOTO_COLUMNS} FROM photos
                WHERE photos.deleted_at IS NULL AND photos.id IN
                    (SELECT album_photos.photo_id FROM album_photos
                    JOIN albums ON albums.id = album_photos.album_id
                    WHERE album_photos.album_id IN ({ids}) AND albums.deleted_at IS NULL)
                ORDER BY coalesce(photos.captured_at, photos.modified_at), photos.path"
            ),
            (),
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

    /// Every album, by name (not those deleted).
    pub fn albums(&self) -> Result<Vec<Album>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT albums.id, albums.parent_id, albums.name,
                        (SELECT count(*) FROM album_photos
                            WHERE album_photos.album_id = albums.id),
                        albums.show_nested
                    FROM albums
                    WHERE albums.deleted_at IS NULL
                    ORDER BY albums.name COLLATE NOCASE, albums.id",
                    (),
                )
                .await?;
            let mut albums = Vec::new();
            while let Some(row) = rows.next().await? {
                albums.push(Album {
                    id: Id::from_value(row.get_value(0)?)?,
                    parent: Id::optional(row.get_value(1)?)?,
                    name: row.get(2)?,
                    photos: row.get::<i64>(3)? as usize,
                    show_nested: row.get::<i64>(4)? != 0,
                });
            }
            Ok(albums)
        })
    }

    /// Makes an album, in `parent` if given, returning its id.
    pub fn create_album(&self, name: &str, parent: Option<Id>) -> Result<Id> {
        let connection = self.connection();
        block_on(async {
            if let Some(parent) = parent
                && !album_exists(&connection, parent).await?
            {
                bail!("there's no album {parent} to put it in");
            }
            let id = Id::new();
            connection
                .execute(
                    "INSERT INTO albums (id, parent_id, name, created_at, version, changed_by)
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    turso::params![id, parent, name, super::now(), Id::new(), self.device],
                )
                .await?;
            // What's in the parent has changed.
            self.changed_albums(&connection, parent.as_slice()).await?;
            Ok(id)
        })
    }

    pub fn rename_album(&self, album: Id, name: &str) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            connection
                .execute(
                    "UPDATE albums SET name = ?2 WHERE id = ?1",
                    turso::params![album, name],
                )
                .await?;
            self.changed_albums(&connection, &[album]).await
        })
    }

    /// Puts an album in `parent`, or at the top level. The caller checks
    /// it isn't being put in itself.
    pub fn move_album(&self, album: Id, parent: Option<Id>) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT parent_id FROM albums WHERE id = ?1",
                    turso::params![album],
                )
                .await?;
            let old_parent = match rows.next().await? {
                Some(row) => Id::optional(row.get_value(0)?)?,
                None => bail!("the album isn't there any more"),
            };
            drop(rows);
            connection
                .execute(
                    "UPDATE albums SET parent_id = ?2 WHERE id = ?1",
                    turso::params![album, parent],
                )
                .await?;
            // It's changed, and so has what's in the albums it left and
            // joined.
            let changed: Vec<Id> = [Some(album), old_parent, parent]
                .into_iter()
                .flatten()
                .collect();
            self.changed_albums(&connection, &changed).await
        })
    }

    pub fn set_album_show_nested(&self, album: Id, show: bool) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            connection
                .execute(
                    "UPDATE albums SET show_nested = ?2 WHERE id = ?1",
                    turso::params![album, show as i64],
                )
                .await?;
            self.changed_albums(&connection, &[album]).await
        })
    }

    /// Every photo, by its displayed file, in each album.
    pub fn album_memberships(&self) -> Result<Vec<(Id, String)>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT album_photos.album_id, photos.path FROM album_photos
                    JOIN photos ON photos.id = album_photos.photo_id
                    JOIN albums ON albums.id = album_photos.album_id
                    WHERE albums.deleted_at IS NULL
                    ORDER BY album_photos.album_id, album_photos.added_at, album_photos.photo_id",
                    (),
                )
                .await?;
            let mut memberships = Vec::new();
            while let Some(row) = rows.next().await? {
                memberships.push((Id::from_value(row.get_value(0)?)?, row.get(1)?));
            }
            Ok(memberships)
        })
    }

    /// Deletes albums, given with every album in them. Their photos stay in
    /// the library. They're marked deleted rather than removed, keeping
    /// what was in them, in case another device has changed them since (see
    /// `album_conflicts`); `purge_deleted_albums` removes them later.
    pub fn delete_albums(&self, albums: &[Id]) -> Result<()> {
        if albums.is_empty() {
            return Ok(());
        }
        let ids = id::sql_list(albums);
        let connection = self.connection();
        block_on(connection.execute(
            &format!(
                "UPDATE albums SET deleted_at = ?1, deleted_by = ?2, deleted_seen = version
                WHERE id IN ({ids}) AND deleted_at IS NULL"
            ),
            turso::params![super::now(), self.device],
        ))?;
        Ok(())
    }

    /// Albums deleted on one device but changed on another since, by name.
    pub fn album_conflicts(&self) -> Result<Vec<AlbumConflict>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT albums.id, albums.name, deleted.name, changed.name,
                        (SELECT count(*) FROM album_photos
                            WHERE album_photos.album_id = albums.id
                            AND album_photos.added_at >= albums.deleted_at)
                    FROM albums
                    LEFT JOIN devices AS deleted ON deleted.id = albums.deleted_by
                    LEFT JOIN devices AS changed ON changed.id = albums.changed_by
                    WHERE albums.deleted_at IS NOT NULL
                        AND albums.deleted_seen IS NOT albums.version
                    ORDER BY albums.name COLLATE NOCASE, albums.id",
                    (),
                )
                .await?;
            let device = |name: Option<String>| name.unwrap_or_else(|| "another device".into());
            let mut conflicts = Vec::new();
            while let Some(row) = rows.next().await? {
                conflicts.push(AlbumConflict {
                    album: Id::from_value(row.get_value(0)?)?,
                    name: row.get(1)?,
                    deleted_on: device(optional_text(&row, 2)?),
                    changed_on: device(optional_text(&row, 3)?),
                    photos_added: row.get::<i64>(4)? as usize,
                });
            }
            Ok(conflicts)
        })
    }

    /// Settles a conflict: keeping the album undeletes it, the albums it's
    /// in, and the albums in it deleted along with it; otherwise it stays
    /// deleted, as the deleting device saw it.
    pub fn resolve_album_conflict(&self, album: Id, keep: bool) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            if !keep {
                connection
                    .execute(
                        "UPDATE albums SET deleted_seen = version WHERE id = ?1",
                        turso::params![album],
                    )
                    .await?;
                return Ok(());
            }
            // Every album, deleted or not: id, parent, and its deletion.
            let mut rows = connection
                .query(
                    "SELECT id, parent_id, deleted_at, deleted_by FROM albums",
                    (),
                )
                .await?;
            let mut albums = HashMap::new();
            while let Some(row) = rows.next().await? {
                let deletion = match row.get_value(2)? {
                    Value::Real(at) => Some((at, Id::optional(row.get_value(3)?)?)),
                    _ => None,
                };
                albums.insert(
                    Id::from_value(row.get_value(0)?)?,
                    (Id::optional(row.get_value(1)?)?, deletion),
                );
            }
            drop(rows);
            let Some((_, Some(deletion))) = albums.get(&album).copied() else {
                return Ok(());
            };
            let mut restore = vec![album];
            // The albums it's in.
            let mut next = albums[&album].0;
            while let Some(parent) = next
                && let Some((grandparent, parent_deletion)) = albums.get(&parent)
                && !restore.contains(&parent)
            {
                if parent_deletion.is_some() {
                    restore.push(parent);
                }
                next = *grandparent;
            }
            // Those in it that went with it.
            let mut ix = 0;
            let mut nested = vec![album];
            while let Some(&parent) = nested.get(ix) {
                for (id, (album_parent, album_deletion)) in &albums {
                    if *album_parent == Some(parent) && !nested.contains(id) {
                        nested.push(*id);
                        if *album_deletion == Some(deletion) {
                            restore.push(*id);
                        }
                    }
                }
                ix += 1;
            }
            let ids = id::sql_list(&restore);
            connection
                .execute(
                    &format!(
                        "UPDATE albums SET deleted_at = NULL, deleted_by = NULL,
                            deleted_seen = NULL
                        WHERE id IN ({ids})"
                    ),
                    (),
                )
                .await?;
            self.changed_albums(&connection, &restore).await
        })
    }

    /// Removes albums deleted before `before` (seconds since the Unix epoch)
    /// that aren't in conflict, and what was in them, and the record of
    /// photos in albums that are gone.
    pub fn purge_deleted_albums(&self, before: f64) -> Result<()> {
        let connection = self.connection();
        block_on(async {
            connection
                .execute(
                    "DELETE FROM album_photos WHERE album_id IN
                        (SELECT id FROM albums WHERE deleted_at < ?1
                            AND deleted_seen IS version)",
                    turso::params![before],
                )
                .await?;
            connection
                .execute(
                    "DELETE FROM albums WHERE deleted_at < ?1 AND deleted_seen IS version",
                    turso::params![before],
                )
                .await?;
            connection
                .execute(
                    "DELETE FROM album_photos WHERE album_id NOT IN (SELECT id FROM albums)",
                    (),
                )
                .await?;
            Ok(())
        })
    }

    /// Changes an album as if on `device`, called `name`, and synced here:
    /// only the columns the change made.
    #[cfg(test)]
    pub fn change_album_elsewhere(&self, album: Id, device: Id, name: &str) {
        let connection = self.connection();
        block_on(async {
            connection
                .execute(
                    "INSERT INTO devices (id, name, seen_at) VALUES (?1, ?2, 0)
                    ON CONFLICT (id) DO NOTHING",
                    turso::params![device, name],
                )
                .await?;
            connection
                .execute(
                    "UPDATE albums SET version = ?2, changed_by = ?3 WHERE id = ?1",
                    turso::params![album, Id::new(), device],
                )
                .await
        })
        .unwrap();
    }

    /// Records that albums have changed here: what's in them, or anything
    /// about them.
    async fn changed_albums(&self, connection: &Connection, albums: &[Id]) -> Result<()> {
        for album in albums {
            connection
                .execute(
                    "UPDATE albums SET version = ?2, changed_by = ?3 WHERE id = ?1",
                    turso::params![*album, Id::new(), self.device],
                )
                .await?;
        }
        Ok(())
    }

    /// Adds photos, by their displayed files, to an album, returning how
    /// many weren't in it already.
    pub fn add_paths_to_album(&self, album: Id, paths: &[String]) -> Result<usize> {
        let connection = self.connection();
        block_on(async {
            if !album_exists(&connection, album).await? {
                bail!("the album isn't there any more");
            }
            let mut added = 0;
            for path in paths {
                let mut rows = connection
                    .query(
                        "SELECT id FROM photos WHERE path = ?1",
                        turso::params![path.as_str()],
                    )
                    .await?;
                let photo = match rows.next().await? {
                    Some(row) => Id::from_value(row.get_value(0)?)?,
                    None => bail!("{path} isn't in the catalog"),
                };
                drop(rows);
                if self.insert_album_photo(&connection, album, photo).await? {
                    added += 1;
                }
            }
            if added > 0 {
                self.changed_albums(&connection, &[album]).await?;
            }
            Ok(added)
        })
    }

    /// Takes photos, by their displayed files, out of albums (not out of
    /// the library), returning how many were in any of them.
    pub fn remove_paths_from_albums(&self, albums: &[Id], paths: &[String]) -> Result<usize> {
        let connection = self.connection();
        block_on(async {
            let mut removed = HashSet::new();
            let mut changed = Vec::new();
            for &album in albums {
                for path in paths {
                    let deleted = connection
                        .execute(
                            "DELETE FROM album_photos WHERE album_id = ?1
                                AND photo_id IN (SELECT id FROM photos WHERE path = ?2)",
                            turso::params![album, path.as_str()],
                        )
                        .await?;
                    if deleted > 0 {
                        removed.insert(path);
                        if !changed.contains(&album) {
                            changed.push(album);
                        }
                    }
                }
            }
            self.changed_albums(&connection, &changed).await?;
            Ok(removed.len())
        })
    }

    /// The albums a photo, by its displayed file, is in, oldest first.
    pub fn albums_with(&self, path: &str) -> Result<Vec<Id>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT album_photos.album_id FROM album_photos
                    JOIN photos ON photos.id = album_photos.photo_id
                    JOIN albums ON albums.id = album_photos.album_id
                    WHERE photos.path = ?1 AND albums.deleted_at IS NULL
                    ORDER BY album_photos.album_id",
                    turso::params![path],
                )
                .await?;
            let mut albums = Vec::new();
            while let Some(row) = rows.next().await? {
                albums.push(Id::from_value(row.get_value(0)?)?);
            }
            Ok(albums)
        })
    }

    /// Adds the photo whose displayed file has this content to an album,
    /// unless it's there already. Returns whether there was such a photo.
    pub fn add_to_album(&self, album: Id, content_hash: &str, file_size: u64) -> Result<bool> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT id FROM photos WHERE content_hash = ?1 AND file_size = ?2 LIMIT 1",
                    turso::params![content_hash, file_size as i64],
                )
                .await?;
            let photo = match rows.next().await? {
                Some(row) => Id::from_value(row.get_value(0)?)?,
                None => return Ok(false),
            };
            drop(rows);
            if self.insert_album_photo(&connection, album, photo).await? {
                self.changed_albums(&connection, &[album]).await?;
            }
            Ok(true)
        })
    }

    /// Puts a photo in an album, unless it's there already. Returns whether
    /// it wasn't.
    async fn insert_album_photo(
        &self,
        connection: &Connection,
        album: Id,
        photo: Id,
    ) -> Result<bool> {
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
                "INSERT INTO album_photos (album_id, photo_id, added_at, added_by)
                VALUES (?1, ?2, ?3, ?4)",
                turso::params![album, photo, super::now(), self.device],
            )
            .await?;
        Ok(true)
    }

    /// How many photos there are (counting a raw pair or Live Photo as one).
    pub fn photo_count(&self) -> Result<usize> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query("SELECT count(*) FROM photos WHERE deleted_at IS NULL", ())
                .await?;
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
                longitude, id, version, changed_by
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
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
                Id::new(),
                Id::new(),
                self.device,
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
        let mut derivatives: HashMap<Id, Vec<LibraryFile>> = HashMap::new();
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT photo_id, kind, path FROM derivatives ORDER BY created_at, kind",
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
                    .entry(Id::from_value(row.get_value(0)?)?)
                    .or_default()
                    .push(LibraryFile {
                        path: row.get(2)?,
                        role: FileRole::Derivative(kind),
                    });
            }
            let mut rows = connection
                .query(
                    "SELECT id, path, raw_path, live_video_path FROM photos
                    WHERE deleted_at IS NULL
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
                let id = Id::from_value(row.get_value(0)?)?;
                files.extend(derivatives.remove(&id).unwrap_or_default());
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
            let photo_id = match rows.next().await? {
                Some(row) => Id::from_value(row.get_value(0)?)?,
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

    /// The photos that `detector` has looked at for objects.
    pub fn object_scans(&self, detector: &str) -> Result<HashSet<String>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT photos.path FROM detector_scans
                    JOIN photos ON photos.id = detector_scans.photo_id
                    WHERE detector_scans.detector = ?1",
                    turso::params![detector],
                )
                .await?;
            let mut paths = HashSet::new();
            while let Some(row) = rows.next().await? {
                paths.insert(row.get(0)?);
            }
            Ok(paths)
        })
    }

    /// The objects `detector` found in a photo, most certain first. None if
    /// it hasn't looked at the photo.
    pub fn objects(&self, photo_path: &str, detector: &str) -> Result<Option<Vec<DetectedObject>>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT 1 FROM detector_scans
                    JOIN photos ON photos.id = detector_scans.photo_id
                    WHERE photos.path = ?1 AND detector_scans.detector = ?2",
                    turso::params![photo_path, detector],
                )
                .await?;
            if rows.next().await?.is_none() {
                return Ok(None);
            }
            drop(rows);
            let mut rows = connection
                .query(
                    "SELECT objects.label, objects.confidence, objects.x1, objects.y1,
                        objects.x2, objects.y2, objects.boxed
                    FROM objects JOIN photos ON photos.id = objects.photo_id
                    WHERE photos.path = ?1 AND objects.detector = ?2
                    ORDER BY objects.confidence DESC",
                    turso::params![photo_path, detector],
                )
                .await?;
            let mut objects = Vec::new();
            while let Some(row) = rows.next().await? {
                let real = |ix| row.get::<f64>(ix).map(|value| value as f32);
                let bounds = [real(2)?, real(3)?, real(4)?, real(5)?];
                objects.push(DetectedObject {
                    label: row.get(0)?,
                    confidence: real(1)?,
                    bounds: (row.get::<i64>(6)? != 0).then_some(bounds),
                });
            }
            Ok(Some(objects))
        })
    }

    /// Every kind of object `detector` has found in the library, those in
    /// the most photos first.
    pub fn object_kinds(&self, detector: &str) -> Result<Vec<ObjectKind>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT objects.label, count(DISTINCT objects.photo_id) AS photos
                    FROM objects
                    WHERE objects.detector = ?1
                    GROUP BY objects.label
                    ORDER BY photos DESC, objects.label",
                    turso::params![detector],
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

    /// The photos in which `detector` found an object with one of `labels`.
    pub fn photos_with_objects(
        &self,
        labels: &[String],
        detector: &str,
    ) -> Result<HashSet<String>> {
        let connection = self.connection();
        block_on(async {
            let mut paths = HashSet::new();
            for label in labels {
                let mut rows = connection
                    .query(
                        "SELECT DISTINCT photos.path FROM objects
                        JOIN photos ON photos.id = objects.photo_id
                        WHERE objects.label = ?1 AND objects.detector = ?2",
                        turso::params![label.as_str(), detector],
                    )
                    .await?;
                while let Some(row) = rows.next().await? {
                    paths.insert(row.get(0)?);
                }
            }
            Ok(paths)
        })
    }

    /// Records the objects `detector` found in a photo (perhaps none),
    /// replacing what it found before.
    pub fn set_objects(
        &self,
        photo_path: &str,
        detector: &str,
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
            let photo_id = match rows.next().await? {
                Some(row) => Id::from_value(row.get_value(0)?)?,
                None => bail!("{photo_path} isn't in the catalog"),
            };
            drop(rows);
            connection.execute("BEGIN", ()).await?;
            let result = async {
                connection
                    .execute(
                        "DELETE FROM objects WHERE photo_id = ?1 AND detector = ?2",
                        turso::params![photo_id, detector],
                    )
                    .await?;
                connection
                    .execute(
                        "DELETE FROM detector_scans WHERE photo_id = ?1 AND detector = ?2",
                        turso::params![photo_id, detector],
                    )
                    .await?;
                connection
                    .execute(
                        "INSERT INTO detector_scans (photo_id, detector, scanned_at)
                        VALUES (?1, ?2, ?3)",
                        turso::params![photo_id, detector, super::now()],
                    )
                    .await?;
                for (ix, object) in objects.iter().enumerate() {
                    let [x1, y1, x2, y2] = object.bounds.unwrap_or_default().map(f64::from);
                    connection
                        .execute(
                            "INSERT INTO objects
                                (photo_id, detector, ix, label, confidence, x1, y1, x2, y2, boxed)
                            VALUES (?1, ?2, ?10, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                            turso::params![
                                photo_id,
                                detector,
                                object.label.as_str(),
                                f64::from(object.confidence),
                                x1,
                                y1,
                                x2,
                                y2,
                                object.bounds.is_some() as i64,
                                ix as i64,
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
                    "SELECT t.id, t.target, t.folder, d.error FROM sync_targets AS t
                    LEFT JOIN device_sync AS d ON d.target_id = t.id AND d.device_id = ?1
                    ORDER BY t.id",
                    turso::params![self.device],
                )
                .await?;
            let mut targets = Vec::new();
            while let Some(row) = rows.next().await? {
                targets.push(SyncTargetRecord {
                    id: Id::from_value(row.get_value(0)?)?,
                    target: row.get(1)?,
                    folder: row.get(2)?,
                    error: optional_text(&row, 3)?,
                });
            }
            Ok(targets)
        })
    }

    /// Adds a target, returning the catalog's id for it.
    pub fn add_sync_target(&self, target: &str, folder: &str) -> Result<Id> {
        let connection = self.connection();
        let id = Id::new();
        block_on(connection.execute(
            "INSERT INTO sync_targets (id, target, folder, created_at) VALUES (?1, ?2, ?3, ?4)",
            turso::params![id, target, folder, super::now()],
        ))?;
        Ok(id)
    }

    /// Records how a sync on this device went: uploading to `target`, or
    /// the catalog's own sync for `None`. `completed` is whether it went
    /// all the way through; `error`, why it stopped if only the user can
    /// fix it; `pending`, the files still to upload.
    pub fn record_device_sync(
        &self,
        target: Option<Id>,
        completed: bool,
        error: Option<&str>,
        pending: Option<usize>,
    ) -> Result<()> {
        let connection = self.connection();
        let now = super::now();
        block_on(connection.execute(
            "INSERT INTO device_sync (device_id, target_id, attempted_at, synced_at, error, pending)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT (device_id, target_id) DO UPDATE SET
                attempted_at = excluded.attempted_at,
                synced_at = coalesce(excluded.synced_at, synced_at),
                error = excluded.error,
                pending = excluded.pending",
            turso::params![
                self.device,
                target_value(target),
                now,
                completed.then_some(now),
                error,
                pending.map(|pending| pending as i64)
            ],
        ))?;
        Ok(())
    }

    /// Every device that has opened the catalog, and how its syncs last
    /// went, this device first, then by name.
    pub fn device_sync(&self) -> Result<Vec<DeviceSyncRecord>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT d.id, d.name, d.seen_at,
                        s.target_id, s.attempted_at, s.synced_at, s.error, s.pending
                    FROM devices AS d
                    LEFT JOIN device_sync AS s ON s.device_id = d.id
                    ORDER BY d.id = ?1 DESC, d.name, d.id",
                    turso::params![self.device],
                )
                .await?;
            let mut devices: Vec<DeviceSyncRecord> = Vec::new();
            while let Some(row) = rows.next().await? {
                let id = Id::from_value(row.get_value(0)?)?;
                if devices.last().is_none_or(|device| device.id != id) {
                    devices.push(DeviceSyncRecord {
                        id,
                        name: row.get(1)?,
                        this_device: id == self.device,
                        seen_at: row.get(2)?,
                        catalog: None,
                        uploads: Vec::new(),
                    });
                }
                let device = devices.last_mut().expect("just pushed");
                let target = match row.get_value(3)? {
                    Value::Null => continue,
                    Value::Blob(bytes) if bytes.is_empty() => None,
                    value => Some(Id::from_value(value)?),
                };
                let run = SyncRun {
                    attempted_at: row.get(4)?,
                    synced_at: match row.get_value(5)? {
                        Value::Real(at) => Some(at),
                        _ => None,
                    },
                    error: optional_text(&row, 6)?,
                    pending: match row.get_value(7)? {
                        Value::Integer(pending) => Some(pending.max(0) as usize),
                        _ => None,
                    },
                };
                match target {
                    None => device.catalog = Some(run),
                    Some(target) => device.uploads.push((target, run)),
                }
            }
            Ok(devices)
        })
    }

    /// Removes a target, and the record of what was synced to it.
    pub fn remove_sync_target(&self, id: Id) -> Result<()> {
        let id = id.sql();
        let connection = self.connection();
        block_on(connection.execute_batch(&format!(
            "BEGIN;
            DELETE FROM sync_items WHERE target_id = {id};
            DELETE FROM device_sync WHERE target_id = {id};
            DELETE FROM sync_targets WHERE id = {id};
            COMMIT;"
        )))?;
        Ok(())
    }

    /// The library's files' syncs to a target.
    pub fn sync_items(&self, target_id: Id) -> Result<Vec<SyncItemRecord>> {
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT path, state, remote_key, remote_version, size, attempts, error
                    FROM sync_items WHERE target_id = ?1",
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
    pub fn record_sync(&self, target_id: Id, item: &SyncItemRecord) -> Result<()> {
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

/// Whether there's an album `album`, not deleted.
/// Opens the database at `path`: synced, if it's been moved to a server
/// (see `Catalog::sync`), or local.
/// Returns it, and whether it's from a newer version of the app (see
/// `migrate`).
async fn open_state(path: &Path, device: &Device) -> Result<(State, bool)> {
    let (connection, store) = match synced_url(path) {
        Some(url) => {
            let token = Arc::new(Mutex::new(None));
            let database = open_synced(path, &url, token.clone()).await?;
            let connection = database.connect().await?;
            (
                connection,
                Store::Synced {
                    database,
                    url,
                    token,
                },
            )
        }
        None => {
            let path = path.to_str().context("catalog path isn't valid UTF-8")?;
            let database = turso::Builder::new_local(path).build().await?;
            (
                database.connect()?,
                Store::Local {
                    _database: database,
                },
            )
        }
    };
    let newer = !migrate(&connection).await?;
    if newer {
        connection.execute("PRAGMA query_only = 1", ()).await?;
    } else {
        connection
            .execute(
                "INSERT INTO devices (id, name, seen_at) VALUES (?1, ?2, ?3)
                ON CONFLICT (id) DO UPDATE SET name = excluded.name, seen_at = excluded.seen_at",
                turso::params![device.id, device.name.as_str(), super::now()],
            )
            .await?;
    }
    Ok((State { connection, store }, newer))
}

/// Opens, or makes, a database synced with `url`. Making one needs the
/// server, to fetch what it has; opening one doesn't.
async fn open_synced(
    path: &Path,
    url: &str,
    token: Arc<Mutex<Option<String>>>,
) -> Result<turso::sync::Database> {
    // Fails, harmlessly, once one's installed.
    rustls::crypto::ring::default_provider()
        .install_default()
        .ok();
    let path = path.to_str().context("catalog path isn't valid UTF-8")?;
    let database = turso::sync::Builder::new_remote(path)
        .with_remote_url(url)
        .with_client_name("1k-words")
        .with_auth_token_fn(move || {
            let token = lock(&token).clone().unwrap_or_default();
            async move { Ok(token) }
        })
        .build()
        .await?;
    Ok(database)
}

/// The server a database at `path` syncs with, if it does: kept by Turso
/// in a file beside it.
fn synced_url(path: &Path) -> Option<String> {
    let info = std::fs::read(with_suffix(path, "-info")).ok()?;
    let info: serde_json::Value = serde_json::from_slice(&info).ok()?;
    info["saved_configuration"]["remote_url"]
        .as_str()
        .map(str::to_owned)
}

/// Stands for the photos' and albums' versions, which every change to them,
/// or to what's in an album, replaces: if it's the same, they are.
async fn fingerprint(connection: &Connection) -> Result<u64> {
    use std::hash::{BuildHasher, BuildHasherDefault, DefaultHasher};
    let mut rows = connection
        .query(
            "SELECT version FROM photos UNION ALL SELECT version FROM albums",
            (),
        )
        .await?;
    let hasher = BuildHasherDefault::<DefaultHasher>::default();
    let (mut combined, mut count) = (0u64, 0u64);
    while let Some(row) = rows.next().await? {
        // Whatever order the rows come in.
        combined ^= hasher.hash_one(row.get_value(0)?.as_blob().map(Vec::as_slice));
        count += 1;
    }
    Ok(combined ^ hasher.hash_one(count))
}

/// Copies every row of the catalog's tables into another catalog of the
/// same version.
async fn copy_tables(from: &Connection, to: &Connection) -> Result<()> {
    to.execute("BEGIN", ()).await?;
    let copied = async {
        for table in TABLES {
            let mut rows = from.query(&format!("SELECT * FROM {table}"), ()).await?;
            while let Some(row) = rows.next().await? {
                let values = (0..row.column_count())
                    .map(|ix| row.get_value(ix))
                    .collect::<turso::Result<Vec<Value>>>()?;
                let params = vec!["?"; values.len()].join(", ");
                to.execute(&format!("INSERT INTO {table} VALUES ({params})"), values)
                    .await?;
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match copied {
        Ok(()) => {
            to.execute("COMMIT", ()).await?;
            Ok(())
        }
        Err(error) => {
            to.execute("ROLLBACK", ()).await.ok();
            Err(error.context("copying the catalog"))
        }
    }
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut path = path.as_os_str().to_owned();
    path.push(suffix);
    PathBuf::from(path)
}

/// Deletes a database's files (see `DATABASE_SUFFIXES`).
pub(super) fn remove_database(path: &Path) -> Result<()> {
    for suffix in DATABASE_SUFFIXES {
        let file = with_suffix(path, suffix);
        match std::fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                return Err(error).with_context(|| format!("deleting {}", file.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

fn rename_database(from: &Path, to: &Path) -> Result<()> {
    for suffix in DATABASE_SUFFIXES {
        let (from, to) = (with_suffix(from, suffix), with_suffix(to, suffix));
        if from.exists() {
            std::fs::rename(&from, &to)
                .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

async fn album_exists(connection: &Connection, album: Id) -> Result<bool> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM albums WHERE id = ?1 AND deleted_at IS NULL",
            turso::params![album],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// `device_sync.target_id`: the target's row, or empty for the catalog.
fn target_value(target: Option<Id>) -> Value {
    target.map_or(Value::Blob(Vec::new()), Value::from)
}

fn optional_text(row: &Row, ix: usize) -> Result<Option<String>> {
    Ok(match row.get_value(ix)? {
        Value::Text(text) => Some(text),
        _ => None,
    })
}

/// Brings the catalog up to `SCHEMA_VERSION`, returning whether it can be
/// changed: one from a newer version of the app, as when another device
/// has synced it, can only be read.
async fn migrate(connection: &Connection) -> Result<bool> {
    let mut version = schema_version(connection).await?;
    if version > SCHEMA_VERSION {
        return Ok(false);
    }
    if version == 0 {
        connection
            .execute_batch(&format!(
                "BEGIN; {SCHEMA_V8} PRAGMA user_version = 8; COMMIT;"
            ))
            .await?;
        version = 8;
    }
    for (to, schema) in [
        (1, SCHEMA_V1),
        (2, SCHEMA_V2),
        (3, SCHEMA_V3),
        (4, SCHEMA_V4),
        (5, SCHEMA_V5),
        (6, SCHEMA_V6),
        (7, SCHEMA_V7),
    ] {
        if version < to {
            connection
                .execute_batch(&format!(
                    "BEGIN; {schema} PRAGMA user_version = {to}; COMMIT;"
                ))
                .await?;
        }
    }
    if version < 8 {
        connection.execute("BEGIN", ()).await?;
        match convert_to_v8(connection).await {
            Ok(()) => connection.execute("COMMIT", ()).await?,
            Err(error) => {
                connection.execute("ROLLBACK", ()).await.ok();
                return Err(error.context("converting the catalog to schema 8"));
            }
        };
    }
    for (to, schema) in [(9, SCHEMA_V9), (10, SCHEMA_V10)] {
        if version < to {
            connection
                .execute_batch(&format!(
                    "BEGIN; {schema}
                    UPDATE catalog_info SET value = {to} WHERE name = 'schema_version';
                    PRAGMA user_version = {to}; COMMIT;"
                ))
                .await?;
        }
    }
    Ok(true)
}

/// The catalog's schema version, 0 for an empty one: in `catalog_info`
/// from version 9, in `PRAGMA user_version` before.
async fn schema_version(connection: &Connection) -> Result<i64> {
    if table_exists(connection, "catalog_info").await? {
        let mut rows = connection
            .query(
                "SELECT value FROM catalog_info WHERE name = 'schema_version'",
                (),
            )
            .await?;
        if let Some(row) = rows.next().await? {
            return Ok(row.get::<i64>(0)?);
        }
    }
    let mut rows = connection.query("PRAGMA user_version", ()).await?;
    Ok(match rows.next().await? {
        Some(row) => row.get::<i64>(0)?,
        None => 0,
    })
}

async fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    let mut rows = connection
        .query(
            "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
        )
        .await?;
    Ok(rows.next().await?.is_some())
}

/// Converts a version 7 catalog to `SCHEMA_V8`, in the transaction under
/// way. Rows get ids made from when they were made, so they keep their
/// order; the old ids are mapped to them in a table of their own, which
/// the copying joins.
async fn convert_to_v8(connection: &Connection) -> Result<()> {
    for index in V7_INDEXES {
        connection
            .execute(&format!("DROP INDEX IF EXISTS {index}"), ())
            .await?;
    }
    for table in V7_TABLES {
        connection
            .execute(&format!("ALTER TABLE {table} RENAME TO v7_{table}"), ())
            .await?;
    }
    connection.execute_batch(SCHEMA_V8).await?;
    connection
        .execute(
            "CREATE TABLE v8_ids (
                tbl TEXT NOT NULL,
                old INTEGER NOT NULL,
                new BLOB NOT NULL,
                PRIMARY KEY (tbl, old)
            )",
            (),
        )
        .await?;
    for (table, time) in [
        ("photos", "imported_at"),
        ("albums", "created_at"),
        ("sync_targets", "created_at"),
    ] {
        let mut rows = connection
            .query(&format!("SELECT id, {time} FROM v7_{table}"), ())
            .await?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next().await? {
            let at = match row.get_value(1)? {
                Value::Real(at) => at,
                Value::Integer(at) => at as f64,
                _ => 0.0,
            };
            ids.push((row.get::<i64>(0)?, Id::at(at)));
        }
        drop(rows);
        for (old, new) in ids {
            connection
                .execute(
                    "INSERT INTO v8_ids (tbl, old, new) VALUES (?1, ?2, ?3)",
                    turso::params![table, old, new],
                )
                .await?;
        }
    }
    connection
        .execute_batch(
            "INSERT INTO photos (id, path, raw_path, live_video_path, content_hash, file_size,
                original_path, imported_at, width, height, captured_at, modified_at, camera,
                exposure_time, f_number, iso, exposure_bias, auto_bracket, manual_exposure,
                latitude, longitude, version)
            SELECT m.new, p.path, p.raw_path, p.live_video_path, p.content_hash, p.file_size,
                p.original_path, p.imported_at, p.width, p.height, p.captured_at, p.modified_at,
                p.camera, p.exposure_time, p.f_number, p.iso, p.exposure_bias, p.auto_bracket,
                p.manual_exposure, p.latitude, p.longitude, m.new
            FROM v7_photos AS p JOIN v8_ids AS m ON m.tbl = 'photos' AND m.old = p.id;

            INSERT INTO albums (id, parent_id, name, created_at, show_nested, version)
            SELECT m.new, parent.new, a.name, a.created_at, a.show_nested, m.new
            FROM v7_albums AS a
            JOIN v8_ids AS m ON m.tbl = 'albums' AND m.old = a.id
            LEFT JOIN v8_ids AS parent ON parent.tbl = 'albums' AND parent.old = a.parent_id;

            INSERT INTO album_photos (album_id, photo_id, added_at)
            SELECT album.new, photo.new, ap.added_at
            FROM v7_album_photos AS ap
            JOIN v8_ids AS album ON album.tbl = 'albums' AND album.old = ap.album_id
            JOIN v8_ids AS photo ON photo.tbl = 'photos' AND photo.old = ap.photo_id;

            INSERT INTO derivatives (photo_id, kind, path, width, height, file_size, version,
                created_at)
            SELECT photo.new, d.kind, d.path, d.width, d.height, d.file_size, d.version,
                d.created_at
            FROM v7_derivatives AS d
            JOIN v8_ids AS photo ON photo.tbl = 'photos' AND photo.old = d.photo_id;

            INSERT INTO detector_scans (photo_id, detector, scanned_at)
            SELECT photo.new, s.detector, s.scanned_at
            FROM v7_detector_scans AS s
            JOIN v8_ids AS photo ON photo.tbl = 'photos' AND photo.old = s.photo_id;

            INSERT INTO objects (photo_id, detector, ix, label, confidence, x1, y1, x2, y2,
                boxed)
            SELECT photo.new, o.detector, o.id, o.label, o.confidence, o.x1, o.y1, o.x2, o.y2,
                o.boxed
            FROM v7_objects AS o
            JOIN v8_ids AS photo ON photo.tbl = 'photos' AND photo.old = o.photo_id;

            INSERT INTO sync_targets (id, target, folder, created_at, error)
            SELECT m.new, t.target, t.folder, t.created_at, t.error
            FROM v7_sync_targets AS t JOIN v8_ids AS m ON m.tbl = 'sync_targets' AND m.old = t.id;

            INSERT INTO sync_items (target_id, path, state, remote_key, remote_version, size,
                attempts, error, synced_at, updated_at)
            SELECT target.new, i.path, i.state, i.remote_key, i.remote_version, i.size,
                i.attempts, i.error, i.synced_at, i.updated_at
            FROM v7_sync_items AS i
            JOIN v8_ids AS target ON target.tbl = 'sync_targets' AND target.old = i.target_id
            WHERE i.path IS NOT NULL;",
        )
        .await?;
    for table in V7_TABLES {
        connection
            .execute(&format!("DROP TABLE v7_{table}"), ())
            .await?;
    }
    connection.execute("DROP TABLE v8_ids", ()).await?;
    connection.execute("PRAGMA user_version = 8", ()).await?;
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

    fn test_device() -> Device {
        Device {
            id: Id::from_u128(1),
            name: "Test".into(),
        }
    }

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

        let catalog = Catalog::open(&path, &test_device()).unwrap();
        catalog.insert(&record).unwrap();
        catalog.insert(&older).unwrap();
        // Paths are unique.
        assert!(catalog.insert(&record).is_err());
        assert!(catalog.contains("abc", 1234).unwrap());
        assert!(!catalog.contains("abc", 99).unwrap());
        assert_eq!(catalog.photo_count().unwrap(), 2);
        drop(catalog);

        // Reopening keeps the photos, oldest first.
        let catalog = Catalog::open(&path, &test_device()).unwrap();
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
        let catalog = Catalog::open(&path, &test_device()).unwrap();
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
            bounds: Some([0.5, 0.5, 1.0, 0.75]),
        };
        let person = DetectedObject {
            label: "person".into(),
            confidence: 0.875,
            bounds: Some([0.0, 0.25, 0.5, 1.0]),
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

        // Each model's findings are kept apart: another finding nothing
        // leaves the first's. Some don't say where things are.
        let cat = DetectedObject {
            label: "cat".into(),
            confidence: 0.75,
            bounds: None,
        };
        catalog
            .set_objects(&photo.path, "newer", &[cat.clone()])
            .unwrap();
        assert_eq!(
            catalog.objects(&photo.path, "newer").unwrap(),
            Some(vec![cat])
        );
        catalog.set_objects(&photo.path, "newer", &[]).unwrap();
        assert_eq!(
            catalog.objects(&photo.path, "newer").unwrap(),
            Some(Vec::new())
        );
        assert_eq!(
            catalog.objects(&photo.path, "model").unwrap(),
            Some(vec![person.clone(), dog.clone()])
        );
        assert_eq!(catalog.object_scans("model").unwrap().len(), 1);
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
        let catalog = Catalog::open(&path, &test_device()).unwrap();
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
        assert!(catalog.create_album("Lost", Some(Id::new())).is_err());
        let album = |id, parent, name: &str, photos| Album {
            id,
            parent,
            name: name.into(),
            photos,
            show_nested: false,
        };

        // Added by content, once each; oldest first.
        assert!(catalog.add_to_album(japan, "b", 10).unwrap());
        assert!(catalog.add_to_album(japan, "a", 10).unwrap());
        assert!(catalog.add_to_album(japan, "a", 10).unwrap());
        assert!(!catalog.add_to_album(japan, "nope", 10).unwrap());
        assert!(catalog.add_to_album(family, "a", 10).unwrap());
        assert_eq!(
            catalog.album_photos(&[japan]).unwrap(),
            [earlier.clone(), later.clone()]
        );
        assert!(catalog.album_photos(&[trips]).unwrap().is_empty());
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
        assert!(catalog.add_paths_to_album(Id::new(), &paths).is_err());
        assert!(
            catalog
                .add_paths_to_album(family, &["Originals/nope.jpg".into()])
                .is_err()
        );
        assert_eq!(catalog.album_photos(&[family]).unwrap().len(), 2);
        assert_eq!(catalog.albums_with(&earlier.path).unwrap(), [japan, family]);

        // Taking them out again.
        assert_eq!(
            catalog
                .remove_paths_from_albums(&[family], &paths[..1])
                .unwrap(),
            1
        );
        assert_eq!(
            catalog
                .remove_paths_from_albums(&[family], &paths[..1])
                .unwrap(),
            0
        );
        assert_eq!(catalog.albums_with(&earlier.path).unwrap(), [japan]);
        assert_eq!(catalog.add_paths_to_album(family, &paths).unwrap(), 1);

        // From several albums at once, each photo once.
        assert_eq!(catalog.album_photos(&[japan, family]).unwrap().len(), 2);
        assert_eq!(
            catalog.album_memberships().unwrap(),
            [
                (japan, later.path.clone()),
                (japan, earlier.path.clone()),
                (family, later.path.clone()),
                (family, earlier.path.clone()),
            ]
        );

        // Moved, and shown with the albums in it.
        catalog.move_album(japan, None).unwrap();
        catalog.set_album_show_nested(trips, true).unwrap();
        let albums = catalog.albums().unwrap();
        assert_eq!(albums[1].parent, None);
        assert!(albums[2].show_nested);
        catalog.move_album(japan, Some(trips)).unwrap();

        catalog.rename_album(family, "Family").unwrap();
        catalog.delete_albums(&[trips, japan]).unwrap();
        assert_eq!(
            catalog.albums().unwrap(),
            [album(family, None, "Family", 2)]
        );
        // The photos stay in the library.
        assert_eq!(catalog.photo_count().unwrap(), 2);
        assert!(catalog.album_photos(&[japan]).unwrap().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn keeps_objects_found_before_detectors_were_kept_apart() {
        let path =
            std::env::temp_dir().join(format!("1k-words-catalog-v6-{}.db", std::process::id()));
        std::fs::remove_file(&path).ok();
        // A catalog as version 6 left it, with a photo the built-in model
        // looked at.
        block_on(async {
            let database = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .unwrap();
            let connection = database.connect().unwrap();
            let schemas = [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6,
            ];
            connection
                .execute_batch(&format!(
                    "BEGIN; {} PRAGMA user_version = 6; COMMIT;",
                    schemas.concat()
                ))
                .await
                .unwrap();
            connection
                .execute_batch(
                    "INSERT INTO photos (id, path, content_hash, file_size, original_path,
                        imported_at, width, height, modified_at)
                        VALUES (7, 'Originals/a.jpg', 'h', 1, 'a.jpg', 0, 1, 1, 0);
                    INSERT INTO object_scans (photo_id, model, scanned_at)
                        VALUES (7, 'yolo11x', 0);
                    INSERT INTO objects (photo_id, label, confidence, x1, y1, x2, y2)
                        VALUES (7, 'dog', 0.5, 0.1, 0.2, 0.3, 0.4);",
                )
                .await
                .unwrap();
        });
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        assert_eq!(
            catalog.objects("Originals/a.jpg", "yolo11x").unwrap(),
            Some(vec![DetectedObject {
                label: "dog".into(),
                confidence: 0.5,
                bounds: Some([0.1, 0.2, 0.3, 0.4]),
            }])
        );
        assert_eq!(catalog.object_kinds("yolo11x").unwrap().len(), 1);
        assert!(catalog.object_scans("plugin:other").unwrap().is_empty());
        drop(catalog);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn converts_version_7_catalogs_keeping_everything() {
        let path =
            std::env::temp_dir().join(format!("1k-words-catalog-v7-{}.db", std::process::id()));
        std::fs::remove_file(&path).ok();
        // A catalog as version 7 left it: two photos, Kyoto inside Japan
        // with both in it, a thumbnail, an object, and a synced file.
        block_on(async {
            let database = turso::Builder::new_local(path.to_str().unwrap())
                .build()
                .await
                .unwrap();
            let connection = database.connect().unwrap();
            let schemas = [
                SCHEMA_V1, SCHEMA_V2, SCHEMA_V3, SCHEMA_V4, SCHEMA_V5, SCHEMA_V6, SCHEMA_V7,
            ];
            connection
                .execute_batch(&format!(
                    "BEGIN; {} PRAGMA user_version = 7; COMMIT;",
                    schemas.concat()
                ))
                .await
                .unwrap();
            connection
                .execute_batch(
                    "INSERT INTO photos (id, path, content_hash, file_size, original_path,
                        imported_at, width, height, modified_at, captured_at)
                        VALUES (3, 'Originals/b.jpg', 'hb', 1, 'b.jpg', 200, 1, 1, 0, 20),
                            (5, 'Originals/a.jpg', 'ha', 1, 'a.jpg', 100, 1, 1, 0, 10);
                    INSERT INTO albums (id, parent_id, name, created_at, show_nested)
                        VALUES (1, NULL, 'Japan', 50, 1), (2, 1, 'Kyoto', 60, 0);
                    INSERT INTO album_photos (album_id, photo_id, added_at)
                        VALUES (2, 3, 70), (2, 5, 71);
                    INSERT INTO derivatives (photo_id, kind, path, width, height, file_size,
                        version, created_at)
                        VALUES (5, 'thumbnail', 'Thumbnails/a.jpg', 560, 400, 99, 1, 80);
                    INSERT INTO detector_scans (photo_id, detector, scanned_at)
                        VALUES (5, 'yolo', 90);
                    INSERT INTO objects (photo_id, detector, label, confidence, x1, y1, x2, y2)
                        VALUES (5, 'yolo', 'dog', 0.5, 0.1, 0.2, 0.3, 0.4);
                    INSERT INTO sync_targets (id, target, folder, created_at)
                        VALUES (4, 'target-a', 'Photos', 95);
                    INSERT INTO sync_items (target_id, path, state, size, updated_at)
                        VALUES (4, 'Originals/a.jpg', 'synced', 1, 96);",
                )
                .await
                .unwrap();
        });

        let catalog = Catalog::open(&path, &test_device()).unwrap();
        let albums = catalog.albums().unwrap();
        let names: Vec<_> = albums.iter().map(|a| (a.name.as_str(), a.photos)).collect();
        assert_eq!(names, [("Japan", 0), ("Kyoto", 2)]);
        let (japan, kyoto) = (&albums[0], &albums[1]);
        assert_eq!(kyoto.parent, Some(japan.id));
        assert!(japan.show_nested);
        // Made from when they were made: Japan first.
        assert!(japan.id < kyoto.id);
        let paths =
            |photos: Vec<PhotoRecord>| photos.into_iter().map(|p| p.path).collect::<Vec<_>>();
        assert_eq!(
            paths(catalog.album_photos(&[kyoto.id]).unwrap()),
            ["Originals/a.jpg", "Originals/b.jpg"]
        );
        assert_eq!(catalog.albums_with("Originals/b.jpg").unwrap(), [kyoto.id]);
        assert_eq!(catalog.derivatives().unwrap()[0].path, "Thumbnails/a.jpg");
        assert_eq!(
            catalog.objects("Originals/a.jpg", "yolo").unwrap().unwrap()[0].label,
            "dog"
        );
        let targets = catalog.sync_targets().unwrap();
        assert_eq!(targets[0].target, "target-a");
        assert_eq!(catalog.sync_items(targets[0].id).unwrap().len(), 1);
        // It goes on working: albums can change, and objects be found again.
        catalog
            .add_paths_to_album(japan.id, &["Originals/a.jpg".into()])
            .unwrap();
        catalog.set_objects("Originals/a.jpg", "yolo", &[]).unwrap();
        drop(catalog);
        // And opens again without converting twice.
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        assert_eq!(catalog.albums().unwrap()[0].photos, 1);
        drop(catalog);
        std::fs::remove_file(&path).ok();
    }

    fn changed_elsewhere(catalog: &Catalog, album: Id, device: Id) {
        catalog.change_album_elsewhere(album, device, "Studio");
    }

    #[test]
    fn finds_albums_deleted_here_but_changed_elsewhere() {
        let path = std::env::temp_dir().join(format!(
            "1k-words-catalog-conflicts-{}.db",
            std::process::id()
        ));
        std::fs::remove_file(&path).ok();
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        let studio = Id::from_u128(2);
        let photo = PhotoRecord {
            path: "Originals/a.jpg".into(),
            ..Default::default()
        };
        catalog.insert(&photo).unwrap();
        let trips = catalog.create_album("Trips", None).unwrap();
        let japan = catalog.create_album("Japan", Some(trips)).unwrap();
        let kyoto = catalog.create_album("Kyoto", Some(japan)).unwrap();
        let peru = catalog.create_album("Peru", Some(trips)).unwrap();
        catalog
            .add_paths_to_album(kyoto, &[photo.path.clone()])
            .unwrap();

        // A change seen before deleting isn't a conflict.
        changed_elsewhere(&catalog, peru, studio);
        catalog.delete_albums(&[peru]).unwrap();
        assert!(catalog.album_conflicts().unwrap().is_empty());

        // Deleting Trips deletes what's in it. Studio, not having seen
        // that, adds a photo to Japan.
        catalog.delete_albums(&[trips, japan, kyoto]).unwrap();
        assert!(catalog.albums().unwrap().is_empty());
        assert!(catalog.album_photos(&[kyoto]).unwrap().is_empty());
        changed_elsewhere(&catalog, japan, studio);
        let conflicts = catalog.album_conflicts().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].album, japan);
        assert_eq!(conflicts[0].name, "Japan");
        assert_eq!(conflicts[0].deleted_on, "Test");
        assert_eq!(conflicts[0].changed_on, "Studio");

        // Keeping it brings back the album it's in, and the album in it
        // that went with it, with its photo; not Peru, deleted before.
        catalog.resolve_album_conflict(japan, true).unwrap();
        let names: Vec<_> = catalog
            .albums()
            .unwrap()
            .into_iter()
            .map(|a| a.name)
            .collect();
        assert_eq!(names, ["Japan", "Kyoto", "Trips"]);
        assert_eq!(catalog.album_photos(&[kyoto]).unwrap().len(), 1);
        assert!(catalog.album_conflicts().unwrap().is_empty());

        // Deleted again, then changed elsewhere, and this time let go.
        catalog.delete_albums(&[trips, japan, kyoto]).unwrap();
        changed_elsewhere(&catalog, kyoto, studio);
        assert_eq!(catalog.album_conflicts().unwrap()[0].album, kyoto);
        catalog.resolve_album_conflict(kyoto, false).unwrap();
        assert!(catalog.album_conflicts().unwrap().is_empty());
        assert!(catalog.albums().unwrap().is_empty());

        // Once they're old enough, deleted albums go for good, with the
        // record of what was in them; one in conflict would stay.
        changed_elsewhere(&catalog, trips, studio);
        catalog
            .purge_deleted_albums(super::super::now() + 1.0)
            .unwrap();
        let remaining = |sql: &str| {
            block_on(async {
                let connection = catalog.connection();
                let mut rows = connection.query(sql, ()).await.unwrap();
                rows.next().await.unwrap().unwrap().get::<i64>(0).unwrap()
            })
        };
        assert_eq!(remaining("SELECT count(*) FROM albums"), 1);
        assert_eq!(remaining("SELECT count(*) FROM album_photos"), 0);
        assert_eq!(catalog.album_conflicts().unwrap()[0].album, trips);
        drop(catalog);
        std::fs::remove_file(&path).ok();
    }

    /// Converts a copy of a real catalog, checking nothing's lost. Set
    /// `CATALOG_COPY` to the copy's path (it's converted in place).
    #[test]
    fn converts_a_real_catalog() {
        let Some(path) = std::env::var_os("CATALOG_COPY") else {
            eprintln!("skipping: CATALOG_COPY not set");
            return;
        };
        let path = std::path::PathBuf::from(path);
        let tables = [
            "photos",
            "albums",
            "album_photos",
            "derivatives",
            "detector_scans",
            "objects",
            "sync_targets",
            "sync_items",
        ];
        let counts = || {
            block_on(async {
                let database = turso::Builder::new_local(path.to_str().unwrap())
                    .build()
                    .await
                    .unwrap();
                let connection = database.connect().unwrap();
                let mut counts = Vec::new();
                for table in tables {
                    let mut rows = connection
                        .query(&format!("SELECT count(*) FROM {table}"), ())
                        .await
                        .unwrap();
                    let row = rows.next().await.unwrap().unwrap();
                    counts.push((table, row.get::<i64>(0).unwrap()));
                }
                counts
            })
        };
        let before = counts();
        eprintln!("before: {before:?}");
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        eprintln!(
            "photos: {}, albums: {}",
            catalog.photo_count().unwrap(),
            catalog.albums().unwrap().len()
        );
        drop(catalog);
        let after = counts();
        eprintln!("after:  {after:?}");
        for ((table, was), (_, now)) in before.iter().zip(&after) {
            // Files only on a service, without a path, aren't kept.
            if *table != "sync_items" {
                assert_eq!(was, now, "{table}");
            }
        }

        // With `TURSO_SYNC_SERVER` too, it moves to a server, and another
        // device gets all of it.
        let dir = path.with_extension("sync-test");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("server")).unwrap();
        let Some(server) = SyncServer::start(&dir.join("server"), None) else {
            return;
        };
        let url = server.url("library");
        let every_count = |catalog: &Catalog| {
            block_on(async {
                let connection = catalog.connection();
                let mut counts = Vec::new();
                for table in TABLES {
                    let mut rows = connection
                        .query(&format!("SELECT count(*) FROM {table}"), ())
                        .await
                        .unwrap();
                    let row = rows.next().await.unwrap().unwrap();
                    counts.push((*table, row.get::<i64>(0).unwrap()));
                }
                counts
            })
        };
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        let here = every_count(&catalog);
        let started = std::time::Instant::now();
        catalog.sync(&url, None).unwrap();
        eprintln!("moved to the server in {:?}", started.elapsed());
        assert_eq!(every_count(&catalog), here);
        let other = Catalog::open(
            &dir.join("other.db"),
            &Device {
                id: Id::from_u128(2),
                name: "Other".into(),
            },
        )
        .unwrap();
        let started = std::time::Instant::now();
        other.sync(&url, None).unwrap();
        eprintln!("fetched in {:?}", started.elapsed());
        let there = every_count(&other);
        eprintln!("there: {there:?}");
        for ((table, was), (_, now)) in here.iter().zip(&there) {
            // The other device adds itself.
            let mine = i64::from(*table == "devices");
            assert_eq!(was + mine, *now, "{table}");
        }
        assert_eq!(other.albums().unwrap(), catalog.albums().unwrap());
        assert_eq!(other.photos().unwrap(), catalog.photos().unwrap());
        drop((catalog, other, server));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn records_each_devices_syncs() {
        let path =
            std::env::temp_dir().join(format!("1k-words-device-sync-{}.db", std::process::id()));
        remove_database(&path).ok();
        let other_device = Device {
            id: Id::from_u128(2),
            name: "Other".into(),
        };

        let catalog = Catalog::open(&path, &test_device()).unwrap();
        let target = catalog.add_sync_target("target-a", "Photos-1234").unwrap();
        catalog.record_device_sync(None, true, None, None).unwrap();
        catalog
            .record_device_sync(Some(target), true, None, Some(2))
            .unwrap();
        drop(catalog);

        // Another device's credentials fail. Its error is its own: this
        // device's uploads there carry on.
        let other = Catalog::open(&path, &other_device).unwrap();
        other
            .record_device_sync(Some(target), false, Some("Access denied."), Some(5))
            .unwrap();
        assert_eq!(
            other.sync_targets().unwrap()[0].error.as_deref(),
            Some("Access denied.")
        );
        drop(other);
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        assert_eq!(catalog.sync_targets().unwrap()[0].error, None);

        // Each device is listed, this one first, with how its syncs went.
        let devices = catalog.device_sync().unwrap();
        assert_eq!(devices.len(), 2);
        let (here, there) = (&devices[0], &devices[1]);
        assert!(here.this_device && !there.this_device);
        assert_eq!((here.name.as_str(), there.name.as_str()), ("Test", "Other"));
        let catalog_run = here.catalog.as_ref().unwrap();
        assert!(catalog_run.synced_at.is_some() && catalog_run.error.is_none());
        assert_eq!(here.uploads.len(), 1);
        assert_eq!(here.uploads[0].0, target);
        assert_eq!(here.uploads[0].1.pending, Some(2));
        assert_eq!(there.catalog, None);
        let (_, run) = &there.uploads[0];
        assert_eq!(run.error.as_deref(), Some("Access denied."));
        assert_eq!(run.pending, Some(5));
        assert_eq!(run.synced_at, None);

        // A failed try keeps when it last went through, and a success
        // clears the error.
        let synced_at = here.uploads[0].1.synced_at;
        catalog
            .record_device_sync(Some(target), false, Some("Nope."), Some(2))
            .unwrap();
        let run = catalog.device_sync().unwrap()[0].uploads[0].1.clone();
        assert_eq!(
            (run.synced_at, run.error.as_deref()),
            (synced_at, Some("Nope."))
        );
        catalog
            .record_device_sync(Some(target), true, None, Some(0))
            .unwrap();
        let run = catalog.device_sync().unwrap()[0].uploads[0].1.clone();
        assert_eq!((run.error, run.pending), (None, Some(0)));

        // Removing a target forgets how syncing to it went.
        catalog.remove_sync_target(target).unwrap();
        let devices = catalog.device_sync().unwrap();
        assert!(devices.iter().all(|device| device.uploads.is_empty()));
        drop((catalog, devices));
        remove_database(&path).ok();
    }

    #[test]
    fn records_syncs() {
        let path =
            std::env::temp_dir().join(format!("1k-words-catalog-sync-{}.db", std::process::id()));
        std::fs::remove_file(&path).ok();
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        let id = catalog.add_sync_target("target-a", "Photos-1234").unwrap();
        let other = catalog.add_sync_target("target-b", "Photos-1234").unwrap();
        // A target's added once.
        assert!(catalog.add_sync_target("target-a", "Again").is_err());
        catalog
            .record_device_sync(Some(id), false, Some("Access denied."), Some(3))
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

    #[test]
    fn lists_every_table_to_copy() {
        let path =
            std::env::temp_dir().join(format!("1k-words-catalog-tables-{}.db", std::process::id()));
        remove_database(&path).unwrap();
        let catalog = Catalog::open(&path, &test_device()).unwrap();
        let mut tables: Vec<String> = block_on(async {
            let connection = catalog.connection();
            let mut rows = connection
                .query(
                    "SELECT name FROM sqlite_schema WHERE type = 'table'
                    AND name NOT LIKE 'sqlite_%' AND name NOT LIKE 'turso_%'
                    AND name != 'catalog_info'",
                    (),
                )
                .await
                .unwrap();
            let mut tables = Vec::new();
            while let Some(row) = rows.next().await.unwrap() {
                tables.push(row.get::<String>(0).unwrap());
            }
            tables
        });
        tables.sort();
        let mut expected: Vec<&str> = TABLES.to_vec();
        expected.sort();
        assert_eq!(tables, expected);
        assert_eq!(
            block_on(schema_version(&catalog.connection())).unwrap(),
            SCHEMA_VERSION
        );
        drop(catalog);
        remove_database(&path).unwrap();
    }

    /// A turso-sync server, run from the binary `TURSO_SYNC_SERVER` names.
    struct SyncServer {
        process: std::process::Child,
        port: u16,
        data: PathBuf,
    }

    impl SyncServer {
        fn start(data: &Path, port: Option<u16>) -> Option<Self> {
            let binary = std::env::var_os("TURSO_SYNC_SERVER")?;
            let port = port.unwrap_or_else(|| {
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.local_addr().unwrap().port()
            });
            let process = std::process::Command::new(binary)
                .arg("serve")
                .arg("--data")
                .arg(data)
                .args([
                    "--listen",
                    &format!("127.0.0.1:{port}"),
                    "--admin-listen",
                    "off",
                ])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..100 {
                if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                    return Some(Self {
                        process,
                        port,
                        data: data.to_owned(),
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            panic!("the sync server didn't start");
        }

        fn url(&self, library: &str) -> String {
            format!("http://127.0.0.1:{}/db/{library}", self.port)
        }

        fn restart(mut self) -> Self {
            self.process.kill().unwrap();
            self.process.wait().unwrap();
            Self::start(&self.data.clone(), Some(self.port)).unwrap()
        }
    }

    impl Drop for SyncServer {
        fn drop(&mut self) {
            self.process.kill().ok();
            self.process.wait().ok();
        }
    }

    /// Two devices syncing a catalog through a server, with
    /// `TURSO_SYNC_SERVER=<path to turso-sync> cargo test syncs_between`.
    #[test]
    fn syncs_between_devices() {
        let dir =
            std::env::temp_dir().join(format!("1k-words-catalog-sync-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("server")).unwrap();
        let Some(server) = SyncServer::start(&dir.join("server"), None) else {
            eprintln!("skipped: TURSO_SYNC_SERVER isn't set");
            return;
        };
        let url = server.url("library");
        let (laptop, studio) = (
            test_device(),
            Device {
                id: Id::from_u128(2),
                name: "Studio".into(),
            },
        );
        let photo = |path: &str| PhotoRecord {
            path: path.into(),
            ..Default::default()
        };

        // The laptop has a library already, which moves to the server.
        let a_path = dir.join("laptop.db");
        let a = Catalog::open(&a_path, &laptop).unwrap();
        a.insert(&photo("Originals/a.jpg")).unwrap();
        let trips = a.create_album("Trips", None).unwrap();
        a.add_paths_to_album(trips, &["Originals/a.jpg".into()])
            .unwrap();
        assert_eq!(a.server_url(), None);
        assert!(a.sync(&url, None).unwrap());
        assert_eq!(a.server_url().as_deref(), Some(url.as_str()));
        assert!(with_suffix(&a_path, ".before-sync").exists());
        assert_eq!(a.photo_count().unwrap(), 1);

        // The studio's catalog of it was empty: it takes the server's.
        let b_path = dir.join("studio.db");
        let b = Catalog::open(&b_path, &studio).unwrap();
        assert!(b.sync(&url, None).unwrap());
        assert_eq!(b.photo_count().unwrap(), 1);
        assert_eq!(b.album_photos(&[trips]).unwrap().len(), 1);

        // Changes go both ways, and are only fetched once.
        b.rename_album(trips, "Travel").unwrap();
        b.insert(&photo("Originals/b.jpg")).unwrap();
        assert!(!b.sync(&url, None).unwrap());
        assert!(a.sync(&url, None).unwrap());
        assert!(!a.sync(&url, None).unwrap());
        assert_eq!(a.albums().unwrap()[0].name, "Travel");
        assert_eq!(a.photo_count().unwrap(), 2);

        // The laptop deletes the album while the studio, not having seen
        // that, adds to it: the laptop's asked which was meant.
        a.delete_albums(&[trips]).unwrap();
        b.add_paths_to_album(trips, &["Originals/b.jpg".into()])
            .unwrap();
        a.sync(&url, None).unwrap();
        b.sync(&url, None).unwrap();
        a.sync(&url, None).unwrap();
        let conflicts = a.album_conflicts().unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(
            (
                conflicts[0].deleted_on.as_str(),
                conflicts[0].changed_on.as_str()
            ),
            ("Test", "Studio")
        );
        a.resolve_album_conflict(trips, true).unwrap();
        a.sync(&url, None).unwrap();
        b.sync(&url, None).unwrap();
        assert_eq!(b.album_photos(&[trips]).unwrap().len(), 2);

        // Offline, catalogs open and change as usual, and sync once the
        // server's back.
        drop(server);
        drop(a);
        let a = Catalog::open(&a_path, &laptop).unwrap();
        assert_eq!(a.photo_count().unwrap(), 2);
        a.insert(&photo("Originals/c.jpg")).unwrap();
        assert!(a.sync(&url, None).is_err());
        let server = SyncServer::start(&dir.join("server"), Some(url_port(&url))).unwrap();
        a.sync(&url, None).unwrap();
        assert!(b.sync(&url, None).unwrap());
        assert_eq!(b.photo_count().unwrap(), 3);

        // Moving to another server, or the same one under another name,
        // copies the catalog there; moving back takes the server's copy.
        let server = server.restart();
        let other = server.url("library-2");
        assert!(b.sync(&other, None).unwrap());
        let c = Catalog::open(&dir.join("other.db"), &laptop).unwrap();
        c.sync(&other, None).unwrap();
        assert_eq!(c.photo_count().unwrap(), 3);
        b.sync(&url, None).unwrap();
        assert_eq!(b.server_url().as_deref(), Some(url.as_str()));
        assert_eq!(b.photo_count().unwrap(), 3);

        // A newer version of the app on the studio changes the schema:
        // the laptop's catalog can then only be read.
        block_on(b.connection().execute(
            "UPDATE catalog_info SET value = 10 WHERE name = 'schema_version'",
            (),
        ))
        .unwrap();
        b.sync(&url, None).unwrap();
        assert!(a.sync(&url, None).unwrap());
        assert!(a.is_newer());
        assert_eq!(a.photo_count().unwrap(), 3);
        assert!(a.insert(&photo("Originals/d.jpg")).is_err());
        drop(a);
        assert!(Catalog::open(&a_path, &laptop).unwrap().is_newer());
        drop((b, c, server));
        std::fs::remove_dir_all(&dir).ok();
    }

    fn url_port(url: &str) -> u16 {
        url.split(':')
            .nth(2)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap()
    }
}

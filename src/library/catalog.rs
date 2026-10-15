//! A library's catalog: a Turso database of its photos' files and metadata.
//!
//! Files are stored relative to the library folder, with `/` separators, so
//! a library can move. The metadata is what `photos::read_shots` extracts,
//! stored in columns for fast filtering. All of it can be read again from
//! the originals, so a lost catalog could be rebuilt.
//!
//! Turso's API is async but does its I/O synchronously, so each call blocks
//! on it; call these from a background thread.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context as _, Result, bail};
use futures::executor::block_on;
use turso::{Connection, Database, Row, Value};

use crate::location::Location;
use crate::photos::CaptureMeta;

/// Bumped with each change to the schema, which `migrate` brings old
/// catalogs up to.
const SCHEMA_VERSION: i64 = 1;

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
        let connection = self.connection();
        block_on(async {
            let mut rows = connection
                .query(
                    "SELECT path, raw_path, live_video_path, content_hash, file_size,
                        original_path, imported_at, width, height, captured_at,
                        modified_at, camera, exposure_time, f_number, iso,
                        exposure_bias, auto_bracket, manual_exposure, latitude,
                        longitude
                    FROM photos
                    ORDER BY coalesce(captured_at, modified_at), path",
                    (),
                )
                .await?;
            let mut photos = Vec::new();
            while let Some(row) = rows.next().await? {
                photos.push(record_from_row(&row)?);
            }
            Ok(photos)
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
    if version < 1 {
        connection
            .execute_batch(&format!(
                "BEGIN; {SCHEMA_V1} PRAGMA user_version = 1; COMMIT;"
            ))
            .await?;
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
        drop(catalog);

        // Reopening keeps the photos, oldest first.
        let catalog = Catalog::open(&path).unwrap();
        let photos = catalog.photos().unwrap();
        assert_eq!(photos, [older, record]);
        std::fs::remove_file(&path).ok();
    }
}

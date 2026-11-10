//! Derivatives: files the app makes from a photo's originals and keeps in
//! the library: thumbnails for the gallery, and viewer-sized previews and
//! Live Photo video posters to show once an original's no longer on the
//! device.
//!
//! Each kind has its own top-level folder, laid out like the originals,
//! with the original's file name and the derivative's extension added:
//!
//! ```text
//! Originals/2026/10/15/IMG_0001.HEIC
//! Thumbnails/2026/10/15/IMG_0001.HEIC.jpg
//! Previews/2026/10/15/IMG_0001.HEIC.jpg
//! Posters/2026/10/15/IMG_0001.MOV.jpg
//! ```
//!
//! The catalog records each one (see `catalog::DerivativeRecord`), apart
//! from the originals. Derivatives can always be made again from the
//! originals, so unlike them they aren't read-only. They sync like the
//! originals do, ahead of them, so that a device short on space can
//! eventually show a whole library from its thumbnails and fetch originals
//! only as they're wanted.

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use image::DynamicImage;
use image::codecs::jpeg::JpegEncoder;

use super::catalog::DerivativeRecord;
use super::protect::Unlocked;
use super::{Library, ORIGINALS_DIR};
use crate::decode;
use crate::photos::Photo;

/// JPEG quality for derivatives: they're small next to the originals, so
/// this costs little.
const JPEG_QUALITY: u8 = 85;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DerivativeKind {
    /// What the gallery shows, `decode::THUMBNAIL_WIDTH` wide.
    Thumbnail,
    /// What the viewer shows when the original isn't on the device, scaled
    /// to fit `decode::VIEWER_PREVIEW_SIZE`.
    Preview,
    /// The first frame of a Live Photo's video, the same size as a preview.
    /// Named after the video, and only Live Photos have one.
    Poster,
}

impl DerivativeKind {
    pub const ALL: [DerivativeKind; 3] = [
        DerivativeKind::Thumbnail,
        DerivativeKind::Preview,
        DerivativeKind::Poster,
    ];

    /// How the catalog refers to it.
    pub fn key(self) -> &'static str {
        match self {
            DerivativeKind::Thumbnail => "thumbnail",
            DerivativeKind::Preview => "preview",
            DerivativeKind::Poster => "poster",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.key() == key)
    }

    /// Its folder in the library.
    pub fn dir(self) -> &'static str {
        match self {
            DerivativeKind::Thumbnail => "Thumbnails",
            DerivativeKind::Preview => "Previews",
            DerivativeKind::Poster => "Posters",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            DerivativeKind::Thumbnail | DerivativeKind::Preview | DerivativeKind::Poster => "jpg",
        }
    }

    /// Bumped when derivatives of this kind are made differently, so ones
    /// made the old way are made again.
    pub fn version(self) -> u32 {
        match self {
            DerivativeKind::Thumbnail | DerivativeKind::Preview | DerivativeKind::Poster => 1,
        }
    }

    /// Where the derivative of the original at `original` (relative to the
    /// library, as the catalog stores it) goes. None for files that aren't
    /// among the originals.
    pub fn path_for(self, original: &str) -> Option<String> {
        let rest = original.strip_prefix(ORIGINALS_DIR)?.strip_prefix('/')?;
        Some(format!("{}/{rest}.{}", self.dir(), self.extension()))
    }

    /// The original it's made from, if the photo has one for this kind.
    fn source(self, photo: &Photo) -> Option<&Path> {
        match self {
            DerivativeKind::Thumbnail | DerivativeKind::Preview => Some(&photo.path),
            DerivativeKind::Poster => photo.live_video.as_deref(),
        }
    }

    /// Makes the photo's derivative of this kind. Blocks for as long as
    /// decoding takes.
    fn make(self, photo: &Photo) -> Result<DynamicImage> {
        match self {
            DerivativeKind::Thumbnail => decode::thumbnail_image(photo),
            DerivativeKind::Preview => decode::preview_image(photo),
            DerivativeKind::Poster => decode::poster_image(self.source(photo).context("no video")?),
        }
    }
}

impl Library {
    /// Makes the photo's thumbnail and keeps it in the library, returning
    /// it. If it can't be kept (say the library's disk is full), it's still
    /// returned. Blocks for as long as decoding the photo takes.
    pub fn make_thumbnail(&self, photo: &Photo) -> Result<DynamicImage> {
        let image = decode::thumbnail_image(photo)?;
        if let Err(error) = self.keep(DerivativeKind::Thumbnail, photo, &image) {
            eprintln!(
                "couldn't keep the thumbnail of {}: {error:#}",
                photo.path.display()
            );
        }
        Ok(image)
    }

    fn keep(&self, kind: DerivativeKind, photo: &Photo, image: &DynamicImage) -> Result<()> {
        let original = self.relative(&photo.path)?;
        let source = self.relative(kind.source(photo).context("nothing to make it from")?)?;
        let path = kind
            .path_for(&source)
            .with_context(|| format!("{source} isn't among the originals"))?;
        let file = self.info.path.join(&path);
        let dir = file.parent().context("no folder")?;
        self.create_derivative_dirs(kind, dir)?;

        // Written beside it then renamed, so a half-written file is never
        // taken for a derivative, and two writers don't mix.
        let partial = dir.join(format!(".{}.partial", uuid::Uuid::new_v4()));
        let write = || -> Result<u64> {
            let mut writer = BufWriter::new(File::create(&partial)?);
            JpegEncoder::new_with_quality(&mut writer, JPEG_QUALITY)
                .encode_image(&image.to_rgb8())?;
            let file = writer.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            Ok(file.metadata()?.len())
        };
        let size = match write() {
            Ok(size) => size,
            Err(error) => {
                std::fs::remove_file(&partial).ok();
                return Err(error);
            }
        };
        std::fs::rename(&partial, &file)?;
        self.catalog.set_derivative(&DerivativeRecord {
            photo_path: original,
            kind,
            path,
            width: image.width(),
            height: image.height(),
            file_size: size,
            version: kind.version(),
        })
    }

    /// Creates `dir` (in `kind`'s folder) and the folders above it. The
    /// library's own folder must be there: it may have been moved away.
    pub(super) fn create_derivative_dirs(&self, kind: DerivativeKind, dir: &Path) -> Result<()> {
        let root = &self.info.path;
        let base = root.join(kind.dir());
        if !root.is_dir() {
            bail!("{} isn't there", root.display());
        }
        if !base.is_dir() {
            let mut unlocked = Unlocked::new(self.info.read_only);
            unlocked.unlock(root)?;
            std::fs::create_dir(&base).or_else(|error| match base.is_dir() {
                true => Ok(()),
                false => Err(error),
            })?;
            unlocked.relock()?;
        }
        std::fs::create_dir_all(dir)?;
        Ok(())
    }

    /// Makes the thumbnails that are missing, or were made the old way,
    /// oldest photos first, returning how many it made. If it's already
    /// running, it's asked to look again when done instead, so photos
    /// imported meanwhile aren't missed. Blocks until done; it stops early
    /// if the library's folder goes (such as when it's moved).
    pub fn make_missing_thumbnails(&self) -> Result<usize> {
        self.thumbnails
            .run(|| self.make_missing(DerivativeKind::Thumbnail))
    }

    /// Makes the previews and posters that are missing, or were made the
    /// old way, as `make_missing_thumbnails` does its thumbnails. Previews
    /// usually need the full image decoded, so this takes much longer, and
    /// runs apart so that nothing has to wait for it.
    pub fn make_missing_previews(&self) -> Result<usize> {
        self.previews.run(|| {
            Ok(self.make_missing(DerivativeKind::Preview)?
                + self.make_missing(DerivativeKind::Poster)?)
        })
    }

    fn make_missing(&self, kind: DerivativeKind) -> Result<usize> {
        let mut made = 0;
        for photo in self.missing(kind)? {
            if !self.info.path.is_dir() {
                bail!("{} isn't there", self.info.path.display());
            }
            let image = match kind.make(&photo) {
                Ok(image) => image,
                // Such as a damaged file; it's shown as it is.
                Err(_) => continue,
            };
            self.keep(kind, &photo, &image)?;
            made += 1;
        }
        Ok(made)
    }

    fn missing(&self, kind: DerivativeKind) -> Result<Vec<Photo>> {
        let made: std::collections::HashMap<String, DerivativeRecord> = self
            .catalog
            .derivatives()?
            .into_iter()
            .filter(|record| record.kind == kind)
            .map(|record| (record.photo_path.clone(), record))
            .collect();
        Ok(self
            .photos()?
            .into_iter()
            .filter(|photo| kind.source(photo).is_some())
            .filter(|photo| {
                let Ok(original) = self.relative(&photo.path) else {
                    return false;
                };
                made.get(&original).is_none_or(|record| {
                    record.version < kind.version() || !self.info.path.join(&record.path).is_file()
                })
            })
            .collect())
    }

    /// `path`, a file in the library, as the catalog stores it.
    pub(super) fn relative(&self, path: &Path) -> Result<String> {
        super::import::relative_path(&self.info.path, path)
    }

    /// Where the library keeps a thumbnail for the original at `original`
    /// (relative to the library), whether or not it's been made.
    pub(super) fn thumbnail_file(&self, original: &str) -> Option<PathBuf> {
        DerivativeKind::Thumbnail
            .path_for(original)
            .map(|path| self.info.path.join(path))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::catalog::FileRole;
    use super::super::{LibraryInfo, test_support};
    use super::*;

    #[test]
    fn derivatives_mirror_the_originals() {
        let kind = DerivativeKind::Thumbnail;
        assert_eq!(
            kind.path_for("Originals/2026/10/15/IMG_0001.HEIC")
                .as_deref(),
            Some("Thumbnails/2026/10/15/IMG_0001.HEIC.jpg")
        );
        assert_eq!(kind.path_for("Elsewhere/IMG_0001.JPG"), None);
        assert_eq!(kind.path_for("OriginalsX/IMG_0001.JPG"), None);
        for kind in DerivativeKind::ALL {
            assert_eq!(DerivativeKind::from_key(kind.key()), Some(kind));
        }
        assert_eq!(
            DerivativeKind::Preview
                .path_for("Originals/2026/10/15/IMG_0001.HEIC")
                .as_deref(),
            Some("Previews/2026/10/15/IMG_0001.HEIC.jpg")
        );
        assert_eq!(
            DerivativeKind::Poster
                .path_for("Originals/2026/10/15/IMG_0001.MOV")
                .as_deref(),
            Some("Posters/2026/10/15/IMG_0001.MOV.jpg")
        );
    }

    #[test]
    fn makes_previews_and_posters() {
        let (config_dir, library_dir) = test_support::temp_dirs("previews");
        let root = config_dir.parent().unwrap().to_path_buf();
        let source = root.join("card");
        std::fs::create_dir_all(&source).unwrap();
        for (name, width, height) in [("IMG_0000", 3000, 2000), ("IMG_0001", 300, 200)] {
            image::RgbImage::from_pixel(width, height, image::Rgb([200, 90, 160]))
                .save(source.join(format!("{name}.JPG")))
                .unwrap();
        }
        // A Live Photo, and one whose video can't be read.
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/video");
        std::fs::copy(
            fixtures.join("bt709-limited.mov"),
            source.join("IMG_0000.MOV"),
        )
        .unwrap();
        std::fs::write(source.join("IMG_0001.MOV"), b"not a video").unwrap();
        let library = Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: "previews".into(),
                    name: "Test".into(),
                    path: library_dir.clone(),
                    read_only: true,
                },
                &config_dir,
            )
            .unwrap(),
        );
        library.import(&[source]).unwrap();

        assert_eq!(library.make_missing_previews().unwrap(), 3);
        let records = library.catalog.derivatives().unwrap();
        let of_kind = |kind| {
            records
                .iter()
                .filter(|record| record.kind == kind)
                .collect::<Vec<_>>()
        };

        // Scaled down to fit the viewer, but never up.
        let previews = of_kind(DerivativeKind::Preview);
        assert_eq!(previews.len(), 2);
        assert_eq!((previews[0].width, previews[0].height), (2048, 1365));
        assert_eq!((previews[1].width, previews[1].height), (300, 200));

        // Only the video that could be read has a poster, kept beside the
        // others under the video's name.
        let posters = of_kind(DerivativeKind::Poster);
        assert_eq!(posters.len(), 1);
        let poster = posters[0];
        assert_eq!(poster.photo_path, previews[0].photo_path);
        assert!(poster.path.starts_with("Posters/"));
        assert!(poster.path.ends_with("/IMG_0000.MOV.jpg"));

        for record in &records {
            let file = library_dir.join(&record.path);
            assert_eq!(std::fs::metadata(&file).unwrap().len(), record.file_size);
            let image = image::open(file).unwrap();
            assert_eq!(
                (image.width(), image.height()),
                (record.width, record.height)
            );
        }

        // Nothing's made twice, but one that's gone is made again. The
        // video that can't be read is tried again and still makes nothing.
        assert_eq!(library.make_missing_previews().unwrap(), 0);
        std::fs::remove_file(library_dir.join(&poster.path)).unwrap();
        assert_eq!(library.make_missing_previews().unwrap(), 1);
        assert_eq!(library.catalog.derivatives().unwrap().len(), 3);

        // They sync as the photo's files, like thumbnails.
        let files = library.catalog.files().unwrap();
        assert!(
            files
                .iter()
                .any(|file| file.role == FileRole::Derivative(DerivativeKind::Poster))
        );
        drop(library);
        test_support::remove(&library_dir);
        test_support::remove(&root);
    }

    #[test]
    fn makes_and_keeps_thumbnails() {
        let (config_dir, library_dir) = test_support::temp_dirs("thumbnails");
        let root = config_dir.parent().unwrap().to_path_buf();
        let source = root.join("card");
        std::fs::create_dir_all(&source).unwrap();
        for (ix, (width, height)) in [(1200, 800), (300, 200)].into_iter().enumerate() {
            image::RgbImage::from_pixel(width, height, image::Rgb([ix as u8 * 80, 90, 160]))
                .save(source.join(format!("IMG_{ix:04}.JPG")))
                .unwrap();
        }
        let library = Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: "thumbnails".into(),
                    name: "Test".into(),
                    path: library_dir.clone(),
                    read_only: true,
                },
                &config_dir,
            )
            .unwrap(),
        );
        library.import(&[source]).unwrap();
        let photos = library.photos().unwrap();
        assert!(
            photos
                .iter()
                .all(|photo| !photo.thumbnail.as_ref().unwrap().exists())
        );

        assert_eq!(library.make_missing_thumbnails().unwrap(), 2);
        // They're catalogued as derivatives of their photos, not originals.
        let records = library.catalog.derivatives().unwrap();
        assert_eq!(records.len(), 2);
        for (record, photo) in records.iter().zip(&photos) {
            assert_eq!(record.kind, DerivativeKind::Thumbnail);
            assert_eq!(library.info.path.join(&record.photo_path), photo.path);
            let file = photo.thumbnail.as_ref().unwrap();
            assert_eq!(library.info.path.join(&record.path), *file);
            assert_eq!(std::fs::metadata(file).unwrap().len(), record.file_size);
            let image = image::open(file).unwrap();
            assert_eq!(
                (image.width(), image.height()),
                (record.width, record.height)
            );
        }
        // Scaled down to the thumbnail width, but never up.
        assert_eq!((records[0].width, records[0].height), (560, 373));
        assert_eq!((records[1].width, records[1].height), (300, 200));

        // Nothing's made twice, but a thumbnail that's gone is made again.
        assert_eq!(library.make_missing_thumbnails().unwrap(), 0);
        std::fs::remove_file(photos[0].thumbnail.as_ref().unwrap()).unwrap();
        assert_eq!(library.make_missing_thumbnails().unwrap(), 1);
        assert_eq!(library.catalog.derivatives().unwrap().len(), 2);

        // Making one for the gallery keeps it too, folders and all.
        super::super::protect::set_writable(&library_dir, true).unwrap();
        std::fs::remove_dir_all(library_dir.join("Thumbnails")).unwrap();
        super::super::protect::set_writable(&library_dir, false).unwrap();
        let image = library.make_thumbnail(&photos[1]).unwrap();
        assert_eq!(image.width(), 300);
        assert!(photos[1].thumbnail.as_ref().unwrap().is_file());

        // Once the library's moved away, it stops.
        drop(library);
        test_support::remove(&library_dir);
        let library = Library::create_or_open(
            LibraryInfo {
                id: "thumbnails".into(),
                name: "Test".into(),
                path: library_dir.clone(),
                read_only: true,
            },
            &config_dir,
        )
        .unwrap();
        test_support::remove(&library_dir);
        assert!(library.make_missing_thumbnails().is_err());
        assert!(!library_dir.exists());
        test_support::remove(&root);
    }
}

//! Importing: copying photos into a library and adding them to its catalog.
//!
//! Each file is copied into a staging folder in the library, hashed as it's
//! copied, then moved into `Originals/YYYY/MM/DD` (by the day it was taken)
//! once it's complete, so a failed or interrupted import never leaves a
//! partial file among the originals. Photos already in the library (by
//! content) are skipped.

use std::fs::File;
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context as _, Result};

use super::albums::AlbumId;
use super::catalog::PhotoRecord;
use super::protect::{self, Unlocked};
use super::{Library, ORIGINALS_DIR, now};
use crate::location::Location;
use crate::photos::{self, Photo};

pub(super) const STAGING_DIR: &str = ".import";
const COPY_BUFFER_SIZE: usize = 1 << 20;

/// The shots found for an import, ready to queue.
#[derive(Debug, Default)]
pub struct PreparedImport {
    /// In the order they were taken.
    pub shots: Vec<Photo>,
    /// Files that couldn't be read, and why.
    pub failed: Vec<(PathBuf, String)>,
}

/// Groups `images` into shots and reads their metadata, ready to import.
pub fn prepare(images: Vec<PathBuf>) -> PreparedImport {
    let mut prepared = PreparedImport::default();
    for result in photos::read_shot_results(images) {
        match result {
            Ok(shot) => prepared.shots.push(shot),
            Err((path, error)) => prepared.failed.push((path, format!("{error:#}"))),
        }
    }
    prepared.shots.sort_by(|a, b| {
        let time = |p: &Photo| p.meta.captured_at.unwrap_or(p.meta.modified_at);
        time(a)
            .total_cmp(&time(b))
            .then_with(|| a.path.cmp(&b.path))
    });
    prepared
}

/// What became of a shot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Imported {
    Added,
    /// The library already had it, so it was skipped.
    Duplicate,
}

#[derive(Debug, Default)]
pub struct ImportSummary {
    pub imported: usize,
    /// Shots skipped because the library already has them.
    pub duplicates: usize,
    /// Files that couldn't be imported, and why.
    pub failed: Vec<(PathBuf, String)>,
}

impl ImportSummary {
    pub fn record(&mut self, path: &Path, result: Result<Imported>) {
        match result {
            Ok(Imported::Added) => self.imported += 1,
            Ok(Imported::Duplicate) => self.duplicates += 1,
            Err(error) => self.failed.push((path.to_path_buf(), format!("{error:#}"))),
        }
    }
}

/// Imports `images` start to finish.
#[cfg(test)]
pub(super) fn import_all(
    library: &Arc<Library>,
    images: Vec<PathBuf>,
    album: Option<AlbumId>,
) -> Result<ImportSummary> {
    let prepared = prepare(images);
    let mut summary = ImportSummary {
        failed: prepared.failed,
        ..Default::default()
    };
    if prepared.shots.is_empty() {
        return Ok(summary);
    }
    let mut session = ImportSession::new(library.clone())?;
    for shot in &prepared.shots {
        summary.record(&shot.path, session.import(shot, album));
    }
    if let Err(error) = session.finish() {
        summary
            .failed
            .push((library.info.path.clone(), format!("{error:#}")));
    }
    Ok(summary)
}

/// A file copied into the staging folder.
struct Staged {
    path: PathBuf,
    /// BLAKE3, in hex.
    hash: String,
    size: u64,
}

/// Imports shots into a library one at a time; `finish` when done.
///
/// Several sessions can import into the same library at once (one per
/// source drive). Each copies and hashes into its own staging folder in
/// parallel with the others; placing a shot among the originals (checking
/// for duplicates, choosing its name, moving it in and cataloguing it) is
/// done under the library's lock, one shot at a time. Folders are only
/// writable while a shot is being placed in them.
pub struct ImportSession {
    library: Arc<Library>,
    /// This session's own folder in the library's staging folder.
    staging: PathBuf,
    staged: usize,
    /// The first failure to make folders read-only again.
    relock_error: Option<std::io::Error>,
    /// Whether photos were added to an album, so the copy of the albums
    /// is written when it's finished.
    added_to_album: bool,
}

impl ImportSession {
    pub fn new(library: Arc<Library>) -> Result<Self> {
        let root = library.info.path.clone();
        let staging_root = root.join(STAGING_DIR);
        let staging = staging_root.join(uuid::Uuid::new_v4().to_string());
        {
            let _placing = library.placing();
            let mut unlocked = Unlocked::new(library.info.read_only);
            unlocked.unlock(&root)?;
            std::fs::create_dir_all(&staging)
                .with_context(|| format!("creating {}", staging.display()))?;
            unlocked.relock()?;
        }
        Ok(Self {
            library,
            staging,
            staged: 0,
            relock_error: None,
            added_to_album: false,
        })
    }

    /// Copies a shot into the library, and adds it to `album` if given,
    /// even if the library already had it.
    pub fn import(&mut self, shot: &Photo, album: Option<AlbumId>) -> Result<Imported> {
        let display = self.stage(&shot.path)?;
        let (hash, size) = (display.hash.clone(), display.size);
        let result = self.import_staged(shot, display);
        if let (Ok(_), Some(album)) = (&result, album) {
            self.library
                .catalog
                .add_to_album(album, &hash, size)
                .context("adding it to the album")?;
            self.added_to_album = true;
        }
        result
    }

    fn import_staged(&mut self, shot: &Photo, display: Staged) -> Result<Imported> {
        // Skip copying the rest of a shot that's already in the library. This
        // is checked again when placing it, in case another session adds it
        // meanwhile.
        if self.library.catalog.contains(&display.hash, display.size)? {
            std::fs::remove_file(&display.path).ok();
            return Ok(Imported::Duplicate);
        }
        let raw = shot
            .raw_pair
            .as_deref()
            .map(|p| self.stage(p))
            .transpose()?;
        let video = shot
            .live_video
            .as_deref()
            .map(|p| self.stage(p))
            .transpose()?;
        let images = [
            Some(display.path.as_path()),
            raw.as_ref().map(|r| r.path.as_path()),
        ];
        let location = Location::find(
            images.into_iter().flatten(),
            video.as_ref().map(|v| v.path.as_path()),
        );

        let staged = StagedShot {
            display,
            raw,
            video,
            location,
        };
        let _placing = self.library.placing();
        let mut unlocked = Unlocked::new(self.library.info.read_only);
        let result = self.place(shot, &staged, &mut unlocked);
        if let Err(error) = unlocked.relock() {
            self.relock_error.get_or_insert(error);
        }
        if !matches!(result, Ok(Imported::Added)) {
            for file in staged.files() {
                std::fs::remove_file(&file.path).ok();
            }
        }
        result
    }

    /// Removes this session's staging folder, and reports if any folder
    /// couldn't be made read-only again.
    pub fn finish(mut self) -> Result<()> {
        if self.added_to_album {
            self.library.save_albums();
        }
        std::fs::remove_dir_all(&self.staging).ok();
        {
            let _placing = self.library.placing();
            let root = self.library.info.path.clone();
            let mut unlocked = Unlocked::new(self.library.info.read_only);
            unlocked.unlock(&root)?;
            // Only once no other session is using it.
            std::fs::remove_dir(root.join(STAGING_DIR)).ok();
            unlocked.relock()?;
        }
        match self.relock_error.take() {
            Some(error) => Err(error).context("couldn't make the library read-only again"),
            None => Ok(()),
        }
    }

    /// Moves a staged shot into the originals and catalogues it. Call with
    /// the library's lock held.
    fn place(
        &self,
        shot: &Photo,
        staged: &StagedShot,
        unlocked: &mut Unlocked,
    ) -> Result<Imported> {
        let display = &staged.display;
        if self.library.catalog.contains(&display.hash, display.size)? {
            return Ok(Imported::Duplicate);
        }

        let (year, month, day) = shot.meta.date();
        let originals = self.library.info.path.join(ORIGINALS_DIR);
        let dir = unlocked.create_dirs(
            &originals,
            &[
                &format!("{year:04}"),
                &format!("{month:02}"),
                &format!("{day:02}"),
            ],
        )?;

        // All of a shot's files keep the same name, apart from the extension.
        let sources: Vec<&Path> = [
            Some(shot.path.as_path()),
            shot.raw_pair.as_deref(),
            shot.live_video.as_deref(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let stem = shot
            .path
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "photo".into());
        let extensions: Vec<String> = sources.iter().map(|path| extension(path)).collect();
        let stem = unique_stem(&dir, &stem, &extensions);

        let mut placed = Vec::new();
        let result = (|| {
            let mut place = |file: &Staged, extension: &str| -> Result<PathBuf> {
                let file_name = if extension.is_empty() {
                    stem.clone()
                } else {
                    format!("{stem}.{extension}")
                };
                let dest = dir.join(file_name);
                if self.library.info.read_only {
                    protect::set_writable(&file.path, false)?;
                }
                std::fs::rename(&file.path, &dest)
                    .with_context(|| format!("moving into {}", dest.display()))?;
                placed.push(dest.clone());
                Ok(dest)
            };
            let display_dest = place(display, &extensions[0])?;
            let mut next_extension = extensions.iter().skip(1);
            let raw_dest = staged
                .raw
                .as_ref()
                .map(|raw| place(raw, next_extension.next().unwrap()))
                .transpose()?;
            let video_dest = staged
                .video
                .as_ref()
                .map(|video| place(video, next_extension.next().unwrap()))
                .transpose()?;

            let root = &self.library.info.path;
            let relative = |path: &Path| relative_path(root, path);
            self.library.catalog.insert(&PhotoRecord {
                path: relative(&display_dest)?,
                raw_path: raw_dest.as_deref().map(relative).transpose()?,
                live_video_path: video_dest.as_deref().map(relative).transpose()?,
                content_hash: display.hash.clone(),
                file_size: display.size,
                original_path: shot.path.to_string_lossy().into_owned(),
                imported_at: now(),
                width: shot.width,
                height: shot.height,
                meta: shot.meta.clone(),
                location: staged.location,
            })
        })();
        if let Err(error) = result {
            // Don't leave files the catalog doesn't know about.
            for path in placed {
                protect::set_writable(&path, true).ok();
                std::fs::remove_file(&path).ok();
            }
            return Err(error);
        }
        Ok(Imported::Added)
    }

    /// Copies `source` into the staging folder, hashing it on the way, and
    /// keeps its modification time and extension (which some metadata
    /// readers go by).
    fn stage(&mut self, source: &Path) -> Result<Staged> {
        self.staged += 1;
        let mut name = self.staged.to_string();
        if let Some(extension) = source.extension() {
            name.push('.');
            name.push_str(&extension.to_string_lossy());
        }
        let path = self.staging.join(name);
        let copy = || -> std::io::Result<Staged> {
            let mut reader = File::open(source)?;
            let mut writer = File::create(&path)?;
            let mut hasher = blake3::Hasher::new();
            let mut buffer = vec![0; COPY_BUFFER_SIZE];
            let mut size = 0;
            loop {
                let read = reader.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
                writer.write_all(&buffer[..read])?;
                size += read as u64;
            }
            if let Ok(modified) = reader.metadata().and_then(|m| m.modified()) {
                writer.set_modified(modified)?;
            }
            writer.sync_all()?;
            Ok(Staged {
                path: path.clone(),
                hash: hasher.finalize().to_hex().to_string(),
                size,
            })
        };
        copy().with_context(|| format!("copying {}", source.display()))
    }
}

/// A shot's files, copied into staging, and its location.
struct StagedShot {
    display: Staged,
    raw: Option<Staged>,
    video: Option<Staged>,
    location: Option<Location>,
}

impl StagedShot {
    fn files(&self) -> impl Iterator<Item = &Staged> {
        [Some(&self.display), self.raw.as_ref(), self.video.as_ref()]
            .into_iter()
            .flatten()
    }
}

/// Which drive `path` is on, so imports from different drives can run at
/// the same time while imports from the same drive take turns.
pub fn source_drive(path: &Path) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if let Ok(metadata) = std::fs::metadata(path) {
            return metadata.dev();
        }
    }
    // Elsewhere (and if that fails), go by the path's prefix, e.g. `E:` on
    // Windows.
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.components().next().hash(&mut hasher);
    hasher.finish()
}

fn extension(path: &Path) -> String {
    path.extension()
        .map(|ext| ext.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// `stem`, or `stem (2)`, `stem (3)`… — the first that no file in `dir`
/// has with any of `extensions`.
fn unique_stem(dir: &Path, stem: &str, extensions: &[String]) -> String {
    let taken = |candidate: &str| {
        extensions.iter().any(|extension| {
            let name = if extension.is_empty() {
                candidate.to_owned()
            } else {
                format!("{candidate}.{extension}")
            };
            dir.join(name).exists()
        })
    };
    if !taken(stem) {
        return stem.to_owned();
    }
    (2..)
        .map(|n| format!("{stem} ({n})"))
        .find(|candidate| !taken(candidate))
        .unwrap()
}

/// `path` relative to `root`, with `/` separators on every platform.
pub(super) fn relative_path(root: &Path, path: &Path) -> Result<String> {
    let relative = path
        .strip_prefix(root)
        .with_context(|| format!("{} isn't in the library", path.display()))?;
    let components: Vec<_> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    Ok(components.join("/"))
}

#[cfg(test)]
mod tests {
    use super::super::test_support;
    use super::*;

    /// Writes a small JPEG with distinct pixels, so its content is unique.
    fn jpeg(path: &Path, shade: u8) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([shade, 90, 160]))
            .save(path)
            .unwrap();
    }

    fn files_under(dir: &Path) -> Vec<String> {
        let mut files = Vec::new();
        for path in photos::images_in(&[dir.to_path_buf()]) {
            files.push(relative_path(dir, &path).unwrap());
        }
        files.sort();
        files
    }

    #[gpui_kit::test]
    fn imports_copies_and_skips_duplicates(cx: &mut gpui_kit::TestAppContext) {
        let (config_dir, default_dir) = test_support::temp_dirs("import");
        let source = config_dir.parent().unwrap().join("card");
        jpeg(&source.join("100TEST/IMG_0001.JPG"), 10);
        jpeg(&source.join("101TEST/IMG_0001.JPG"), 20);
        jpeg(&source.join("101TEST/IMG_0002.JPG"), 30);
        // The same photo again, under another name.
        std::fs::copy(source.join("101TEST/IMG_0002.JPG"), source.join("copy.jpg")).unwrap();
        std::fs::write(source.join("broken.jpg"), b"not a jpeg").unwrap();

        cx.update(|cx| super::super::init_at(config_dir.clone(), default_dir.clone(), cx));
        let library = cx.update(|cx| super::super::current(cx)).unwrap();

        let summary = library.import(std::slice::from_ref(&source)).unwrap();
        assert_eq!(summary.imported, 3);
        assert_eq!(summary.duplicates, 1);
        assert_eq!(summary.failed.len(), 1, "{:?}", summary.failed);

        // Without EXIF dates they're filed by modification day; the two
        // IMG_0001s get distinct names.
        let (y, m, d) = photos::read_shots(vec![source.join("copy.jpg")])[0]
            .meta
            .date();
        let day = format!("Originals/{y:04}/{m:02}/{d:02}");
        assert_eq!(
            files_under(&default_dir),
            [
                format!("{day}/IMG_0001 (2).JPG"),
                format!("{day}/IMG_0001.JPG"),
                format!("{day}/IMG_0002.JPG"),
            ]
        );
        assert!(!default_dir.join(STAGING_DIR).exists());

        let photos = library.photos().unwrap();
        assert_eq!(photos.len(), 3);
        assert!(
            photos
                .iter()
                .all(|photo| photo.path.starts_with(&default_dir))
        );

        // The originals are read-only, and so are their folders.
        let original = &photos[0].path;
        assert!(
            std::fs::metadata(original)
                .unwrap()
                .permissions()
                .readonly()
        );
        assert!(
            std::fs::metadata(original.parent().unwrap())
                .unwrap()
                .permissions()
                .readonly()
        );

        // Importing again adds nothing.
        let again = library.import(std::slice::from_ref(&source)).unwrap();
        assert_eq!((again.imported, again.duplicates), (0, 4));

        test_support::remove(config_dir.parent().unwrap());
    }

    /// Two sessions importing at once, as from two drives: same-named shots
    /// get distinct names, and a shot on both drives is only added once.
    #[gpui_kit::test]
    fn concurrent_sessions_take_turns_placing(cx: &mut gpui_kit::TestAppContext) {
        let (config_dir, default_dir) = test_support::temp_dirs("concurrent");
        let root = config_dir.parent().unwrap().to_path_buf();
        const SHOTS: u8 = 20;
        // PNGs, as solid JPEGs of nearby shades can compress to the same
        // bytes.
        let png = |path: PathBuf, shade: u8| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            image::RgbImage::from_pixel(16, 16, image::Rgb([shade, 90, 160]))
                .save(path)
                .unwrap();
        };
        for (card, offset) in [("card-a", 0), ("card-b", 100)] {
            for ix in 0..SHOTS {
                png(root.join(format!("{card}/IMG_{ix:04}.PNG")), offset + ix);
            }
            // The same photo on both cards.
            png(root.join(format!("{card}/shared.png")), 250);
        }
        cx.update(|cx| super::super::init_at(config_dir.clone(), default_dir.clone(), cx));
        let library = cx.update(|cx| super::super::current(cx)).unwrap();

        let barrier = std::sync::Barrier::new(2);
        let summaries: Vec<ImportSummary> = std::thread::scope(|scope| {
            let workers: Vec<_> = ["card-a", "card-b"]
                .into_iter()
                .map(|card| {
                    let (library, barrier, source) = (&library, &barrier, root.join(card));
                    scope.spawn(move || {
                        let prepared = prepare(photos::images_in(&[source]));
                        let mut session = ImportSession::new(library.clone()).unwrap();
                        barrier.wait();
                        let mut summary = ImportSummary::default();
                        for shot in &prepared.shots {
                            summary.record(&shot.path, session.import(shot, None));
                        }
                        session.finish().unwrap();
                        summary
                    })
                })
                .collect();
            workers.into_iter().map(|w| w.join().unwrap()).collect()
        });

        let failed: Vec<_> = summaries.iter().flat_map(|s| &s.failed).collect();
        assert!(failed.is_empty(), "{failed:?}");
        let imported: usize = summaries.iter().map(|s| s.imported).sum();
        let duplicates: usize = summaries.iter().map(|s| s.duplicates).sum();
        let expected = 2 * SHOTS as usize + 1;
        assert_eq!((imported, duplicates), (expected, 1));

        // Every catalogued file exists, and none was overwritten.
        let photos = library.photos().unwrap();
        assert_eq!(photos.len(), expected);
        let mut paths: Vec<_> = photos.iter().map(|p| p.path.clone()).collect();
        paths.sort();
        paths.dedup();
        assert_eq!(paths.len(), expected);
        assert_eq!(files_under(&default_dir).len(), expected);
        assert!(!default_dir.join(STAGING_DIR).exists());

        test_support::remove(&root);
    }

    #[test]
    fn unique_stems_cover_every_extension() {
        let dir = std::env::temp_dir().join(format!("1k-words-stems-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("IMG_0001.CR3"), b"").unwrap();
        let extensions = ["JPG".to_owned(), "CR3".to_owned()];
        // The JPEG name is free, but its raw pair's isn't.
        assert_eq!(unique_stem(&dir, "IMG_0001", &extensions), "IMG_0001 (2)");
        assert_eq!(unique_stem(&dir, "IMG_0002", &extensions), "IMG_0002");
        std::fs::remove_dir_all(&dir).ok();
    }
}

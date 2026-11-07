//! Downloading a library's files from a sync target (see `sync`).
//!
//! A library's derivatives are all downloaded, so it can be shown whole; its
//! originals are downloaded only when they're asked for: when a photo that's
//! been evicted is opened, or pinned (see `on_device`). Those opened join the
//! recently fetched pool, which is kept to a budget by evicting what was
//! wanted longest ago. Pinned photos are never evicted.
//!
//! Each file is downloaded into a staging folder in the library and checked
//! against what the catalog knows of it, then moved into place, so a failed
//! or interrupted download never leaves a partial file among the library's.
//!
//! Like the rest of the catalog, this blocks; call it from a background
//! thread.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use anyhow::{Context as _, Result, anyhow, bail};

use super::catalog::{FileRole, SyncItemRecord, SyncState};
use super::id::Id;
use super::import::STAGING_DIR;
use super::on_device::OriginalSource;
use super::protect::{self, Unlocked};
use super::sync::{MAX_FAILURES_IN_A_ROW, TargetSettings};
use super::{Library, ORIGINALS_DIR};
use crate::photos::Photo;
use crate::plugins::{Capabilities, Plugin, Plugins, RemoteItem, Session, SyncError};

/// How many files are downloaded at once.
const DOWNLOAD_WORKERS: usize = 4;
/// How much the recently fetched pool may hold, in each library.
const FETCHED_POOL_BUDGET: u64 = 4 << 30;

/// A target originals can be downloaded from.
#[derive(Clone, Debug)]
pub struct DownloadSource {
    /// The library's id for it.
    pub id: Id,
    pub settings: TargetSettings,
}

#[derive(Debug, Default)]
pub struct DownloadSummary {
    pub downloaded: usize,
    /// Files that failed, and why.
    pub failed: Vec<(String, String)>,
    /// Why the download stopped before the end, if it did.
    pub stopped: Option<String>,
}

impl DownloadSummary {
    /// Adds how another download went.
    pub fn add(&mut self, other: DownloadSummary) {
        self.downloaded += other.downloaded;
        self.failed.extend(other.failed);
        self.stopped = self.stopped.take().or(other.stopped);
    }
}

/// A file to download.
#[derive(Clone, Debug)]
pub(super) struct Wanted {
    /// In the library.
    pub path: String,
    pub role: FileRole,
    pub remote: RemoteItem,
    /// BLAKE3 of the file, if the catalog knows it.
    pub hash: Option<String>,
}

impl Wanted {
    /// What's needed to download the file at `path`, if it's on the target.
    pub fn new(folder: &str, path: &str, role: FileRole, item: &SyncItemRecord) -> Option<Self> {
        if item.state != SyncState::Synced {
            return None;
        }
        Some(Self {
            path: path.to_owned(),
            role,
            remote: RemoteItem {
                key: item.remote_key.clone()?,
                path: Some(format!("{folder}/{path}")),
                version: item.remote_version.clone(),
                size: item.size,
            },
            hash: None,
        })
    }
}

/// Why a file's download failed.
enum Failure {
    /// The target's settings are wrong: there's no use trying more.
    Config(String),
    /// Nothing wrong with the service, such as a file that's gone from it.
    File(String),
    /// Counted, as a run of these means the service or network is down.
    Transient(String),
}

impl Library {
    /// Downloads the derivatives that are missing from the target (`id`
    /// being the library's id for it), until done or `cancel` is set.
    /// Failures are retried next time; the download stops early if the
    /// target's settings are wrong, or too many files fail in a row. Does
    /// nothing if the target's plugin only uploads.
    pub fn download_derivatives(
        &self,
        plugins: &Plugins,
        id: Id,
        settings: &TargetSettings,
        cancel: &AtomicBool,
    ) -> Result<DownloadSummary> {
        let target = self.sync_target(id)?;
        let items = self.sync_items(id)?;
        let wanted: Vec<Wanted> = self
            .catalog
            .files()?
            .into_iter()
            .filter(|file| matches!(file.role, FileRole::Derivative(_)))
            .filter(|file| !self.info.path.join(&file.path).is_file())
            .filter_map(|file| {
                let item = items.get(&file.path)?;
                Wanted::new(&target.folder, &file.path, file.role, item)
            })
            .collect();
        let plugin = plugins
            .get(&settings.plugin)
            .filter(|plugin| plugin.info.capabilities.contains(Capabilities::DOWNLOAD));
        match plugin {
            Some(plugin) if !wanted.is_empty() => {
                self.download_files(plugin, settings, wanted, cancel)
            }
            _ => Ok(DownloadSummary::default()),
        }
    }

    /// Fetches whichever of a photo's originals aren't on this device from
    /// the first of `sources` that has them, then keeps the recently fetched
    /// pool to its budget. Does nothing if they're all here.
    pub fn fetch_original(
        &self,
        plugins: &Plugins,
        sources: &[DownloadSource],
        photo: &Photo,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let files: Vec<String> = photo
            .files()
            .filter(|file| !file.is_file())
            .map(|file| self.relative(file))
            .collect::<Result<_>>()?;
        if files.is_empty() {
            return Ok(());
        }
        let summary = self.download_originals(plugins, sources, files, cancel)?;
        if !photo.files().all(|file| file.is_file()) {
            match (summary.failed.last(), summary.stopped) {
                (Some((_, error)), _) => bail!("{error}"),
                (None, Some(stopped)) => bail!("{stopped}"),
                (None, None) => bail!("It isn't on a sync target that can download it."),
            }
        }
        let keep = photo
            .files()
            .map(|file| self.relative(file))
            .collect::<Result<_>>()?;
        self.trim_fetched(FETCHED_POOL_BUDGET, &keep)
    }

    /// Downloads what pinned photos and albums need that isn't here, such
    /// as photos added to a pinned album since. Stays out of the recently
    /// fetched pool's budget: pinned originals can't be evicted to meet it.
    pub fn download_pinned(
        &self,
        plugins: &Plugins,
        sources: &[DownloadSource],
        cancel: &AtomicBool,
    ) -> Result<DownloadSummary> {
        // An unplugged drive looks like every original's missing.
        if !self.info.path.join(super::MARKER_FILE).is_file() {
            bail!("{} isn't there", self.info.path.display());
        }
        let mut files: Vec<String> = self
            .pinned_originals()?
            .into_iter()
            .filter(|path| !self.info.path.join(path).is_file())
            .collect();
        files.sort();
        if files.is_empty() {
            return Ok(DownloadSummary::default());
        }
        self.download_originals(plugins, sources, files, cancel)
    }

    /// Downloads originals, by their paths in the library, trying each of
    /// `sources` in turn for those still to come. Files none of them has
    /// are left out of the summary: another target may have them.
    fn download_originals(
        &self,
        plugins: &Plugins,
        sources: &[DownloadSource],
        files: Vec<String>,
        cancel: &AtomicBool,
    ) -> Result<DownloadSummary> {
        let hashes: HashMap<String, String> = self
            .catalog
            .photos()?
            .into_iter()
            .map(|record| (record.path, record.content_hash))
            .collect();
        let mut left: Vec<String> = files;
        let mut summary = DownloadSummary::default();
        // Why each file failed, until another source has it.
        let mut errors: HashMap<String, String> = HashMap::new();
        for source in sources {
            if left.is_empty() || cancel.load(Ordering::Relaxed) {
                break;
            }
            let plugin = match download_plugin(plugins, &source.settings) {
                Ok(plugin) => plugin,
                Err(_) => continue,
            };
            let target = self.sync_target(source.id)?;
            let items = self.sync_items(source.id)?;
            let wanted: Vec<Wanted> = left
                .iter()
                .filter_map(|path| {
                    let item = items.get(path)?;
                    let mut file = Wanted::new(&target.folder, path, FileRole::Original, item)?;
                    file.hash = hashes.get(path).cloned();
                    Some(file)
                })
                .collect();
            if wanted.is_empty() {
                continue;
            }
            let from_here = self.download_files(plugin, &source.settings, wanted, cancel)?;
            summary.downloaded += from_here.downloaded;
            summary.stopped = from_here.stopped;
            errors.extend(from_here.failed);
            left.retain(|path| !self.info.path.join(path).is_file());
        }
        summary.failed = left
            .into_iter()
            .filter_map(|path| errors.remove_entry(&path))
            .collect();
        Ok(summary)
    }

    /// Removes an original from this device. It must be safely on a target,
    /// and not pinned: this is the only copy here, and may be the last that
    /// can be told apart from a lost file.
    pub fn evict_original(&self, path: &str) -> Result<()> {
        if self.pinned_originals()?.contains(path) {
            bail!("{path} is pinned");
        }
        if !self.synced_paths()?.contains(path) {
            bail!("{path} isn't safely synced");
        }
        library_path(path)?;
        let file = self.info.path.join(path);
        let dir = file.parent().context("no folder")?;
        {
            let _placing = self.placing();
            let mut unlocked = Unlocked::new(self.info.read_only);
            unlocked.unlock(dir)?;
            if self.info.read_only {
                // Windows won't delete a read-only file.
                protect::set_writable(&file, true)?;
            }
            std::fs::remove_file(&file).with_context(|| format!("removing {path}"))?;
            unlocked.relock()?;
        }
        self.set_evicted(path)
    }

    /// Evicts what was wanted longest ago from the recently fetched pool
    /// until it fits in `budget`, leaving the files in `keep`. Pinned
    /// originals aren't in the pool, and don't count.
    fn trim_fetched(&self, budget: u64, keep: &HashSet<String>) -> Result<()> {
        let pool = self.fetched_originals()?;
        let mut size: u64 = pool.iter().map(|original| original.size).sum();
        for original in pool {
            if size <= budget {
                break;
            }
            if keep.contains(&original.path) {
                continue;
            }
            self.evict_original(&original.path)?;
            size -= original.size;
        }
        Ok(())
    }

    /// Downloads `files`, several at once, moving each into place as it
    /// arrives. See `download_derivatives` for when it stops.
    pub(super) fn download_files(
        &self,
        plugin: &Plugin,
        settings: &TargetSettings,
        files: Vec<Wanted>,
        cancel: &AtomicBool,
    ) -> Result<DownloadSummary> {
        let staging = self.create_download_staging()?;
        let next = AtomicUsize::new(0);
        let shared = Mutex::new(Downloading::default());
        let workers = DOWNLOAD_WORKERS.min(files.len()).max(1);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| {
                    let mut session = None;
                    loop {
                        let ix = next.fetch_add(1, Ordering::Relaxed);
                        let Some(file) = files.get(ix) else { return };
                        if cancel.load(Ordering::Relaxed) {
                            lock(&shared).stop("Stopped.".into());
                        }
                        if lock(&shared).summary.stopped.is_some() {
                            return;
                        }
                        let partial = staging.join(ix.to_string());
                        let result =
                            self.download_file(plugin, settings, &mut session, file, &partial);
                        std::fs::remove_file(&partial).ok();
                        lock(&shared).finished(file, result);
                    }
                });
            }
        });
        self.remove_download_staging(&staging);
        let downloading = shared.into_inner().unwrap_or_else(|e| e.into_inner());
        Ok(downloading.summary)
    }

    /// Downloads a file to `partial`, checks it, and moves it into place.
    fn download_file(
        &self,
        plugin: &Plugin,
        settings: &TargetSettings,
        session: &mut Option<Session>,
        file: &Wanted,
        partial: &Path,
    ) -> Result<(), Failure> {
        let transient = |error: anyhow::Error| Failure::Transient(format!("{error:#}"));
        let running = match session {
            Some(running) => running,
            None => session.insert(
                plugin
                    .session(settings.config.clone())
                    .map_err(transient)?,
            ),
        };
        let result = running.download(&file.remote, partial);
        if result.is_err() {
            // Crashed: start afresh for the next file.
            *session = None;
        }
        match result.map_err(transient)? {
            Ok(()) => {}
            Err(error @ (SyncError::Config(_) | SyncError::Unsupported)) => {
                return Err(Failure::Config(error.message().to_owned()));
            }
            Err(error @ SyncError::Rejected(_)) => {
                return Err(Failure::File(error.message().to_owned()));
            }
            Err(error) => return Err(Failure::Transient(error.message().to_owned())),
        }
        check(partial, file).map_err(|e| Failure::File(format!("{e:#}")))?;
        self.place_download(file, partial).map_err(transient)
    }

    /// Moves a downloaded file into place.
    fn place_download(&self, file: &Wanted, partial: &Path) -> Result<()> {
        // The catalog's paths can come from another device.
        let components = library_path(&file.path)?;
        let root = &self.info.path;
        let dest = root.join(&file.path);
        match file.role {
            FileRole::Derivative(kind) => {
                let dir = dest.parent().context("no folder")?;
                self.create_derivative_dirs(kind, dir)?;
                std::fs::rename(partial, &dest)?;
            }
            FileRole::Original => {
                let (_, dirs) = components
                    .split_last()
                    .context("the path has no file name")?;
                if dirs.first().map(String::as_str) != Some(ORIGINALS_DIR) {
                    bail!("{} isn't among the originals", file.path);
                }
                let dirs: Vec<&str> = dirs[1..].iter().map(String::as_str).collect();
                let _placing = self.placing();
                let mut unlocked = Unlocked::new(self.info.read_only);
                unlocked.create_dirs(&root.join(ORIGINALS_DIR), &dirs)?;
                if self.info.read_only {
                    protect::set_writable(partial, false)?;
                }
                std::fs::rename(partial, &dest)?;
                unlocked.relock()?;
                drop(_placing);
                // It's here, whatever comes of this. Unrecorded, the next
                // refresh takes it for imported, so it just isn't evicted.
                if let Err(error) = self.set_on_device(&file.path, OriginalSource::Fetched) {
                    eprintln!("couldn't record {}: {error:#}", file.path);
                }
            }
        }
        Ok(())
    }

    /// A folder of its own for a download, in the library's staging folder,
    /// which is on the same drive as the library's files.
    fn create_download_staging(&self) -> Result<PathBuf> {
        let root = &self.info.path;
        let staging = root
            .join(STAGING_DIR)
            .join(uuid::Uuid::new_v4().to_string());
        let _placing = self.placing();
        let mut unlocked = Unlocked::new(self.info.read_only);
        unlocked.unlock(root)?;
        std::fs::create_dir_all(&staging)
            .with_context(|| format!("creating {}", staging.display()))?;
        unlocked.relock()?;
        Ok(staging)
    }

    fn remove_download_staging(&self, staging: &Path) {
        std::fs::remove_dir_all(staging).ok();
        let root = &self.info.path;
        let _placing = self.placing();
        let mut unlocked = Unlocked::new(self.info.read_only);
        if unlocked.unlock(root).is_ok() {
            // Only once nothing else is using it.
            std::fs::remove_dir(root.join(STAGING_DIR)).ok();
        }
    }
}

/// How the workers of a download are getting on.
#[derive(Default)]
struct Downloading {
    failures_in_a_row: usize,
    summary: DownloadSummary,
}

impl Downloading {
    fn stop(&mut self, why: String) {
        self.summary.stopped.get_or_insert(why);
    }

    fn finished(&mut self, file: &Wanted, result: Result<(), Failure>) {
        let message = match result {
            Ok(()) => {
                self.failures_in_a_row = 0;
                self.summary.downloaded += 1;
                return;
            }
            Err(Failure::Config(message)) => {
                self.stop(message.clone());
                message
            }
            Err(Failure::File(message)) => message,
            Err(Failure::Transient(message)) => {
                self.failures_in_a_row += 1;
                if self.failures_in_a_row >= MAX_FAILURES_IN_A_ROW {
                    let failures = self.failures_in_a_row;
                    self.stop(format!(
                        "Stopped after {failures} files in a row failed. The last error: \
                         {message}"
                    ));
                }
                message
            }
        };
        self.summary.failed.push((file.path.clone(), message));
    }
}

fn lock(downloading: &Mutex<Downloading>) -> std::sync::MutexGuard<'_, Downloading> {
    downloading.lock().unwrap_or_else(|e| e.into_inner())
}

/// The plugin for `settings`, if it can download.
pub(super) fn download_plugin<'a>(
    plugins: &'a Plugins,
    settings: &TargetSettings,
) -> Result<&'a Plugin> {
    let plugin = plugins
        .get(&settings.plugin)
        .ok_or_else(|| anyhow!("the \"{}\" sync plugin isn't installed", settings.plugin))?;
    if !plugin.info.capabilities.contains(Capabilities::DOWNLOAD) {
        bail!("{} can't download", plugin.info.name);
    }
    Ok(plugin)
}

/// Checks a downloaded file is the one that was uploaded.
fn check(partial: &Path, file: &Wanted) -> Result<()> {
    let size = std::fs::metadata(partial)?.len();
    if let Some(expected) = file.remote.size
        && size != expected
    {
        bail!(
            "{} came back as {size} bytes rather than {expected}",
            file.path
        );
    }
    if let Some(expected) = &file.hash {
        let mut hasher = blake3::Hasher::new();
        std::io::copy(&mut std::fs::File::open(partial)?, &mut hasher)?;
        if hasher.finalize().to_hex().as_str() != expected {
            bail!("{} came back different from what was uploaded", file.path);
        }
    }
    Ok(())
}

/// `path`'s components, if it stays inside the library.
fn library_path(path: &str) -> Result<Vec<String>> {
    Path::new(path)
        .components()
        .map(|component| match component {
            Component::Normal(name) => Ok(name.to_string_lossy().into_owned()),
            _ => Err(anyhow!("{path} isn't a path in the library")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    use super::super::sync::test_support::FakeS3;
    use super::super::{LibraryInfo, test_support};
    use super::*;

    /// A library of `count` photos with thumbnails, all uploaded to a fake
    /// S3, the plugins, and the library's id and settings for the target.
    struct Synced {
        library: Arc<Library>,
        root: PathBuf,
        s3: FakeS3,
        plugins: Plugins,
        id: Id,
        settings: TargetSettings,
    }

    fn synced(name: &str, count: u8) -> Synced {
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
        library.import(&[source]).unwrap();
        library.make_missing_thumbnails().unwrap();

        let s3 = FakeS3::start();
        let plugins = Plugins::load(None).unwrap();
        let config = crate::sync_targets::validate(&plugins, "s3", s3.config()).unwrap();
        let settings = TargetSettings {
            plugin: "s3".into(),
            config,
        };
        let id = library.add_sync_target("target-1").unwrap();
        let summary = library
            .upload(
                &plugins,
                id,
                &settings,
                &AtomicBool::new(false),
                &Arc::new(AtomicU64::new(0)),
                |_| {},
            )
            .unwrap();
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        s3.take();
        Synced {
            library,
            root,
            s3,
            plugins,
            id,
            settings,
        }
    }

    impl Synced {
        fn download_derivatives(&self) -> DownloadSummary {
            self.library
                .download_derivatives(
                    &self.plugins,
                    self.id,
                    &self.settings,
                    &AtomicBool::new(false),
                )
                .unwrap()
        }

        fn thumbnails(&self) -> Vec<PathBuf> {
            self.library
                .catalog
                .derivatives()
                .unwrap()
                .into_iter()
                .map(|record| self.library.info.path.join(record.path))
                .collect()
        }
    }

    #[test]
    fn downloads_only_the_derivatives_that_are_missing() {
        let synced = synced("download01", 3);
        let thumbnails = synced.thumbnails();
        let kept = std::fs::read(&thumbnails[0]).unwrap();
        std::fs::remove_file(&thumbnails[0]).unwrap();
        std::fs::remove_file(&thumbnails[2]).unwrap();

        let summary = synced.download_derivatives();
        assert_eq!(summary.downloaded, 2);
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        assert_eq!(std::fs::read(&thumbnails[0]).unwrap(), kept);
        assert!(thumbnails[2].is_file());

        // Only those were asked for, and none of the originals.
        let requests = synced.s3.take();
        assert_eq!(requests.len(), 2);
        assert!(requests.iter().all(|r| r.method == "GET"));
        assert!(requests.iter().all(|r| r.path.contains("/Thumbnails/")));
        assert!(!synced.library.info.path.join(STAGING_DIR).exists());

        // There's nothing left to do.
        assert_eq!(synced.download_derivatives().downloaded, 0);
        assert!(synced.s3.take().is_empty());

        test_support::remove(&synced.root);
    }

    #[test]
    fn keeps_nothing_of_a_bad_download() {
        let synced = synced("download02", 2);
        let thumbnails = synced.thumbnails();
        for thumbnail in &thumbnails {
            std::fs::remove_file(thumbnail).unwrap();
        }
        // One's gone from the service: the other still comes.
        let lost = thumbnails[0].file_name().unwrap().to_str().unwrap();
        synced.s3.lose(lost);
        let summary = synced.download_derivatives();
        assert_eq!(summary.downloaded, 1);
        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.stopped, None);
        assert!(!thumbnails[0].exists());
        assert!(thumbnails[1].is_file());

        test_support::remove(&synced.root);
    }

    #[test]
    fn stops_when_the_targets_settings_are_wrong() {
        let synced = synced("download03", 2);
        for thumbnail in synced.thumbnails() {
            std::fs::remove_file(thumbnail).unwrap();
        }
        synced.s3.fail_next(&[403, 403]);
        let summary = synced.download_derivatives();
        assert_eq!(summary.downloaded, 0);
        assert!(summary.stopped.is_some());
        assert!(!synced.library.info.path.join(STAGING_DIR).exists());

        test_support::remove(&synced.root);
    }

    impl Synced {
        fn source(&self) -> DownloadSource {
            DownloadSource {
                id: self.id,
                settings: self.settings.clone(),
            }
        }

        fn fetch(&self, photo: &Photo) -> Result<()> {
            self.library.fetch_original(
                &self.plugins,
                &[self.source()],
                photo,
                &AtomicBool::new(false),
            )
        }

        fn download_pinned(&self) -> DownloadSummary {
            self.library
                .download_pinned(&self.plugins, &[self.source()], &AtomicBool::new(false))
                .unwrap()
        }

        fn relative(&self, photo: &Photo) -> String {
            self.library.relative(&photo.path).unwrap()
        }

        fn evict(&self, photo: &Photo) {
            self.library.evict_original(&self.relative(photo)).unwrap();
            assert!(!photo.path.exists());
        }
    }

    #[test]
    fn fetches_evicted_originals_into_the_pool() {
        let synced = synced("download04", 2);
        let photos = synced.library.photos().unwrap();
        let bytes = std::fs::read(&photos[0].path).unwrap();
        synced.evict(&photos[0]);
        assert!(synced.library.fetched_originals().unwrap().is_empty());

        synced.fetch(&photos[0]).unwrap();
        assert_eq!(std::fs::read(&photos[0].path).unwrap(), bytes);
        // Read-only like any other original.
        let metadata = std::fs::metadata(&photos[0].path).unwrap();
        assert!(metadata.permissions().readonly());
        let dir = photos[0].path.parent().unwrap();
        assert!(std::fs::metadata(dir).unwrap().permissions().readonly());
        let pool = synced.library.fetched_originals().unwrap();
        assert_eq!(pool.len(), 1);
        assert_eq!(pool[0].path, synced.relative(&photos[0]));

        // It's there now, so nothing more is asked of the service.
        synced.s3.take();
        synced.fetch(&photos[0]).unwrap();
        assert!(synced.s3.take().is_empty());

        // And it can go again.
        synced.evict(&photos[0]);
        assert!(synced.library.fetched_originals().unwrap().is_empty());

        test_support::remove(&synced.root);
    }

    #[test]
    fn refuses_an_original_that_came_back_different() {
        let synced = synced("download05", 1);
        let photo = synced.library.photos().unwrap().remove(0);
        synced.evict(&photo);
        synced.s3.corrupt("/Originals/");

        let error = synced.fetch(&photo).unwrap_err();
        assert!(error.to_string().contains("different"), "{error:#}");
        assert!(!photo.path.exists());
        assert!(synced.library.fetched_originals().unwrap().is_empty());
        assert!(!synced.library.info.path.join(STAGING_DIR).exists());

        test_support::remove(&synced.root);
    }

    #[test]
    fn only_evicts_originals_that_are_synced_and_not_pinned() {
        let synced = synced("download06", 2);
        let photos = synced.library.photos().unwrap();
        synced.library.pin_photo(&photos[0]).unwrap();
        let pinned = synced.relative(&photos[0]);
        assert!(synced.library.evict_original(&pinned).is_err());
        assert!(photos[0].path.is_file());

        // Nothing's been uploaded for the photo imported since.
        let card = synced.root.join("card2");
        std::fs::create_dir_all(&card).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3]))
            .save(card.join("NEW.JPG"))
            .unwrap();
        synced.library.import(&[card]).unwrap();
        let photos = synced.library.photos().unwrap();
        let new = photos.iter().find(|p| p.path.ends_with("NEW.JPG")).unwrap();
        assert!(
            synced
                .library
                .evict_original(&synced.relative(new))
                .is_err()
        );
        assert!(new.path.is_file());

        test_support::remove(&synced.root);
    }

    #[test]
    fn trims_the_pool_by_what_was_wanted_longest_ago() {
        let synced = synced("download07", 4);
        let photos = synced.library.photos().unwrap();
        let library = &synced.library;
        for photo in &photos[..3] {
            synced.evict(photo);
            synced.fetch(photo).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        // The oldest fetched was opened since, so the next oldest goes
        // first. The pinned one's out of the pool altogether.
        library.touch(&photos[0]).unwrap();
        library.pin_photo(&photos[2]).unwrap();
        let size = |photo: &Photo| std::fs::metadata(&photo.path).unwrap().len();
        library.trim_fetched(size(&photos[0]), &HashSet::new()).unwrap();
        assert!(photos[0].path.is_file());
        assert!(!photos[1].path.exists());
        assert!(photos[2].path.is_file());
        // Imported originals are never in it.
        assert!(photos[3].path.is_file());

        // Nothing's left to go but what's kept.
        let keep = HashSet::from([synced.relative(&photos[0])]);
        library.trim_fetched(0, &keep).unwrap();
        assert!(photos[0].path.is_file());
        library.trim_fetched(0, &HashSet::new()).unwrap();
        assert!(!photos[0].path.exists());
        assert!(photos[2].path.is_file());

        // Unpinned, it can go too.
        library.unpin_photo(&photos[2]).unwrap();
        library.trim_fetched(0, &HashSet::new()).unwrap();
        assert!(!photos[2].path.exists());

        test_support::remove(&synced.root);
    }

    #[test]
    fn downloads_what_pins_need() {
        let synced = synced("download08", 3);
        let photos = synced.library.photos().unwrap();
        let library = &synced.library;
        for photo in &photos {
            synced.evict(photo);
        }
        assert_eq!(synced.download_pinned().downloaded, 0);

        library.pin_photo(&photos[0]).unwrap();
        assert_eq!(synced.download_pinned().downloaded, 1);
        assert!(photos[0].path.is_file());
        assert!(!photos[1].path.exists());

        // An album's pin covers what's added to it later.
        let album = library.create_album("Trips", None).unwrap();
        library.pin_album(album).unwrap();
        assert_eq!(synced.download_pinned().downloaded, 0);
        library.add_to_album(album, &photos[1..2]).unwrap();
        assert_eq!(synced.download_pinned().downloaded, 1);
        assert!(photos[1].path.is_file());
        assert!(!photos[2].path.exists());
        let path = synced.relative(&photos[1]);
        assert!(library.evict_original(&path).is_err());

        test_support::remove(&synced.root);
    }

    #[test]
    fn tries_each_target_for_an_original() {
        let synced = synced("download09", 1);
        let library = &synced.library;
        let photo = library.photos().unwrap().remove(0);

        // A second target has it, the first has lost it.
        let other = FakeS3::start();
        let config = crate::sync_targets::validate(&synced.plugins, "s3", other.config()).unwrap();
        let second = DownloadSource {
            id: library.add_sync_target("target-2").unwrap(),
            settings: TargetSettings {
                plugin: "s3".into(),
                config,
            },
        };
        library
            .upload(
                &synced.plugins,
                second.id,
                &second.settings,
                &AtomicBool::new(false),
                &Arc::new(AtomicU64::new(0)),
                |_| {},
            )
            .unwrap();
        synced.evict(&photo);
        synced.s3.lose("/Originals/");
        library
            .fetch_original(
                &synced.plugins,
                &[synced.source(), second],
                &photo,
                &AtomicBool::new(false),
            )
            .unwrap();
        assert!(photo.path.is_file());

        test_support::remove(&synced.root);
    }

    #[test]
    fn says_when_nothing_has_an_original() {
        let synced = synced("download10", 1);
        let photo = synced.library.photos().unwrap().remove(0);
        synced.evict(&photo);
        let error = synced
            .library
            .fetch_original(&synced.plugins, &[], &photo, &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.to_string().contains("sync target"), "{error:#}");

        test_support::remove(&synced.root);
    }

    #[test]
    fn evicted_originals_stay_tracked_while_syncing() {
        let synced = synced("download11", 2);
        let library = &synced.library;
        let photos = library.photos().unwrap();
        synced.evict(&photos[0]);

        // A new target skips what can't go from here, rather than failing.
        let other = FakeS3::start();
        let config = crate::sync_targets::validate(&synced.plugins, "s3", other.config()).unwrap();
        let settings = TargetSettings {
            plugin: "s3".into(),
            config,
        };
        let second = library.add_sync_target("target-2").unwrap();
        let summary = library
            .upload(
                &synced.plugins,
                second,
                &settings,
                &AtomicBool::new(false),
                &Arc::new(AtomicU64::new(0)),
                |_| {},
            )
            .unwrap();
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        assert_eq!(summary.uploaded, 3);

        // The only target with it can't be forgotten until it's back.
        let error = library.remove_sync_target(synced.id).unwrap_err();
        assert!(error.to_string().contains("only on this target"), "{error:#}");
        synced.fetch(&photos[0]).unwrap();
        library.remove_sync_target(synced.id).unwrap();

        test_support::remove(&synced.root);
    }

    #[test]
    fn keeps_paths_inside_the_library() {
        assert!(library_path("Originals/2026/a.jpg").is_ok());
        assert!(library_path("Originals/../../a.jpg").is_err());
        assert!(library_path("/etc/passwd").is_err());
    }
}

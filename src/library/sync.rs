//! Syncing a library to sync targets (see `sync_targets`), through sync
//! plugins (see `plugins`).
//!
//! A library can sync to any number of targets, and targets can be shared
//! by libraries: each library's files go in its own folder in the target,
//! `<library name>-<start of its id>`, so they don't mix. The catalog
//! records which targets the library syncs to, and each file's sync to each,
//! and is the canonical record of what's where: services needn't be able to
//! list what they hold. A file is uploaded if it has no record yet, or its
//! last attempt failed; files the service rejected aren't retried.
//!
//! Derivatives such as thumbnails (see `derivatives`) are synced too, ahead
//! of the originals. Only uploading is done for now; the intent is that a
//! device short on space can download a library's thumbnails, and its
//! originals only as they're wanted.
//!
//! Like the rest of the catalog, this blocks; call it from a background
//! thread.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context as _, Result, anyhow, bail};

use super::Library;
use super::catalog::{Catalog, FileRole, LibraryFile, SyncItemRecord, SyncState};
use crate::plugins::{Capabilities, Config, LocalItem, Plugins, Session, SyncError};

/// After this many failures in a row, presumably the service or network is
/// down, so the upload stops.
const MAX_FAILURES_IN_A_ROW: usize = 5;

/// A target the library syncs to.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryTarget {
    /// The library's own id for it, which its sync records refer to.
    pub id: i64,
    /// The target's id in `sync_targets`.
    pub target: String,
    /// The library's folder in the target.
    pub folder: String,
    /// Why syncing stopped (such as bad credentials), until it next works.
    pub error: Option<String>,
}

/// How a library's files stand with a target.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncStatus {
    pub synced: usize,
    /// Not yet uploaded, or to be tried again.
    pub pending: usize,
    pub rejected: usize,
}

/// A target's plugin and settings, to sync with.
#[derive(Clone, Debug)]
pub struct TargetSettings {
    pub plugin: String,
    pub config: Config,
}

/// Reported as each file starts uploading.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadProgress<'a> {
    /// Photos done so far, and to do in all. A photo's done once all its
    /// files are: its originals (its raw pair and Live Photo video too) and
    /// its derivatives.
    pub photos_done: usize,
    pub photos_total: usize,
    /// The size of the files done so far, and of all of them.
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// The file being uploaded, and its size.
    pub path: &'a str,
    pub size: u64,
}

#[derive(Debug, Default)]
pub struct UploadSummary {
    pub uploaded: usize,
    /// Files that failed or were rejected, and why.
    pub failed: Vec<(String, String)>,
    /// Why the upload stopped before the end, if it did.
    pub stopped: Option<String>,
}

impl Library {
    pub fn sync_targets(&self) -> Result<Vec<LibraryTarget>> {
        sync_targets(&self.catalog)
    }

    fn sync_target(&self, id: i64) -> Result<LibraryTarget> {
        self.sync_targets()?
            .into_iter()
            .find(|target| target.id == id)
            .ok_or_else(|| anyhow!("the library doesn't sync to target {id}"))
    }

    /// Starts syncing to the target with `target` as its id in
    /// `sync_targets`, returning the library's id for it.
    pub fn add_sync_target(&self, target: &str) -> Result<i64> {
        if self.sync_targets()?.iter().any(|t| t.target == target) {
            bail!("The library already syncs there.");
        }
        let short_id: String = self.info.id.chars().take(8).collect();
        let folder = format!("{}-{short_id}", folder_name(&self.info.name));
        self.catalog.add_sync_target(target, &folder)
    }

    /// Stops syncing to a target, forgetting what was synced. What was
    /// uploaded stays there.
    pub fn remove_sync_target(&self, id: i64) -> Result<()> {
        self.catalog.remove_sync_target(id)
    }

    fn sync_items(&self, id: i64) -> Result<HashMap<String, SyncItemRecord>> {
        sync_items(&self.catalog, id)
    }

    /// Uploads the files not yet on a target (`id` being the library's id
    /// for it), oldest photos first, until done or `cancel` is set. Bytes
    /// sent over the network are added to `sent` as they go.
    /// Failures are recorded and retried next time; the upload stops early
    /// if the target's settings are wrong, or too many files fail in a row.
    pub fn upload(
        &self,
        plugins: &Plugins,
        id: i64,
        settings: &TargetSettings,
        cancel: &AtomicBool,
        sent: &Arc<AtomicU64>,
        mut progress: impl FnMut(UploadProgress),
    ) -> Result<UploadSummary> {
        let target = self.sync_target(id)?;
        let plugin = plugins
            .get(&settings.plugin)
            .ok_or_else(|| anyhow!("the \"{}\" sync plugin isn't installed", settings.plugin))?;
        if !plugin.info.capabilities.contains(Capabilities::UPLOAD) {
            bail!("{} can't upload", plugin.info.name);
        }

        let items = self.sync_items(id)?;
        // The files that need uploading, with their photos (by index) and
        // sizes: derivatives first, so a library's thumbnails get there well
        // ahead of its originals, then originals, oldest photos first.
        let mut pending: Vec<(usize, LibraryFile, u64)> = self
            .catalog
            .photo_files()?
            .into_iter()
            .enumerate()
            .flat_map(|(photo, files)| files.into_iter().map(move |file| (photo, file)))
            .filter(|(_, file)| {
                items
                    .get(&file.path)
                    .is_none_or(|item| item.state == SyncState::Failed)
            })
            .map(|(photo, file)| {
                let size = std::fs::metadata(self.info.path.join(&file.path))
                    .map_or(0, |metadata| metadata.len());
                (photo, file, size)
            })
            .collect();
        pending.sort_by_key(|(photo, file, _)| (file.role == FileRole::Original, *photo));
        let bytes_total = pending.iter().map(|(_, _, size)| size).sum();
        // How many files each photo has left; it's done when none are.
        let mut left: HashMap<usize, usize> = HashMap::new();
        for (photo, _, _) in &pending {
            *left.entry(*photo).or_default() += 1;
        }
        let photos_total = left.len();

        let mut summary = UploadSummary::default();
        let mut session: Option<Session> = None;
        let mut failures_in_a_row = 0;
        let mut bytes_done = 0;
        let mut photos_done = 0;
        for (photo, file, size) in &pending {
            if cancel.load(Ordering::Relaxed) {
                summary.stopped = Some("Stopped.".into());
                break;
            }
            let path = &file.path;
            progress(UploadProgress {
                photos_done,
                photos_total,
                bytes_done,
                bytes_total,
                path,
                size: *size,
            });
            bytes_done += size;
            let photo_left = left.get_mut(photo).expect("counted");
            *photo_left -= 1;
            if *photo_left == 0 {
                photos_done += 1;
            }
            let session = match &mut session {
                Some(session) => session,
                None => {
                    let mut new = plugin.session(settings.config.clone())?;
                    new.count_sent(sent.clone());
                    session.insert(new)
                }
            };
            let role = file.role;
            let file = self.info.path.join(path);
            let mut record = SyncItemRecord {
                path: path.clone(),
                state: SyncState::Failed,
                remote_key: None,
                remote_version: None,
                size: None,
                attempts: 0,
                error: None,
            };
            let result = local_item(&target.folder, path, role, &file).and_then(|item| {
                record.size = Some(item.size);
                session.upload(&item, &file)
            });
            match result {
                Ok(Ok(remote)) => {
                    record.state = SyncState::Synced;
                    record.remote_key = Some(remote.key);
                    record.remote_version = remote.version;
                    summary.uploaded += 1;
                    failures_in_a_row = 0;
                }
                Ok(Err(error @ (SyncError::Config(_) | SyncError::Unsupported))) => {
                    let message = error.message().to_owned();
                    self.catalog.set_sync_target_error(id, Some(&message))?;
                    summary.stopped = Some(message);
                    return Ok(summary);
                }
                Ok(Err(SyncError::Rejected(message))) => {
                    record.state = SyncState::Rejected;
                    record.error = Some(message);
                }
                Ok(Err(SyncError::Transient(message))) => {
                    record.error = Some(message);
                    failures_in_a_row += 1;
                }
                Err(error) => {
                    // The plugin crashed (or the file couldn't be read), so
                    // start it afresh for the next file.
                    record.error = Some(format!("{error:#}"));
                    failures_in_a_row += 1;
                    *session = plugin.session(settings.config.clone())?;
                    session.count_sent(sent.clone());
                }
            }
            if let Some(error) = &record.error {
                summary.failed.push((path.clone(), error.clone()));
            }
            self.catalog.record_sync(id, &record)?;
            if failures_in_a_row >= MAX_FAILURES_IN_A_ROW {
                summary.stopped = Some(format!(
                    "Stopped after {failures_in_a_row} files in a row failed. The last error: {}",
                    record.error.unwrap_or_default()
                ));
                return Ok(summary);
            }
        }
        if target.error.is_some() {
            self.catalog.set_sync_target_error(id, None)?;
        }
        Ok(summary)
    }
}

pub(super) fn sync_targets(catalog: &Catalog) -> Result<Vec<LibraryTarget>> {
    Ok(catalog
        .sync_targets()?
        .into_iter()
        .map(|record| LibraryTarget {
            id: record.id,
            target: record.target,
            folder: record.folder,
            error: record.error,
        })
        .collect())
}

pub(super) fn sync_status(catalog: &Catalog, id: i64) -> Result<SyncStatus> {
    let items = sync_items(catalog, id)?;
    let mut status = SyncStatus::default();
    for file in catalog.files()? {
        match items.get(&file.path).map(|item| item.state) {
            Some(SyncState::Synced) => status.synced += 1,
            Some(SyncState::Rejected) => status.rejected += 1,
            Some(SyncState::Failed) | None => status.pending += 1,
        }
    }
    Ok(status)
}

fn sync_items(catalog: &Catalog, id: i64) -> Result<HashMap<String, SyncItemRecord>> {
    Ok(catalog
        .sync_items(id)?
        .into_iter()
        .map(|item| (item.path.clone(), item))
        .collect())
}

/// `name`, safe as a folder name on any service: without slashes or
/// control characters, and not starting with a dot.
fn folder_name(name: &str) -> String {
    let name: String = name
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c.is_control() {
                '-'
            } else {
                c
            }
        })
        .collect();
    let name = name.trim().trim_start_matches('.');
    if name.is_empty() {
        "Library".into()
    } else {
        name.into()
    }
}

/// The file at `path` in the library, to upload into `folder`.
fn local_item(folder: &str, path: &str, role: FileRole, file: &Path) -> Result<LocalItem> {
    let metadata = std::fs::metadata(file).with_context(|| format!("reading {path}"))?;
    let modified_at = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());
    Ok(LocalItem {
        path: format!("{folder}/{path}"),
        size: metadata.len(),
        media_type: media_type(path).to_owned(),
        modified_at,
        derivative: match role {
            FileRole::Original => None,
            FileRole::Derivative(kind) => Some(kind.key().to_owned()),
        },
    })
}

/// The MIME type for a library file, by its extension.
fn media_type(path: &str) -> &'static str {
    let extension = path
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        "dng" => "image/x-adobe-dng",
        "mov" => "video/quicktime",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use crate::plugins::Config;

    /// A request the fake S3 server received.
    #[derive(Debug)]
    pub struct Received {
        pub method: String,
        pub path: String,
        pub headers: Vec<(String, String)>,
        pub body: Vec<u8>,
    }

    impl Received {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// A stand-in for an S3 service on localhost. Answers each request
    /// with the next status in `statuses` (200 once they run out).
    pub struct FakeS3 {
        pub endpoint: String,
        received: Arc<Mutex<Vec<Received>>>,
        statuses: Arc<Mutex<Vec<u16>>>,
    }

    impl FakeS3 {
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(Vec::new()));
            let statuses = Arc::new(Mutex::new(Vec::new()));
            let (log, script) = (received.clone(), statuses.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let mut parts = line.split_whitespace();
                    let method = parts.next().unwrap_or_default().to_owned();
                    let path = parts.next().unwrap_or_default().to_owned();
                    let mut headers = Vec::new();
                    loop {
                        let mut line = String::new();
                        reader.read_line(&mut line).unwrap();
                        let Some((name, value)) = line.trim_end().split_once(':') else {
                            break;
                        };
                        headers.push((name.to_owned(), value.trim().to_owned()));
                    }
                    let length = headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                        .map_or(0, |(_, v)| v.parse().unwrap());
                    let mut body = vec![0; length];
                    reader.read_exact(&mut body).unwrap();
                    let status = {
                        let mut script = script.lock().unwrap();
                        if script.is_empty() {
                            200
                        } else {
                            script.remove(0)
                        }
                    };
                    let error = match status {
                        403 => {
                            "<Error><Code>InvalidAccessKeyId</Code><Message>Nope</Message></Error>"
                        }
                        500 => "<Error><Code>InternalError</Code></Error>",
                        _ => "",
                    };
                    write!(
                        stream,
                        "HTTP/1.1 {status} X\r\nETag: \"etag-{}\"\r\nContent-Length: {}\r\n\
                         Connection: close\r\n\r\n{error}",
                        body.len(),
                        error.len()
                    )
                    .unwrap();
                    log.lock().unwrap().push(Received {
                        method,
                        path,
                        headers,
                        body,
                    });
                }
            });
            Self {
                endpoint,
                received,
                statuses,
            }
        }

        pub fn config(&self) -> Config {
            [
                ("endpoint", self.endpoint.as_str()),
                ("bucket", "photos"),
                ("prefix", "/backup/"),
                ("access_key_id", "AKIDEXAMPLE"),
                ("secret_access_key", "secret"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect()
        }

        pub fn take(&self) -> Vec<Received> {
            std::mem::take(&mut *self.received.lock().unwrap())
        }

        pub fn fail_next(&self, statuses: &[u16]) {
            self.statuses.lock().unwrap().extend_from_slice(statuses);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::derivatives::DerivativeKind;
    use super::super::{LibraryInfo, test_support};
    use super::test_support::FakeS3;
    use super::*;

    fn jpeg(path: &Path, shade: u8) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([shade, 90, 160]))
            .save(path)
            .unwrap();
    }

    /// A library with `count` photos, and their thumbnails.
    fn library(name: &str, count: u8) -> (Arc<Library>, std::path::PathBuf) {
        let (config_dir, library_dir) = test_support::temp_dirs(name);
        let source = config_dir.parent().unwrap().join("card");
        for i in 0..count {
            jpeg(&source.join(format!("IMG_{i:04}.JPG")), i * 10);
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
        assert_eq!(library.make_missing_thumbnails().unwrap(), count as usize);
        (library, config_dir.parent().unwrap().to_path_buf())
    }

    /// Validated settings for the fake S3, as `sync_targets` would save.
    fn settings(plugins: &Plugins, s3: &FakeS3) -> TargetSettings {
        let config = crate::sync_targets::validate(plugins, "s3", s3.config()).unwrap();
        s3.take();
        TargetSettings {
            plugin: "s3".into(),
            config,
        }
    }

    #[test]
    fn uploads_files_once() {
        let (library, root) = library("upload01", 3);
        let s3 = FakeS3::start();
        let plugins = Plugins::load(None).unwrap();
        assert!(plugins.failed.is_empty(), "{:?}", plugins.failed);
        let settings = settings(&plugins, &s3);

        let id = library.add_sync_target("target-1").unwrap();
        let target = &library.sync_targets().unwrap()[0];
        assert_eq!(target.target, "target-1");
        assert_eq!(target.folder, "Test-upload01");
        // Once only.
        assert!(library.add_sync_target("target-1").is_err());

        // Thumbnails first, then originals.
        let (thumbnails, originals): (Vec<_>, Vec<_>) = library
            .catalog
            .files()
            .unwrap()
            .into_iter()
            .partition(|file| file.role == FileRole::Derivative(DerivativeKind::Thumbnail));
        assert_eq!((thumbnails.len(), originals.len()), (3, 3));
        assert!(
            thumbnails
                .iter()
                .all(|file| file.path.starts_with("Thumbnails/"))
        );
        let files: Vec<String> = thumbnails
            .into_iter()
            .chain(originals)
            .map(|file| file.path)
            .collect();
        let sizes: Vec<u64> = files
            .iter()
            .map(|path| {
                std::fs::metadata(library.info.path.join(path))
                    .unwrap()
                    .len()
            })
            .collect();
        let total: u64 = sizes.iter().sum();
        let mut seen = Vec::new();
        let sent = Arc::new(AtomicU64::new(0));
        let summary = library
            .upload(
                &plugins,
                id,
                &settings,
                &AtomicBool::new(false),
                &sent,
                |p| {
                    seen.push((
                        p.path.to_owned(),
                        p.photos_done,
                        p.photos_total,
                        p.bytes_done,
                        p.bytes_total,
                        p.size,
                    ))
                },
            )
            .unwrap();
        assert_eq!(summary.uploaded, 6);
        assert!(summary.failed.is_empty() && summary.stopped.is_none());
        // A photo's done once its thumbnail and original are.
        let expected: Vec<_> = (0..6)
            .map(|ix| {
                (
                    files[ix].clone(),
                    ix.saturating_sub(3),
                    3,
                    sizes[..ix].iter().sum::<u64>(),
                    total,
                    sizes[ix],
                )
            })
            .collect();
        assert_eq!(seen, expected);
        // Every byte of every file went over the network.
        assert_eq!(sent.load(Ordering::Relaxed), total);

        let received = s3.take();
        assert_eq!(received.len(), 6);
        for (request, path) in received.iter().zip(&files) {
            let contents = std::fs::read(library.info.path.join(path)).unwrap();
            assert_eq!(request.method, "PUT");
            // Under the target's prefix, in the library's folder.
            assert_eq!(request.path, format!("/photos/backup/Test-upload01/{path}"));
            assert_eq!(request.body, contents);
            assert_eq!(request.header("content-type"), Some("image/jpeg"));
            assert!(
                request
                    .header("authorization")
                    .unwrap()
                    .starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/")
            );
        }
        let items = library.catalog.sync_items(id).unwrap();
        assert_eq!(items.len(), 6);
        assert!(items.iter().all(|item| item.state == SyncState::Synced));
        assert_eq!(
            items[0].remote_key.as_deref(),
            Some(format!("backup/Test-upload01/{}", items[0].path).as_str())
        );
        assert!(
            items[0]
                .remote_version
                .as_deref()
                .unwrap()
                .starts_with("etag-")
        );

        // Nothing's uploaded twice.
        let summary = library
            .upload(
                &plugins,
                id,
                &settings,
                &AtomicBool::new(false),
                &sent,
                |_| {},
            )
            .unwrap();
        assert_eq!(summary.uploaded, 0);
        assert!(s3.take().is_empty());
        assert_eq!(
            sync_status(&library.catalog, id).unwrap(),
            SyncStatus {
                synced: 6,
                pending: 0,
                rejected: 0
            }
        );

        // A thumbnail made again is uploaded again.
        let photo = &library.photos().unwrap()[1];
        library.make_thumbnail(photo).unwrap();
        let summary = library
            .upload(
                &plugins,
                id,
                &settings,
                &AtomicBool::new(false),
                &sent,
                |_| {},
            )
            .unwrap();
        assert_eq!(summary.uploaded, 1);
        assert_eq!(
            s3.take()[0].path,
            format!("/photos/backup/Test-upload01/{}", files[1])
        );
        test_support::remove(&root);
    }

    #[test]
    fn libraries_share_targets_and_use_several() {
        let (first, first_root) = library("sharedaa", 2);
        let (second, second_root) = library("sharedbb", 1);
        let (one, two) = (FakeS3::start(), FakeS3::start());
        let plugins = Plugins::load(None).unwrap();
        let (one_settings, two_settings) = (settings(&plugins, &one), settings(&plugins, &two));
        let cancel = AtomicBool::new(false);
        let sent = Arc::new(AtomicU64::new(0));

        // Both libraries sync to the first target, each into its own folder.
        for library in [&first, &second] {
            let id = library.add_sync_target("one").unwrap();
            library
                .upload(&plugins, id, &one_settings, &cancel, &sent, |_| {})
                .unwrap();
        }
        let paths: Vec<String> = one.take().into_iter().map(|r| r.path).collect();
        assert_eq!(paths.len(), 6);
        assert!(
            paths[..4]
                .iter()
                .all(|p| p.starts_with("/photos/backup/Test-sharedaa/"))
        );
        assert!(
            paths[4..]
                .iter()
                .all(|p| p.starts_with("/photos/backup/Test-sharedbb/"))
        );

        // The first syncs to the second target too, separately.
        let id = first.add_sync_target("two").unwrap();
        let summary = first
            .upload(&plugins, id, &two_settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 4);
        assert_eq!(two.take().len(), 4);
        assert_eq!(first.sync_targets().unwrap().len(), 2);
        test_support::remove(&first_root);
        test_support::remove(&second_root);
    }

    #[test]
    fn retries_failures_and_stops_on_bad_settings() {
        let (library, root) = library("failure1", 3);
        let s3 = FakeS3::start();
        let plugins = Plugins::load(None).unwrap();
        let settings = settings(&plugins, &s3);
        let cancel = AtomicBool::new(false);
        let sent = Arc::new(AtomicU64::new(0));
        let id = library.add_sync_target("target").unwrap();

        // A server error fails one file, which is retried next time.
        s3.fail_next(&[500]);
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 5);
        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.failed[0].1, "InternalError");
        assert_eq!(sync_status(&library.catalog, id).unwrap().pending, 1);
        s3.take();

        // Bad credentials stop the upload, and are noted on the target.
        library.remove_sync_target(id).unwrap();
        let id = library.add_sync_target("target").unwrap();
        s3.fail_next(&[403]);
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 0);
        assert_eq!(summary.stopped.as_deref(), Some("InvalidAccessKeyId: Nope"));
        assert_eq!(s3.take().len(), 1);
        let error = library.sync_targets().unwrap()[0].error.clone();
        assert_eq!(error.as_deref(), Some("InvalidAccessKeyId: Nope"));

        // Once it works again, the error's cleared.
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 6);
        assert_eq!(library.sync_targets().unwrap()[0].error, None);

        // Stopping part way finishes the file under way: here, the second
        // original, after the three thumbnails and the first.
        library.remove_sync_target(id).unwrap();
        let id = library.add_sync_target("target").unwrap();
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |p| {
                if p.photos_done == 1 {
                    cancel.store(true, Ordering::Relaxed);
                }
            })
            .unwrap();
        assert_eq!(summary.uploaded, 5);
        assert_eq!(summary.stopped.as_deref(), Some("Stopped."));
        test_support::remove(&root);
    }
}

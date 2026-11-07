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

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::UNIX_EPOCH;

use anyhow::{Context as _, Result, anyhow, bail};

use super::Library;
use super::catalog::{Catalog, FileRole, LibraryFile, SyncItemRecord, SyncState};
use super::derivatives::DerivativeKind;
use super::id::Id;
use crate::plugins::{
    Capabilities, Config, LocalItem, Multipart, Plugin, Plugins, RemoteItem, Session, SyncError,
};

/// After this many failures in a row, presumably the service or network is
/// down, so the upload stops.
const MAX_FAILURES_IN_A_ROW: usize = 5;

/// A target the library syncs to.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryTarget {
    /// The library's own id for it, which its sync records refer to.
    pub id: Id,
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

/// Reported as each file, or part of one, starts uploading, and finishes.
#[derive(Clone, Debug, PartialEq)]
pub struct UploadProgress<'a> {
    /// Photos done so far, and to do in all. A photo's done once all its
    /// files are: its originals (its raw pair and Live Photo video too) and
    /// its derivatives.
    pub photos_done: usize,
    pub photos_total: usize,
    /// The size of the files (and parts of them) done so far, and of all
    /// of them.
    pub bytes_done: u64,
    pub bytes_total: u64,
    /// The file just started, if one has (in the target), and the size of
    /// all the files and parts under way.
    pub path: &'a str,
    pub under_way: u64,
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

    fn sync_target(&self, id: Id) -> Result<LibraryTarget> {
        self.sync_targets()?
            .into_iter()
            .find(|target| target.id == id)
            .ok_or_else(|| anyhow!("the library doesn't sync to target {id}"))
    }

    /// Starts syncing to the target with `target` as its id in
    /// `sync_targets`, returning the library's id for it.
    pub fn add_sync_target(&self, target: &str) -> Result<Id> {
        if self.sync_targets()?.iter().any(|t| t.target == target) {
            bail!("The library already syncs there.");
        }
        let short_id: String = self.info.id.chars().take(8).collect();
        let folder = format!("{}-{short_id}", folder_name(&self.info.name));
        self.catalog.add_sync_target(target, &folder)
    }

    /// Stops syncing to a target, forgetting what was synced. What was
    /// uploaded stays there.
    pub fn remove_sync_target(&self, id: Id) -> Result<()> {
        self.catalog.remove_sync_target(id)
    }

    fn sync_items(&self, id: Id) -> Result<HashMap<String, SyncItemRecord>> {
        sync_items(&self.catalog, id)
    }

    /// Uploads the files not yet on a target (`id` being the library's id
    /// for it), oldest photos first, until done or `cancel` is set. Bytes
    /// sent over the network are added to `sent` as they go.
    ///
    /// Several files go at once, each with its own instance of the plugin,
    /// and big files go in parts, at once too, if the plugin can take them
    /// that way. A big file's parts go ahead of other files, so few are
    /// part-uploaded at a time.
    ///
    /// Failures are recorded and retried next time; the upload stops early
    /// if the target's settings are wrong, or too many files fail in a row.
    pub fn upload(
        &self,
        plugins: &Plugins,
        id: Id,
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
        let multipart = plugin
            .info
            .multipart
            .filter(|_| plugin.info.capabilities.contains(Capabilities::MULTIPART));

        let items = self.sync_items(id)?;
        // The files that need uploading, with their photos (by index) and
        // sizes, in `upload_order`, oldest photos first.
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
        pending.sort_by_key(|(photo, file, _)| (upload_order(file.role), *photo));

        let mut upload = Uploading {
            library: self,
            id,
            folder: &target.folder,
            multipart,
            files: pending
                .into_iter()
                .map(|(photo, file, size)| FileUpload {
                    photo,
                    path: file.path,
                    role: file.role,
                    size,
                    item: None,
                    multipart: None,
                    bytes_counted: 0,
                    recorded: false,
                })
                .collect(),
            next_file: 0,
            ready: VecDeque::new(),
            in_flight: 0,
            bytes_done: 0,
            bytes_under_way: 0,
            photos_done: 0,
            files_left: HashMap::new(),
            failures_in_a_row: 0,
            stopping: None,
            summary: UploadSummary::default(),
        };
        for file in &upload.files {
            *upload.files_left.entry(file.photo).or_default() += 1;
        }
        let photos_total = upload.files_left.len();
        let bytes_total = upload.files.iter().map(|file| file.size).sum();

        let workers = UPLOAD_WORKERS.min(upload.files.len()).max(1);
        let (job_sender, jobs) = mpsc::channel::<Job>();
        let jobs = Mutex::new(jobs);
        let (done_sender, done) = mpsc::channel::<(Job, JobResult)>();
        std::thread::scope(|scope| -> Result<()> {
            for _ in 0..workers {
                let done = done_sender.clone();
                let jobs = &jobs;
                let config = &settings.config;
                scope.spawn(move || {
                    let mut session: Option<Session> = None;
                    loop {
                        let job = jobs.lock().unwrap_or_else(|e| e.into_inner()).recv();
                        let Ok(job) = job else { return };
                        let result = run_job(plugin, config, sent, &mut session, &job);
                        if done.send((job, result)).is_err() {
                            return;
                        }
                    }
                });
            }
            drop(done_sender);
            // Dropped on the way out, however that is, so the workers stop.
            let job_sender = job_sender;

            loop {
                if upload.stopping.is_none() && cancel.load(Ordering::Relaxed) {
                    upload.stop(Stop::Other("Stopped.".into()))?;
                }
                // Hand out work to every idle worker.
                while upload.in_flight < workers
                    && let Some(job) = upload.next_job(&self.info.path)?
                {
                    upload.in_flight += 1;
                    upload.bytes_under_way += job.size();
                    progress(upload.progress(photos_total, bytes_total, job.path()));
                    job_sender
                        .send(job)
                        .map_err(|_| anyhow!("the upload workers stopped"))?;
                }
                if upload.in_flight == 0 {
                    return Ok(());
                }
                let (job, result) = done
                    .recv()
                    .map_err(|_| anyhow!("the upload workers stopped"))?;
                upload.in_flight -= 1;
                upload.bytes_under_way -= job.size();
                upload.finished(job, result)?;
                progress(upload.progress(photos_total, bytes_total, ""));
            }
        })?;

        let mut summary = upload.summary;
        match upload.stopping {
            Some(Stop::Config(message)) => {
                self.catalog.set_sync_target_error(id, Some(&message))?;
                summary.stopped = Some(message);
            }
            Some(Stop::Other(message)) => summary.stopped = Some(message),
            None if target.error.is_some() => self.catalog.set_sync_target_error(id, None)?,
            None => {}
        }
        Ok(summary)
    }
}

/// How many files, or parts of them, are uploaded at once.
const UPLOAD_WORKERS: usize = 4;
/// How many times a part of a file is tried before the file's upload fails.
/// Starting a big file again is costly, so parts are retried at once,
/// unlike whole files, which wait for the next sync.
const PART_ATTEMPTS: u32 = 3;

/// Something for an upload worker to do.
#[derive(Clone, Debug)]
enum Job {
    /// Uploading a file whole.
    File {
        file: usize,
        item: LocalItem,
        path: PathBuf,
    },
    /// Starting a file's upload in parts.
    Begin { file: usize, item: LocalItem },
    Part {
        file: usize,
        item: LocalItem,
        path: PathBuf,
        upload: String,
        /// From 1.
        part: u32,
        offset: u64,
        len: u64,
        attempt: u32,
    },
    Finish {
        file: usize,
        item: LocalItem,
        upload: String,
        parts: Vec<String>,
    },
    /// Giving up on a file's upload in parts.
    Abort { item: LocalItem, upload: String },
}

impl Job {
    /// The bytes it sends.
    fn size(&self) -> u64 {
        match self {
            Job::File { item, .. } => item.size,
            Job::Part { len, .. } => *len,
            _ => 0,
        }
    }

    /// The path of the file it's for, in the target.
    fn path(&self) -> &str {
        match self {
            Job::File { item, .. }
            | Job::Begin { item, .. }
            | Job::Part { item, .. }
            | Job::Finish { item, .. }
            | Job::Abort { item, .. } => &item.path,
        }
    }
}

/// What a job came to: a remote item for a whole file or a finished upload
/// in parts, an id for a started one, what a part was, or nothing. `Err`
/// means the plugin crashed, or the file couldn't be read.
enum JobOutcome {
    Uploaded(RemoteItem),
    Begun(String),
    Part(String),
    Aborted,
}

type JobResult = Result<Result<JobOutcome, SyncError>>;

fn run_job(
    plugin: &Plugin,
    config: &Config,
    sent: &Arc<AtomicU64>,
    session: &mut Option<Session>,
    job: &Job,
) -> JobResult {
    let running = match session {
        Some(running) => running,
        None => {
            let mut new = plugin.session(config.clone())?;
            new.count_sent(sent.clone());
            session.insert(new)
        }
    };
    let result = match job {
        Job::File { item, path, .. } => running
            .upload(item, path)
            .map(|r| r.map(JobOutcome::Uploaded)),
        Job::Begin { item, .. } => running.begin_upload(item).map(|r| r.map(JobOutcome::Begun)),
        Job::Part {
            item,
            path,
            upload,
            part,
            offset,
            len,
            ..
        } => running
            .upload_part(item, upload, *part, path, *offset, *len)
            .map(|r| r.map(JobOutcome::Part)),
        Job::Finish {
            item,
            upload,
            parts,
            ..
        } => running
            .finish_upload(item, upload, parts)
            .map(|r| r.map(JobOutcome::Uploaded)),
        Job::Abort { item, upload } => running
            .abort_upload(item, upload)
            .map(|r| r.map(|()| JobOutcome::Aborted)),
    };
    if result.is_err() {
        // Crashed: start afresh for the next job.
        *session = None;
    }
    result
}

/// Why an upload stopped early.
enum Stop {
    /// The target's settings are wrong, which is recorded until they work.
    Config(String),
    Other(String),
}

/// A file to upload.
struct FileUpload {
    photo: usize,
    /// In the library.
    path: String,
    role: FileRole,
    size: u64,
    /// Once it's started.
    item: Option<LocalItem>,
    /// For an upload in parts, once it's begun.
    multipart: Option<PartUpload>,
    /// Of its bytes, those counted as done so far (its parts', as they go).
    bytes_counted: u64,
    /// Whether how it went has been recorded.
    recorded: bool,
}

/// A file's upload in parts.
struct PartUpload {
    id: String,
    /// What each part was, once it's uploaded.
    parts: Vec<Option<String>>,
    /// How many parts the workers have.
    under_way: usize,
    /// Why it failed, once it has: the parts not yet handed out are
    /// dropped, and it's given up on once those under way are done.
    failed: Option<(SyncState, String)>,
}

/// An upload under way (see `Library::upload`).
struct Uploading<'a> {
    library: &'a Library,
    id: Id,
    folder: &'a str,
    multipart: Option<Multipart>,
    files: Vec<FileUpload>,
    /// The first file not yet started.
    next_file: usize,
    /// Jobs to hand out ahead of starting more files: the parts of files
    /// under way, and finishing or giving up on them.
    ready: VecDeque<Job>,
    in_flight: usize,
    bytes_done: u64,
    bytes_under_way: u64,
    photos_done: usize,
    /// How many files each photo has left; it's done when none are.
    files_left: HashMap<usize, usize>,
    failures_in_a_row: usize,
    /// Once it's stopping: no more files start, and their parts not yet
    /// handed out aren't, but jobs under way finish, as do uploads in parts
    /// whose parts are all in; the rest are given up on.
    stopping: Option<Stop>,
    summary: UploadSummary,
}

impl Uploading<'_> {
    fn stop(&mut self, why: Stop) -> Result<()> {
        if self.stopping.is_none() {
            self.stopping = Some(why);
        }
        let stopped: Vec<usize> = self
            .ready
            .iter()
            .filter_map(|job| match job {
                Job::Part { file, .. } => Some(*file),
                _ => None,
            })
            .collect();
        for file in stopped {
            self.fail_parts(file, SyncState::Failed, "Stopped.".into())?;
        }
        Ok(())
    }

    fn progress<'p>(
        &self,
        photos_total: usize,
        bytes_total: u64,
        path: &'p str,
    ) -> UploadProgress<'p> {
        UploadProgress {
            photos_done: self.photos_done,
            photos_total,
            bytes_done: self.bytes_done,
            bytes_total,
            path,
            under_way: self.bytes_under_way,
        }
    }

    /// The next job to hand out, if there is one.
    fn next_job(&mut self, root: &Path) -> Result<Option<Job>> {
        if let Some(job) = self.ready.pop_front() {
            if let Job::Part { file, .. } = &job
                && let Some(upload) = &mut self.files[*file].multipart
            {
                upload.under_way += 1;
            }
            return Ok(Some(job));
        }
        while self.stopping.is_none() && self.next_file < self.files.len() {
            let ix = self.next_file;
            self.next_file += 1;
            let file = &mut self.files[ix];
            let path = root.join(&file.path);
            let item = match local_item(self.folder, &file.path, file.role, &path) {
                Ok(item) => item,
                Err(error) => {
                    // Such as when it's gone.
                    self.record(ix, SyncState::Failed, None, Some(format!("{error:#}")))?;
                    continue;
                }
            };
            file.item = Some(item.clone());
            let in_parts = self
                .multipart
                .is_some_and(|multipart| item.size >= multipart.threshold);
            return Ok(Some(if in_parts {
                Job::Begin { file: ix, item }
            } else {
                Job::File {
                    file: ix,
                    item,
                    path,
                }
            }));
        }
        Ok(None)
    }

    /// Takes in what a job came to.
    fn finished(&mut self, job: Job, result: JobResult) -> Result<()> {
        if let Job::Part { file, .. } = &job
            && let Some(upload) = &mut self.files[*file].multipart
        {
            upload.under_way -= 1;
        }
        let error = match result {
            Ok(Ok(outcome)) => return self.succeeded(job, outcome),
            Ok(Err(error)) => error,
            Err(error) => SyncError::Transient(format!("{error:#}")),
        };
        match (job, error) {
            // Giving up is best effort.
            (Job::Abort { .. }, _) => Ok(()),
            (job, error @ (SyncError::Config(_) | SyncError::Unsupported)) => {
                let message = error.message().to_owned();
                self.failed(&job, SyncState::Failed, message.clone())?;
                self.stop(Stop::Config(message))
            }
            // A part's tried again at once, a few times.
            (
                Job::Part {
                    file,
                    item,
                    path,
                    upload,
                    part,
                    offset,
                    len,
                    attempt,
                },
                SyncError::Transient(_),
            ) if attempt + 1 < PART_ATTEMPTS
                && self.stopping.is_none()
                && self.files[file]
                    .multipart
                    .as_ref()
                    .is_some_and(|upload| upload.failed.is_none()) =>
            {
                self.ready.push_front(Job::Part {
                    file,
                    item,
                    path,
                    upload,
                    part,
                    offset,
                    len,
                    attempt: attempt + 1,
                });
                Ok(())
            }
            (job, error) => {
                let state = match error {
                    SyncError::Rejected(_) => SyncState::Rejected,
                    _ => {
                        self.failures_in_a_row += 1;
                        SyncState::Failed
                    }
                };
                let message = error.message().to_owned();
                self.failed(&job, state, message.clone())?;
                if self.failures_in_a_row >= MAX_FAILURES_IN_A_ROW {
                    let failures = self.failures_in_a_row;
                    self.stop(Stop::Other(format!(
                        "Stopped after {failures} files in a row failed. The last error: \
                         {message}"
                    )))?;
                }
                Ok(())
            }
        }
    }

    /// Records that the file a job was for failed.
    fn failed(&mut self, job: &Job, state: SyncState, message: String) -> Result<()> {
        match *job {
            Job::File { file, .. } | Job::Begin { file, .. } => {
                self.record(file, state, None, Some(message))
            }
            Job::Part { file, .. } | Job::Finish { file, .. } => {
                self.fail_parts(file, state, message)
            }
            Job::Abort { .. } => Ok(()),
        }
    }

    fn succeeded(&mut self, job: Job, outcome: JobOutcome) -> Result<()> {
        match (job, outcome) {
            (Job::File { file, .. } | Job::Finish { file, .. }, JobOutcome::Uploaded(remote)) => {
                self.failures_in_a_row = 0;
                self.summary.uploaded += 1;
                self.record(file, SyncState::Synced, Some(remote), None)?;
            }
            (Job::Begin { file, item }, JobOutcome::Begun(upload)) => {
                let multipart = self.multipart.expect("only begun with multipart");
                let size = item.size;
                // Bigger parts if need be, to keep within the plugin's most.
                let part_size = multipart
                    .part_size
                    .max(size.div_ceil(u64::from(multipart.max_parts.max(1))))
                    .max(1);
                let count = size.div_ceil(part_size).max(1);
                self.files[file].multipart = Some(PartUpload {
                    id: upload.clone(),
                    parts: vec![None; count as usize],
                    under_way: 0,
                    failed: None,
                });
                if self.stopping.is_some() {
                    return self.fail_parts(file, SyncState::Failed, "Stopped.".into());
                }
                let path = self.library.info.path.join(&self.files[file].path);
                // Ahead of other files' parts, so this file's are together.
                let mut parts: Vec<Job> = (0..count)
                    .map(|part| {
                        let offset = part * part_size;
                        Job::Part {
                            file,
                            item: item.clone(),
                            path: path.clone(),
                            upload: upload.clone(),
                            part: part as u32 + 1,
                            offset,
                            len: part_size.min(size - offset),
                            attempt: 0,
                        }
                    })
                    .collect();
                parts.reverse();
                for part in parts {
                    self.ready.push_front(part);
                }
            }
            (
                Job::Part {
                    file, part, len, ..
                },
                JobOutcome::Part(etag),
            ) => {
                self.bytes_done += len;
                let entry = &mut self.files[file];
                entry.bytes_counted += len;
                if let Some(upload) = &mut entry.multipart {
                    upload.parts[part as usize - 1] = Some(etag);
                }
                self.finish_parts(file)?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Marks a file's upload in parts as failed, dropping its parts not yet
    /// handed out, and gives up on it once none are under way.
    fn fail_parts(&mut self, file: usize, state: SyncState, message: String) -> Result<()> {
        if let Some(upload) = &mut self.files[file].multipart {
            upload.failed.get_or_insert((state, message));
        }
        self.ready
            .retain(|job| !matches!(job, Job::Part { file: of, .. } if *of == file));
        self.finish_parts(file)
    }

    /// Finishes a file's upload in parts once all its parts are in, or
    /// gives up on it once it's failed and none of its parts are under way.
    fn finish_parts(&mut self, file: usize) -> Result<()> {
        let entry = &mut self.files[file];
        let Some(upload) = &mut entry.multipart else {
            return Ok(());
        };
        let item = entry.item.clone().expect("started");
        match &upload.failed {
            None if upload.parts.iter().all(Option::is_some) => {
                let parts = upload.parts.iter().flatten().cloned().collect();
                let job = Job::Finish {
                    file,
                    item,
                    upload: upload.id.clone(),
                    parts,
                };
                self.ready.push_front(job);
            }
            Some((state, message)) if upload.under_way == 0 => {
                let (state, message) = (*state, message.clone());
                let id = upload.id.clone();
                // Given up on once only.
                entry.multipart = None;
                self.ready.push_front(Job::Abort { item, upload: id });
                self.record(file, state, None, Some(message))?;
            }
            _ => {}
        }
        Ok(())
    }

    /// Records how a file's upload went, and counts it done.
    fn record(
        &mut self,
        file: usize,
        state: SyncState,
        remote: Option<RemoteItem>,
        error: Option<String>,
    ) -> Result<()> {
        let entry = &mut self.files[file];
        if entry.recorded {
            return Ok(());
        }
        entry.recorded = true;
        let record = SyncItemRecord {
            path: entry.path.clone(),
            state,
            remote_key: remote.as_ref().map(|remote| remote.key.clone()),
            remote_version: remote.and_then(|remote| remote.version),
            size: entry.item.as_ref().map(|item| item.size),
            attempts: 0,
            error: error.clone(),
        };
        if let Some(error) = error {
            self.summary.failed.push((entry.path.clone(), error));
        }
        self.bytes_done += entry.size.saturating_sub(entry.bytes_counted);
        entry.bytes_counted = entry.size;
        let photo = entry.photo;
        self.library.catalog.record_sync(self.id, &record)?;
        let left = self.files_left.get_mut(&photo).expect("counted");
        *left -= 1;
        if *left == 0 {
            self.photos_done += 1;
        }
        Ok(())
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

pub(super) fn sync_status(catalog: &Catalog, id: Id) -> Result<SyncStatus> {
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

fn sync_items(catalog: &Catalog, id: Id) -> Result<HashMap<String, SyncItemRecord>> {
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
/// Files upload in this order: thumbnails, so a whole library can be shown
/// soon, then the other derivatives, which are much bigger, then originals.
fn upload_order(role: FileRole) -> u8 {
    match role {
        FileRole::Derivative(DerivativeKind::Thumbnail) => 0,
        FileRole::Derivative(_) => 1,
        FileRole::Original => 2,
    }
}

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

    /// A stand-in for an S3 service on localhost, taking requests at once.
    /// Answers each with 200 (or as S3 would to multipart uploads), unless
    /// a failure's set up for it (see `fail_next` and `fail_matching`).
    pub struct FakeS3 {
        pub endpoint: String,
        received: Arc<Mutex<Vec<Received>>>,
        /// Statuses for requests whose path and query contain the string.
        failures: Arc<Mutex<Vec<(String, u16)>>>,
    }

    impl FakeS3 {
        pub fn start() -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let received = Arc::new(Mutex::new(Vec::new()));
            let failures = Arc::new(Mutex::new(Vec::new()));
            let (log, script) = (received.clone(), failures.clone());
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let (log, script) = (log.clone(), script.clone());
                    std::thread::spawn(move || answer(stream, &log, &script));
                }
            });
            Self {
                endpoint,
                received,
                failures,
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

        /// Fails the next requests with `statuses`, one each.
        pub fn fail_next(&self, statuses: &[u16]) {
            self.fail_matching("", statuses);
        }

        /// Fails the next requests whose path (with its query) contains
        /// `pattern` with `statuses`, one each.
        pub fn fail_matching(&self, pattern: &str, statuses: &[u16]) {
            let mut failures = self.failures.lock().unwrap();
            failures.extend(statuses.iter().map(|&status| (pattern.to_owned(), status)));
        }
    }

    fn answer(
        mut stream: std::net::TcpStream,
        log: &Mutex<Vec<Received>>,
        script: &Mutex<Vec<(String, u16)>>,
    ) {
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
            match script
                .iter()
                .position(|(pattern, _)| path.contains(pattern.as_str()))
            {
                Some(ix) => script.remove(ix).1,
                None => 200,
            }
        };
        let query = path.split_once('?').map_or("", |(_, query)| query);
        let part = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("partNumber="));
        let (etag, response) = match (status, method.as_str()) {
            (403, _) => (
                String::new(),
                "<Error><Code>InvalidAccessKeyId</Code><Message>Nope</Message></Error>".to_owned(),
            ),
            (500, _) => (
                String::new(),
                "<Error><Code>InternalError</Code></Error>".to_owned(),
            ),
            (_, "POST") if query.starts_with("uploads") => (
                String::new(),
                "<InitiateMultipartUploadResult><UploadId>upload-1</UploadId>\
                 </InitiateMultipartUploadResult>"
                    .to_owned(),
            ),
            (_, "POST") => (
                String::new(),
                "<CompleteMultipartUploadResult><ETag>&quot;final&quot;</ETag>\
                 </CompleteMultipartUploadResult>"
                    .to_owned(),
            ),
            _ => match part {
                Some(part) => (format!("part-{part}"), String::new()),
                None => (format!("etag-{}", body.len()), String::new()),
            },
        };
        // Logged first, so it's there once the client has its answer.
        log.lock().unwrap().push(Received {
            method,
            path,
            headers,
            body,
        });
        write!(
            stream,
            "HTTP/1.1 {status} X\r\nETag: \"{etag}\"\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::{LibraryInfo, test_support};
    use super::test_support::{FakeS3, Received};
    use super::*;

    #[test]
    fn thumbnails_upload_before_other_derivatives_and_originals() {
        let order = [
            FileRole::Derivative(DerivativeKind::Thumbnail),
            FileRole::Derivative(DerivativeKind::Preview),
            FileRole::Original,
        ]
        .map(upload_order);
        assert!(order.is_sorted() && order[0] < order[1] && order[1] < order[2]);
        assert_eq!(
            upload_order(FileRole::Derivative(DerivativeKind::Poster)),
            upload_order(FileRole::Derivative(DerivativeKind::Preview))
        );
    }

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
                        p.under_way,
                    ))
                },
            )
            .unwrap();
        assert_eq!(summary.uploaded, 6);
        assert!(summary.failed.is_empty() && summary.stopped.is_none());
        // They start in order, several at once, and are counted as they
        // finish; a photo's done once its thumbnail and original are.
        let started: Vec<&str> = seen
            .iter()
            .map(|(path, ..)| path.as_str())
            .filter(|path| !path.is_empty())
            .collect();
        let in_target: Vec<String> = files.iter().map(|f| format!("Test-upload01/{f}")).collect();
        assert_eq!(started, in_target);
        assert_eq!(
            seen.last().unwrap(),
            &(String::new(), 3, 3, total, total, 0)
        );
        assert!(seen.iter().any(|seen| seen.5 > sizes[0]), "several at once");
        // Every byte of every file went over the network.
        assert_eq!(sent.load(Ordering::Relaxed), total);

        let mut received = s3.take();
        assert_eq!(received.len(), 6);
        received.sort_by_key(|request| {
            files
                .iter()
                .position(|path| request.path.ends_with(path.as_str()))
        });
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

        // Bad credentials stop the upload, and are noted on the target:
        // here, for the first files, which go at once.
        library.remove_sync_target(id).unwrap();
        let id = library.add_sync_target("target").unwrap();
        s3.fail_next(&[403; UPLOAD_WORKERS]);
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 0);
        assert_eq!(summary.stopped.as_deref(), Some("InvalidAccessKeyId: Nope"));
        assert_eq!(s3.take().len(), UPLOAD_WORKERS);
        let error = library.sync_targets().unwrap()[0].error.clone();
        assert_eq!(error.as_deref(), Some("InvalidAccessKeyId: Nope"));

        // Once it works again, the error's cleared.
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 6);
        assert_eq!(library.sync_targets().unwrap()[0].error, None);

        // Stopping part way finishes the files under way: here, the first
        // ones, which started at once.
        library.remove_sync_target(id).unwrap();
        let id = library.add_sync_target("target").unwrap();
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {
                cancel.store(true, Ordering::Relaxed);
            })
            .unwrap();
        assert_eq!(summary.uploaded, UPLOAD_WORKERS);
        assert_eq!(summary.stopped.as_deref(), Some("Stopped."));
        test_support::remove(&root);
    }

    #[test]
    fn uploads_big_files_in_parts() {
        let (library, root) = library("parts001", 2);
        let s3 = FakeS3::start();
        let plugins = Plugins::load(None).unwrap();
        let settings = settings(&plugins, &s3);
        let cancel = AtomicBool::new(false);
        let sent = Arc::new(AtomicU64::new(0));
        // A thumbnail made big enough to go in parts: 16 MiB, 16 MiB, then
        // the rest.
        let big = library
            .catalog
            .files()
            .unwrap()
            .into_iter()
            .find(|file| file.path.starts_with("Thumbnails/"))
            .unwrap()
            .path;
        let big_file = library.info.path.join(&big);
        let size = (40 << 20) + 123;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&big_file)
            .unwrap()
            .set_len(size)
            .unwrap();
        let contents = std::fs::read(&big_file).unwrap();

        let id = library.add_sync_target("target").unwrap();
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 4);
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        let received = s3.take();
        let key = format!("/photos/backup/Test-parts001/{big}");
        let of_big: Vec<&Received> = received
            .iter()
            .filter(|r| r.path.starts_with(&key))
            .collect();
        // Begun, three parts, then finished.
        assert_eq!(of_big.len(), 5);
        assert_eq!(of_big[0].method, "POST");
        assert!(of_big[0].path.ends_with("?uploads="));
        let mut parts: Vec<&&Received> = of_big.iter().filter(|r| r.method == "PUT").collect();
        assert_eq!(parts.len(), 3);
        parts.sort_by_key(|r| r.path.clone());
        assert!(parts[0].path.contains("partNumber=1&uploadId=upload-1"));
        let sent_back: Vec<u8> = parts.iter().flat_map(|r| r.body.clone()).collect();
        assert_eq!(sent_back.len(), contents.len());
        assert!(sent_back == contents);
        assert_eq!(parts[0].body.len(), 16 << 20);
        let finish = of_big.last().unwrap();
        assert_eq!(finish.method, "POST");
        assert!(finish.path.ends_with("?uploadId=upload-1"));
        let body = String::from_utf8_lossy(&finish.body);
        for part in 1..=3 {
            assert!(body.contains(&format!(
                "<Part><PartNumber>{part}</PartNumber><ETag>&quot;part-{part}&quot;</ETag></Part>"
            )));
        }
        let items = library.catalog.sync_items(id).unwrap();
        let item = items.iter().find(|item| item.path == big).unwrap();
        assert_eq!(item.state, SyncState::Synced);
        assert_eq!(item.remote_version.as_deref(), Some("final"));
        assert!(sent.load(Ordering::Relaxed) >= size);

        // A part that fails is tried again at once.
        let id = library.add_sync_target("again").unwrap();
        s3.fail_matching("partNumber=2", &[500]);
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 4);
        let tries = s3
            .take()
            .iter()
            .filter(|r| r.path.contains("partNumber=2"))
            .count();
        assert_eq!(tries, 2);

        // One that keeps failing fails the file, whose upload is given up.
        let id = library.add_sync_target("failing").unwrap();
        s3.fail_matching("partNumber=2", &[500; PART_ATTEMPTS as usize]);
        let summary = library
            .upload(&plugins, id, &settings, &cancel, &sent, |_| {})
            .unwrap();
        assert_eq!(summary.uploaded, 3);
        assert_eq!(summary.failed.len(), 1);
        assert_eq!(summary.failed[0].0, big);
        let received = s3.take();
        let abort = format!("{key}?uploadId=upload-1");
        assert!(
            received
                .iter()
                .any(|r| r.method == "DELETE" && r.path == abort)
        );
        assert!(
            !received
                .iter()
                .any(|r| r.method == "POST" && r.path.ends_with("?uploadId=upload-1"))
        );
        assert_eq!(sync_status(&library.catalog, id).unwrap().pending, 1);
        test_support::remove(&root);
    }
}

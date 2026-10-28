//! Object detection: finds things such as people, dogs and cars in photos.
//! Libraries record what's found in their catalogs (see
//! `Library::detect_missing_objects`), kept apart for each `Detector`:
//!
//! - The built-in one is YOLO11, run on this device by ONNX Runtime (through
//!   yolo-rs).
//! - Detection plugins (see `plugins`) can do it otherwise, such as by
//!   asking a vision model on a service.
//!
//! The rest of this is about the built-in model.
//!
//! The model is too big to build into the app, so it's downloaded the first
//! time it's needed, into the app's config directory, and checked against
//! its known hash. Only the model is downloaded: photos never leave the
//! device.
//!
//! The model runs in a helper process: the app's own executable, started
//! with `HELPER_ARG`. Loaded, it takes around 500 MB, which ONNX Runtime
//! doesn't give back when its session is dropped, so the helper's ended
//! once it's gone unused for a while instead, and started again when it's
//! next needed.

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use image::DynamicImage;
use ort::session::Session;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use yolo_rs::model::YoloModelSession;

use crate::paths;
use crate::plugins::{self, Config, DetectError};

/// How catalogs refer to the built-in model. Photos are looked at again
/// when it changes.
pub const MODEL: &str = "yolo11x";

/// The model, pinned to a revision of yolo-rs's author's export of YOLO11x
/// (trained on COCO's 80 kinds of object).
const MODEL_URL: &str = "https://huggingface.co/pan93412/yolo-v11-onnx/resolve/a2b3cf2eb0795b6997ef8e16846fee4babddcb06/yolo11x.onnx";
pub const MODEL_SIZE: u64 = 228_016_485;
const MODEL_SHA256: &str = "c3d0431659993c018741a085af1ba4a399f9a92a0c013a037510552b935b43c2";

/// The helper process the model runs in, while it's running. Running the
/// model needs exclusive access, so photos are looked at one at a time.
static HELPER: Mutex<Option<Helper>> = Mutex::new(None);
/// The helper's ended once it's gone unused this long. Starting it again
/// takes a fraction of a second, with the model compiled for CoreML cached.
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Starts the app as the helper (see `run_helper`).
pub const HELPER_ARG: &str = "--object-detection-helper";
/// Held while downloading, so the model's only downloaded once.
static DOWNLOADING: Mutex<()> = Mutex::new(());

/// Something found in a photo.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DetectedObject {
    /// What it is, in the model's words, e.g. `person` or `dog`.
    pub label: String,
    /// How sure the model is, from 0 to 1.
    pub confidence: f32,
    /// Its bounds (left, top, right, bottom), as fractions of the photo's
    /// width and height, so they're the same at any size; if the detector
    /// knows them.
    pub bounds: Option<[f32; 4]>,
}

/// What finds objects in photos.
#[derive(Clone)]
pub enum Detector {
    /// YOLO11, run on this device.
    BuiltIn,
    /// A detection plugin, with its settings.
    Plugin {
        plugin: Arc<plugins::Detector>,
        config: Config,
    },
}

/// Why a photo couldn't be looked at.
#[derive(Debug)]
pub enum DetectFailure {
    /// Nothing more can be: the settings are wrong, or the model can't run.
    Stop(anyhow::Error),
    /// This photo couldn't be, this time, such as when the network's down.
    Skip(anyhow::Error),
    /// The detector won't look at this photo.
    Rejected(String),
}

impl From<DetectFailure> for anyhow::Error {
    fn from(failure: DetectFailure) -> Self {
        match failure {
            DetectFailure::Stop(error) | DetectFailure::Skip(error) => error,
            DetectFailure::Rejected(message) => anyhow!(message),
        }
    }
}

impl Detector {
    /// How catalogs refer to it: what it finds is kept apart from what
    /// others do.
    pub fn key(&self) -> String {
        key_for(match self {
            Detector::BuiltIn => None,
            Detector::Plugin { plugin, .. } => Some(&plugin.info.id),
        })
    }

    /// Gets it ready to look at photos, which for the built-in model means
    /// downloading it if need be, adding the bytes to `downloaded` as they
    /// come. Blocks.
    pub fn prepare(&self, downloaded: &AtomicU64) -> Result<()> {
        match self {
            Detector::BuiltIn => download_model(downloaded),
            Detector::Plugin { .. } => Ok(()),
        }
    }

    /// Whether it can look at photos without `prepare`.
    pub fn ready(&self) -> bool {
        match self {
            Detector::BuiltIn => model_available(),
            Detector::Plugin { .. } => true,
        }
    }

    /// Finds the objects in `image`, most certain first. Blocks for as
    /// long as that takes: for a plugin, perhaps seconds.
    pub fn detect(&self, image: &DynamicImage) -> Result<Vec<DetectedObject>, DetectFailure> {
        let (plugin, config) = match self {
            Detector::BuiltIn => return detect(image).map_err(DetectFailure::Stop),
            Detector::Plugin { plugin, config } => (plugin, config),
        };
        let rgb = image.to_rgb8();
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85)
            .encode_image(&rgb)
            .map_err(|error| DetectFailure::Skip(error.into()))?;
        let image = plugins::Image {
            width: rgb.width(),
            height: rgb.height(),
            jpeg,
        };
        let found = plugin
            .session(config.clone())
            .and_then(|mut session| session.detect(&image))
            // The plugin crashed.
            .map_err(DetectFailure::Skip)?;
        match found {
            Ok(objects) => Ok(objects
                .into_iter()
                .map(|object| DetectedObject {
                    label: object.label,
                    confidence: object.confidence.clamp(0., 1.),
                    bounds: object
                        .bounds
                        .map(|b| [b.left, b.top, b.right, b.bottom].map(|edge| edge.clamp(0., 1.))),
                })
                .collect()),
            Err(DetectError::Config(message)) => Err(DetectFailure::Stop(anyhow!(message))),
            Err(DetectError::Transient(message)) => Err(DetectFailure::Skip(anyhow!(message))),
            Err(DetectError::Rejected(message)) => Err(DetectFailure::Rejected(message)),
        }
    }
}

/// The key (see `Detector::key`) of the built-in detector, or the
/// detection plugin `plugin`.
pub fn key_for(plugin: Option<&str>) -> String {
    match plugin {
        None => MODEL.to_owned(),
        Some(id) => format!("plugin:{id}"),
    }
}

fn model_dir() -> Result<PathBuf> {
    Ok(paths::config_dir()
        .context("couldn't find the home folder")?
        .join("models"))
}

fn model_path() -> Result<PathBuf> {
    Ok(model_dir()?.join(format!("{MODEL}.onnx")))
}

/// Whether the model's been downloaded. Never in tests, so they don't
/// depend on it.
pub fn model_available() -> bool {
    !cfg!(test) && model_path().is_ok_and(|path| path.is_file())
}

/// Downloads the model if it isn't there yet, adding the bytes to
/// `downloaded` as they come. Blocks until done, which may be a while: it's
/// over 200 MB (`MODEL_SIZE`).
pub fn download_model(downloaded: &AtomicU64) -> Result<()> {
    let _downloading = DOWNLOADING.lock().unwrap_or_else(|e| e.into_inner());
    let path = model_path()?;
    if path.is_file() {
        return Ok(());
    }
    let dir = model_dir()?;
    std::fs::create_dir_all(&dir)?;
    // Written beside it then renamed, so a half-downloaded file is never
    // taken for the model.
    let partial = dir.join(format!(".{}.partial", uuid::Uuid::new_v4()));
    let download = || -> Result<()> {
        let response = ureq::get(MODEL_URL).call()?;
        let mut body = response.into_body().into_reader();
        let mut writer = BufWriter::new(File::create(&partial)?);
        let mut hasher = Sha256::new();
        let mut buffer = vec![0; 1 << 16];
        let mut size = 0;
        loop {
            let read = body.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            size += read as u64;
            downloaded.fetch_add(read as u64, Ordering::Relaxed);
            if size > MODEL_SIZE {
                bail!("the model is bigger than expected");
            }
            hasher.update(&buffer[..read]);
            writer.write_all(&buffer[..read])?;
        }
        if format!("{:x}", hasher.finalize()) != MODEL_SHA256 {
            bail!("the model isn't the one expected");
        }
        let file = writer.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        Ok(())
    };
    if let Err(error) = download() {
        std::fs::remove_file(&partial).ok();
        return Err(error.context("downloading the object detection model"));
    }
    std::fs::rename(&partial, &path)?;
    Ok(())
}

/// Finds the objects in `image` with the built-in model, most certain
/// first. Starts the helper process if it isn't running; the model must
/// have been downloaded. Blocks for as long as running the model takes
/// (tens of milliseconds on Apple silicon, once the helper's started).
fn detect(image: &DynamicImage) -> Result<Vec<DetectedObject>> {
    let mut helper = HELPER.lock().unwrap_or_else(|e| e.into_inner());
    if helper.is_none() {
        if !model_path()?.is_file() {
            bail!("the object detection model hasn't been downloaded");
        }
        *helper = Some(Helper::start()?);
        std::thread::Builder::new()
            .name("object-detection-idle".into())
            .spawn(end_helper_when_idle)?;
    }
    let running = helper.as_mut().expect("started above");
    let response = running.detect(image);
    running.last_used = Instant::now();
    match response {
        Ok(Response::Found(objects)) => Ok(objects),
        Ok(Response::Failed(error)) => Err(anyhow::anyhow!(error)),
        // It may have crashed: it's started again next time.
        Err(error) => {
            if let Some(mut ended) = helper.take() {
                ended.end();
            }
            Err(error.context("running the object detection helper"))
        }
    }
}

/// What the helper sends back for each image.
#[derive(Serialize, Deserialize)]
enum Response {
    Found(Vec<DetectedObject>),
    Failed(String),
}

struct Helper {
    process: Child,
    requests: BufWriter<ChildStdin>,
    responses: BufReader<ChildStdout>,
    last_used: Instant,
}

impl Helper {
    fn start() -> Result<Self> {
        let mut process = Command::new(std::env::current_exe()?)
            .arg(HELPER_ARG)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context("starting the object detection helper")?;
        Ok(Self {
            requests: BufWriter::new(process.stdin.take().context("no stdin")?),
            responses: BufReader::new(process.stdout.take().context("no stdout")?),
            process,
            last_used: Instant::now(),
        })
    }

    /// Sends the image's width and height, then its RGB pixels, and reads
    /// back a line of JSON.
    fn detect(&mut self, image: &DynamicImage) -> Result<Response> {
        let image = image.to_rgb8();
        self.requests.write_all(&image.width().to_le_bytes())?;
        self.requests.write_all(&image.height().to_le_bytes())?;
        self.requests.write_all(image.as_raw())?;
        self.requests.flush()?;
        let mut line = String::new();
        if self.responses.read_line(&mut line)? == 0 {
            bail!("it quit");
        }
        Ok(serde_json::from_str(&line)?)
    }

    fn end(&mut self) {
        self.process.kill().ok();
        self.process.wait().ok();
    }
}

/// Runs while the helper does, ending it once it's idle.
fn end_helper_when_idle() {
    loop {
        std::thread::sleep(IDLE_TIMEOUT / 4);
        if end_helper_if_idle(IDLE_TIMEOUT) {
            return;
        }
    }
}

/// Ends the helper if it's gone unused for `idle`. Returns whether it's
/// not running now.
fn end_helper_if_idle(idle: Duration) -> bool {
    let mut helper = HELPER.lock().unwrap_or_else(|e| e.into_inner());
    match &mut *helper {
        Some(running) if running.last_used.elapsed() < idle => false,
        _ => {
            if let Some(mut ended) = helper.take() {
                ended.end();
            }
            true
        }
    }
}

/// The helper process's main loop: answers each image sent on stdin (see
/// `Helper::detect`) until stdin closes, as it does when the app quits.
pub fn run_helper() {
    let mut model = None;
    let mut requests = std::io::stdin().lock();
    let mut responses = std::io::stdout().lock();
    loop {
        let mut header = [0; 8];
        if requests.read_exact(&mut header).is_err() {
            return;
        }
        let width = u32::from_le_bytes(header[..4].try_into().unwrap());
        let height = u32::from_le_bytes(header[4..].try_into().unwrap());
        let mut pixels = vec![0; width as usize * height as usize * 3];
        if requests.read_exact(&mut pixels).is_err() {
            return;
        }
        let response = match image::RgbImage::from_raw(width, height, pixels) {
            Some(image) => match detect_here(&mut model, &DynamicImage::ImageRgb8(image)) {
                Ok(objects) => Response::Found(objects),
                Err(error) => Response::Failed(format!("{error:#}")),
            },
            None => Response::Failed("the image was cut short".into()),
        };
        let sent = serde_json::to_writer(&mut responses, &response)
            .map_err(std::io::Error::from)
            .and_then(|()| writeln!(responses))
            .and_then(|()| responses.flush());
        if sent.is_err() {
            return;
        }
    }
}

/// Runs the model in this process, loading it into `model` first if it
/// isn't loaded.
fn detect_here(
    model: &mut Option<YoloModelSession>,
    image: &DynamicImage,
) -> Result<Vec<DetectedObject>> {
    let model = match model {
        Some(model) => model,
        empty => empty.insert(load()?),
    };
    let input = yolo_rs::image_to_yolo_input_tensor(image);
    let found = yolo_rs::inference(model, input.view())?;
    let (width, height) = (image.width() as f32, image.height() as f32);
    let fraction = |value: f32, of: f32| (value / of).clamp(0., 1.);
    Ok(found
        .into_iter()
        .map(|object| {
            let bounds = object.bounding_box;
            DetectedObject {
                label: object.label.to_string(),
                confidence: object.confidence,
                bounds: Some([
                    fraction(bounds.x1, width),
                    fraction(bounds.y1, height),
                    fraction(bounds.x2, width),
                    fraction(bounds.y2, height),
                ]),
            }
        })
        .collect())
}

fn load() -> Result<YoloModelSession> {
    let path = model_path()?;
    if !path.is_file() {
        bail!("the object detection model hasn't been downloaded");
    }
    #[allow(unused_mut)]
    let mut builder = Session::builder()?;
    // The GPU and Neural Engine, falling back to the CPU for what they
    // can't run. Compiling the model for them is slow, so it's cached.
    // ONNX Runtime's arena and memory patterns are for inputs that vary in
    // size; these don't, and they take around 60 MB.
    #[cfg(target_os = "macos")]
    {
        builder = builder
            .with_execution_providers([
                ort::ep::CoreML::default()
                    .with_model_cache_dir(model_dir()?.join("coreml-cache").display())
                    .build(),
                ort::ep::CPU::default().with_arena_allocator(false).build(),
            ])
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    let session = builder
        .with_memory_pattern(false)
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .commit_from_file(&path)
        .with_context(|| format!("loading {}", path.display()))?;
    Ok(YoloModelSession::new_v8(session))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Downloads the model (over 200 MB) into the app's config directory,
    /// as the app would, and runs it. The test executable can't be the
    /// helper, so it's run here.
    #[test]
    #[ignore = "downloads the model"]
    fn downloads_and_runs_the_model() {
        download_model(&AtomicU64::new(0)).unwrap();
        let blank = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            560,
            373,
            image::Rgb([128, 128, 128]),
        ));
        assert!(detect_here(&mut None, &blank).unwrap().is_empty());
    }
}

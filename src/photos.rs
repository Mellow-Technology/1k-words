//! Finding photos, reading their metadata, and grouping exposure brackets
//! and bursts into stacks.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context as _, Result, anyhow};
use exif::{In, Tag, Value};
use rayon::prelude::*;

use crate::heif::{self, is_heif};
use crate::raw::{self, is_raw};

const IMAGE_EXTENSIONS: &[&str] = &["jpg", "jpeg", "png", "webp", "gif", "bmp", "tif", "tiff"];
/// The video half of a Live Photo. Videos without a matching photo are
/// ignored.
const LIVE_VIDEO_EXTENSIONS: &[&str] = &["mov"];

/// Shots further apart than this are never part of the same bracket.
const MAX_BRACKET_GAP_SECS: f64 = 2.0;
/// Consecutive frames of a burst further apart than this end the burst.
/// This has to allow a full second because many cameras only record
/// whole-second capture times, putting frames of the same burst up to a
/// second apart.
const MAX_BURST_GAP_SECS: f64 = 1.0;
/// Cameras rarely bracket more than 9 frames; cap it so a long burst of
/// varying exposures doesn't swallow a whole folder.
const MAX_BRACKET_SIZE: usize = 9;
/// Minimum exposure difference (in stops) for two shots to count as
/// different exposures.
const MIN_EXPOSURE_DELTA_EV: f64 = 0.3;

/// A single shot: one image file, or a JPEG with the raw file the camera
/// saved alongside it.
#[derive(Clone, Debug)]
pub struct Photo {
    /// The file displayed: the JPEG of a RAW+JPEG pair, or the only file.
    pub path: PathBuf,
    /// The raw file of a RAW+JPEG pair.
    pub raw_pair: Option<PathBuf>,
    /// The video of a Live Photo.
    pub live_video: Option<PathBuf>,
    /// Display width and height, with EXIF orientation applied.
    pub width: u32,
    pub height: u32,
    pub meta: CaptureMeta,
}

impl Photo {
    pub fn aspect_ratio(&self) -> f32 {
        if self.width == 0 || self.height == 0 {
            1.0
        } else {
            self.width as f32 / self.height as f32
        }
    }

    /// The raw file for this shot, whether paired or on its own.
    pub fn raw_file(&self) -> Option<&Path> {
        self.raw_pair
            .as_deref()
            .or_else(|| is_raw(&self.path).then_some(self.path.as_path()))
    }
}

/// The subset of EXIF data used for sorting and bracket detection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CaptureMeta {
    /// Capture time in seconds (local camera time; only used for ordering).
    pub captured_at: Option<f64>,
    /// File modification time in seconds since the Unix epoch.
    pub modified_at: f64,
    pub camera: Option<String>,
    pub exposure_bias: Option<f64>,
    pub exposure_time: Option<f64>,
    pub f_number: Option<f64>,
    pub iso: Option<u32>,
    /// EXIF ExposureMode == 2 ("auto bracket").
    pub auto_bracket: bool,
    /// EXIF ExposureProgram == 1 ("manual").
    pub manual_exposure: bool,
}

impl CaptureMeta {
    /// Relative brightness of the exposure settings in stops, where larger
    /// means more light hit the sensor.
    fn exposure_value(&self) -> Option<f64> {
        let t = self.exposure_time?;
        let n = self.f_number.unwrap_or(1.0);
        let iso = self.iso.unwrap_or(100) as f64;
        if t <= 0.0 || n <= 0.0 || iso <= 0.0 {
            return None;
        }
        Some(t.log2() - 2.0 * n.log2() + (iso / 100.0).log2())
    }
}

/// One tile in the gallery: either a single photo or a bracketed set
/// collapsed into one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackKind {
    Single,
    /// The same scene shot at several exposures.
    Bracket,
    /// Several frames in quick succession at the same exposure.
    Burst,
}

#[derive(Clone, Debug)]
pub struct Stack {
    pub kind: StackKind,
    pub photos: Vec<Photo>,
    /// Index into `photos` of the frame shown as the thumbnail.
    pub cover: usize,
}

impl Stack {
    pub fn cover(&self) -> &Photo {
        &self.photos[self.cover]
    }

    /// The index of the frame to show first.
    pub fn start_frame(&self, start: StartFrame) -> usize {
        match start {
            StartFrame::Cover => self.cover,
            StartFrame::First => 0,
            StartFrame::Last => self.photos.len().saturating_sub(1),
        }
    }
}

/// Which frame of a stack to show first when moving to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartFrame {
    Cover,
    /// Arriving from the previous photo, ready to step forwards through the
    /// frames.
    First,
    /// Arriving from the next photo, ready to step backwards.
    Last,
}

/// Reads every supported image in `dir` (non-recursively) and returns them
/// grouped into stacks, in capture order.
/// Reads the photos on a camera card or other removable volume, to show
/// them before they're imported.
pub fn load_device(volume: &Path) -> Result<Vec<Stack>> {
    Ok(stacks(read_shots(device_images(volume)?)))
}

/// The images on a camera card or other removable volume. Cameras save them
/// under `DCIM`, in numbered folders like `100CANON` and `101CANON` (the DCF
/// layout), so all of `DCIM` is read, recursively. On a volume without
/// `DCIM`, only the top level is read.
pub fn device_images(volume: &Path) -> Result<Vec<PathBuf>> {
    let dcim = volume.join("DCIM");
    if !dcim.is_dir() {
        let paths = std::fs::read_dir(volume)
            .with_context(|| format!("reading {}", volume.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| is_supported_image(path))
            .collect();
        return Ok(paths);
    }
    let mut paths = Vec::new();
    collect_images(&dcim, &mut paths).with_context(|| format!("reading {}", dcim.display()))?;
    Ok(paths)
}

/// The supported images among `paths`, reading folders recursively.
pub fn images_in(paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut images = Vec::new();
    for path in paths {
        if path.is_dir() {
            collect_images(path, &mut images).ok();
        } else if is_supported_image(path) {
            images.push(path.clone());
        }
    }
    // A file chosen along with its folder is only imported once.
    images.sort();
    images.dedup();
    images
}

/// Adds the supported images in `dir` and its subfolders to `paths`,
/// skipping hidden folders (e.g. `.Spotlight-V100`, `.Trashes`) and not
/// following symbolic links.
fn collect_images(dir: &Path, paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)?.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            let hidden = entry.file_name().to_string_lossy().starts_with('.');
            // A subfolder that can't be read shouldn't hide the rest.
            if !hidden {
                collect_images(&path, paths).ok();
            }
        } else if is_supported_image(&path) {
            paths.push(path);
        }
    }
    Ok(())
}

/// Groups image files into shots (pairing RAW+JPEG and Live Photo videos)
/// and reads each shot's metadata. Files that can't be read are skipped.
pub fn read_shots(paths: Vec<PathBuf>) -> Vec<Photo> {
    read_shot_results(paths)
        .into_iter()
        .filter_map(Result::ok)
        .collect()
}

/// `read_shots`, with the files that couldn't be read and why.
pub fn read_shot_results(paths: Vec<PathBuf>) -> Vec<Result<Photo, (PathBuf, anyhow::Error)>> {
    group_shot_files(paths)
        .into_par_iter()
        .map(|files| {
            let path = files.display.clone();
            read_photo(files).map_err(|error| (path, error))
        })
        .collect()
}

/// Sorts photos into capture order and groups them into stacks.
pub fn stacks(mut photos: Vec<Photo>) -> Vec<Stack> {
    sort_photos(&mut photos);
    group_stacks(photos)
}

fn is_supported_image(path: &Path) -> bool {
    let hidden = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with('.'));
    !hidden
        && path.is_file()
        && (has_extension(path, IMAGE_EXTENSIONS)
            || is_heif(path)
            || is_raw(path)
            || is_live_video(path))
}

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| extensions.contains(&ext.to_ascii_lowercase().as_str()))
}

fn is_live_video(path: &Path) -> bool {
    has_extension(path, LIVE_VIDEO_EXTENSIONS)
}

/// The files that make up one shot.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ShotFiles {
    /// The displayed image.
    display: PathBuf,
    raw_pair: Option<PathBuf>,
    live_video: Option<PathBuf>,
}

/// Groups files that belong to the same shot, which cameras save with the
/// same name apart from the extension: a raw file and the JPEG saved next to
/// it (RAW+JPEG), and a photo and its Live Photo video.
fn group_shot_files(paths: Vec<PathBuf>) -> Vec<ShotFiles> {
    let mut by_stem: BTreeMap<(Option<PathBuf>, String), Vec<PathBuf>> = BTreeMap::new();
    for path in paths {
        let stem = path
            .file_stem()
            .map(|stem| stem.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        by_stem
            .entry((path.parent().map(Path::to_path_buf), stem))
            .or_default()
            .push(path);
    }

    let single = |files: Vec<PathBuf>| ShotFiles {
        display: files.into_iter().next().unwrap(),
        raw_pair: None,
        live_video: None,
    };
    let mut shots = Vec::new();
    for (_, files) in by_stem {
        let (mut videos, files): (Vec<_>, Vec<_>) =
            files.into_iter().partition(|p| is_live_video(p));
        let (mut raws, mut images): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| is_raw(p));
        if !images.is_empty() {
            // Prefer the JPEG if the camera saved several formats.
            let display = images
                .iter()
                .position(|p| has_extension(p, &["jpg", "jpeg"]))
                .unwrap_or(0);
            shots.push(ShotFiles {
                display: images.remove(display),
                raw_pair: (raws.len() == 1).then(|| raws.remove(0)),
                live_video: (videos.len() == 1).then(|| videos.remove(0)),
            });
        }
        // Anything left over stands alone; videos without a photo are
        // skipped.
        shots.extend(
            raws.into_iter()
                .chain(images)
                .map(|path| single(vec![path])),
        );
    }
    shots
}

fn read_photo(files: ShotFiles) -> Result<Photo> {
    let ShotFiles {
        display: path,
        raw_pair,
        live_video,
    } = files;
    let exif = read_exif(&path);
    let (mut width, mut height) = if is_raw(&path) {
        raw_dimensions(&path, exif.as_ref())?
    } else if is_heif(&path) {
        heif::dimensions(&path)?
    } else {
        let size = imagesize::size(&path)?;
        (size.width as u32, size.height as u32)
    };

    let modified_at = std::fs::metadata(&path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or(0.0, |d| d.as_secs_f64());

    let mut meta = CaptureMeta {
        modified_at,
        ..Default::default()
    };

    let orientation = if let Some(exif) = &exif {
        meta.captured_at = capture_time(exif);
        meta.camera = camera_name(exif);
        meta.exposure_bias = rational_field(exif, Tag::ExposureBiasValue);
        meta.exposure_time = rational_field(exif, Tag::ExposureTime);
        meta.f_number = rational_field(exif, Tag::FNumber);
        meta.iso = uint_field(exif, Tag::PhotographicSensitivity);
        meta.auto_bracket = uint_field(exif, Tag::ExposureMode) == Some(2);
        meta.manual_exposure = uint_field(exif, Tag::ExposureProgram) == Some(1);
        uint_field(exif, Tag::Orientation)
    } else if is_raw(&path) {
        raw_capture_meta(&path, &mut meta)
    } else {
        None
    };
    // Orientations 5-8 are rotated by 90 degrees. HEIF dimensions already
    // include the rotation.
    if matches!(orientation, Some(5..=8)) && !is_heif(&path) {
        std::mem::swap(&mut width, &mut height);
    }

    Ok(Photo {
        path,
        raw_pair,
        live_video,
        width,
        height,
        meta,
    })
}

/// Sensor dimensions of a raw file, before orientation is applied. Raw
/// files often aren't readable by `imagesize`, so use the EXIF dimensions,
/// falling back to the embedded preview's (whose aspect ratio matches).
fn raw_dimensions(path: &Path, exif: Option<&exif::Exif>) -> Result<(u32, u32)> {
    if let Some(exif) = exif
        && let (Some(width), Some(height)) = (
            uint_field(exif, Tag::PixelXDimension),
            uint_field(exif, Tag::PixelYDimension),
        )
        && width > 0
        && height > 0
    {
        return Ok((width, height));
    }
    let preview = raw::embedded_preview(path)?.ok_or_else(|| anyhow!("no raw dimensions"))?;
    Ok((preview.width(), preview.height()))
}

/// Fills in capture metadata from rawler for raw formats the EXIF reader
/// can't parse. Returns the EXIF orientation.
fn raw_capture_meta(path: &Path, meta: &mut CaptureMeta) -> Option<u32> {
    let metadata = raw::metadata(path).ok()?;
    let exif = &metadata.exif;
    let rational = |r: Option<rawler::formats::tiff::Rational>| {
        r.filter(|r| r.d != 0).map(|r| r.n as f64 / r.d as f64)
    };
    meta.captured_at = exif.date_time_original.as_ref().and_then(|time| {
        parse_capture_time(
            time.as_bytes(),
            exif.sub_sec_time_original.as_ref().map(|s| s.as_bytes()),
        )
    });
    meta.camera = Some(format!("{} {}", metadata.make, metadata.model));
    meta.exposure_bias = exif
        .exposure_bias
        .filter(|r| r.d != 0)
        .map(|r| r.n as f64 / r.d as f64);
    meta.exposure_time = rational(exif.exposure_time);
    meta.f_number = rational(exif.fnumber);
    meta.iso = exif.iso_speed_ratings.map(u32::from).or(exif.iso_speed);
    meta.auto_bracket = exif.exposure_mode == Some(2);
    meta.manual_exposure = exif.exposure_program == Some(1);
    exif.orientation.map(u32::from)
}

pub(crate) fn read_exif(path: &Path) -> Option<exif::Exif> {
    let mut reader = BufReader::new(File::open(path).ok()?);
    exif::Reader::new().read_from_container(&mut reader).ok()
}

fn uint_field(exif: &exif::Exif, tag: Tag) -> Option<u32> {
    exif.get_field(tag, In::PRIMARY)?.value.get_uint(0)
}

fn rational_field(exif: &exif::Exif, tag: Tag) -> Option<f64> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Rational(v) => v.first().map(|r| r.to_f64()),
        Value::SRational(v) => v.first().map(|r| r.to_f64()),
        _ => None,
    }
    .filter(|v| v.is_finite())
}

pub(crate) fn ascii_field(exif: &exif::Exif, tag: Tag) -> Option<&[u8]> {
    match &exif.get_field(tag, In::PRIMARY)?.value {
        Value::Ascii(v) => v.first().map(|s| s.as_slice()),
        _ => None,
    }
}

fn camera_name(exif: &exif::Exif) -> Option<String> {
    let part = |tag| ascii_field(exif, tag).map(|s| String::from_utf8_lossy(s).trim().to_string());
    match (part(Tag::Make), part(Tag::Model)) {
        (None, None) => None,
        (make, model) => Some(format!(
            "{} {}",
            make.unwrap_or_default(),
            model.unwrap_or_default()
        )),
    }
}

fn capture_time(exif: &exif::Exif) -> Option<f64> {
    let (time_tag, subsec_tag) = if ascii_field(exif, Tag::DateTimeOriginal).is_some() {
        (Tag::DateTimeOriginal, Tag::SubSecTimeOriginal)
    } else {
        (Tag::DateTime, Tag::SubSecTime)
    };
    parse_capture_time(ascii_field(exif, time_tag)?, ascii_field(exif, subsec_tag))
}

/// Parses an EXIF `YYYY:MM:DD HH:MM:SS` time and optional sub-second digits
/// into seconds.
fn parse_capture_time(time: &[u8], subsec: Option<&[u8]>) -> Option<f64> {
    let mut dt = exif::DateTime::from_ascii(time).ok()?;
    if let Some(subsec) = subsec {
        let _ = dt.parse_subsec(subsec);
    }
    let days = days_from_civil(dt.year as i64, dt.month as i64, dt.day as i64);
    let secs = days * 86_400 + dt.hour as i64 * 3600 + dt.minute as i64 * 60 + dt.second as i64;
    Some(secs as f64 + dt.nanosecond.unwrap_or(0) as f64 / 1e9)
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The proleptic Gregorian date (year, month, day) `days` after 1970-01-01:
/// the inverse of `days_from_civil`.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

impl CaptureMeta {
    /// The day the photo was taken (by the camera's clock), or else the day
    /// its file was last modified (UTC).
    pub fn date(&self) -> (i64, u32, u32) {
        let secs = self.captured_at.unwrap_or(self.modified_at);
        civil_from_days((secs / 86_400.0).floor() as i64)
    }
}

/// Sorts by capture time, falling back to modification time, then name.
fn sort_photos(photos: &mut [Photo]) {
    photos.sort_by(|a, b| {
        let key = |p: &Photo| p.meta.captured_at.unwrap_or(p.meta.modified_at);
        key(a)
            .total_cmp(&key(b))
            .then_with(|| a.path.file_name().cmp(&b.path.file_name()))
    });
}

/// Collapses runs of photos that look like an exposure bracket or a burst
/// into a single stack. Expects `photos` to be sorted by capture time.
///
/// Consecutive shots can only be grouped when they come from the same camera
/// and have the same dimensions. The first two frames decide the kind of
/// group:
///
/// - **Bracket:** taken within [`MAX_BRACKET_GAP_SECS`] of each other at
///   different exposures. Every frame's exposure must differ from every other
///   frame in the group, so back-to-back brackets are split apart.
/// - **Burst:** taken within [`MAX_BURST_GAP_SECS`] of each other at the same
///   exposure. The burst continues while each frame follows the previous one
///   just as closely at the same exposure.
pub fn group_stacks(photos: Vec<Photo>) -> Vec<Stack> {
    let mut stacks: Vec<Stack> = Vec::new();
    let mut current: Vec<Photo> = Vec::new();
    let mut kind = StackKind::Single;

    for photo in photos {
        let joined_kind = current
            .last()
            .and_then(|prev| continues_stack(kind, &current, prev, &photo));
        match joined_kind {
            Some(joined_kind) => kind = joined_kind,
            None => {
                if !current.is_empty() {
                    stacks.push(make_stack(std::mem::take(&mut current), kind));
                }
                kind = StackKind::Single;
            }
        }
        current.push(photo);
    }
    if !current.is_empty() {
        stacks.push(make_stack(current, kind));
    }
    stacks
}

/// Returns the kind of the stack if `next` can be added to `current`, whose
/// last photo is `prev`.
fn continues_stack(
    kind: StackKind,
    current: &[Photo],
    prev: &Photo,
    next: &Photo,
) -> Option<StackKind> {
    let (Some(t0), Some(t1)) = (prev.meta.captured_at, next.meta.captured_at) else {
        return None;
    };
    if prev.meta.camera != next.meta.camera
        || (prev.width, prev.height) != (next.width, next.height)
    {
        return None;
    }

    let gap = (t1 - t0).abs();
    let is_bracket_frame = || {
        gap <= MAX_BRACKET_GAP_SECS
            && current.len() < MAX_BRACKET_SIZE
            && current
                .iter()
                .all(|other| exposures_differ(&other.meta, &next.meta))
    };
    let is_burst_frame = || gap <= MAX_BURST_GAP_SECS && !exposures_differ(&prev.meta, &next.meta);

    match kind {
        StackKind::Single if is_bracket_frame() => Some(StackKind::Bracket),
        StackKind::Single if is_burst_frame() => Some(StackKind::Burst),
        StackKind::Bracket if is_bracket_frame() => Some(StackKind::Bracket),
        StackKind::Burst if is_burst_frame() => Some(StackKind::Burst),
        _ => None,
    }
}

fn exposures_differ(a: &CaptureMeta, b: &CaptureMeta) -> bool {
    let differs = |x: Option<f64>, y: Option<f64>| match (x, y) {
        (Some(x), Some(y)) => (x - y).abs() >= MIN_EXPOSURE_DELTA_EV,
        _ => false,
    };
    // Exposure compensation is what auto-exposure bracketing varies. In
    // manual mode (or when the camera flags the shot as bracketed) the
    // compensation may stay at zero, so look at the actual exposure settings
    // instead. Outside those modes the camera's metering changes exposure
    // shot to shot, which shouldn't be mistaken for a bracket.
    differs(a.exposure_bias, b.exposure_bias)
        || ((a.manual_exposure || a.auto_bracket)
            && (b.manual_exposure || b.auto_bracket)
            && differs(a.exposure_value(), b.exposure_value()))
}

/// Brackets use the frame closest to the middle exposure as the cover;
/// bursts use their first frame.
fn make_stack(photos: Vec<Photo>, kind: StackKind) -> Stack {
    let cover = match kind {
        StackKind::Single | StackKind::Burst => 0,
        StackKind::Bracket => middle_exposure(&photos),
    };
    Stack {
        kind,
        photos,
        cover,
    }
}

fn middle_exposure(photos: &[Photo]) -> usize {
    let use_bias = photos
        .iter()
        .any(|p| p.meta.exposure_bias.is_some_and(|b| b != 0.0));
    let brightness = |p: &Photo| {
        if use_bias {
            p.meta.exposure_bias.unwrap_or(0.0)
        } else {
            p.meta.exposure_value().unwrap_or(0.0)
        }
    };
    let mut order: Vec<usize> = (0..photos.len()).collect();
    order.sort_by(|&a, &b| brightness(&photos[a]).total_cmp(&brightness(&photos[b])));
    order[order.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn photo(name: &str, t: f64, bias: f64) -> Photo {
        Photo {
            path: PathBuf::from(name),
            raw_pair: None,
            live_video: None,
            width: 6000,
            height: 4000,
            meta: CaptureMeta {
                captured_at: Some(t),
                camera: Some("Test Cam".into()),
                exposure_bias: Some(bias),
                exposure_time: Some(1.0 / 125.0),
                f_number: Some(8.0),
                iso: Some(100),
                ..Default::default()
            },
        }
    }

    fn names(stacks: &[Stack]) -> Vec<Vec<&str>> {
        stacks
            .iter()
            .map(|s| s.photos.iter().map(|p| p.path.to_str().unwrap()).collect())
            .collect()
    }

    fn kinds(stacks: &[Stack]) -> Vec<StackKind> {
        stacks.iter().map(|s| s.kind).collect()
    }

    #[test]
    fn groups_three_shot_bracket() {
        let stacks = group_stacks(vec![
            photo("a", 0.0, 0.0),
            photo("b", 0.3, -2.0),
            photo("c", 0.6, 2.0),
            photo("d", 30.0, 0.0),
        ]);
        assert_eq!(names(&stacks), vec![vec!["a", "b", "c"], vec!["d"]]);
        assert_eq!(kinds(&stacks), vec![StackKind::Bracket, StackKind::Single]);
        assert_eq!(stacks[0].cover().path, PathBuf::from("a"));
    }

    #[test]
    fn groups_constant_exposure_burst() {
        let stacks = group_stacks(vec![
            photo("a", 0.0, 0.0),
            photo("b", 0.1, 0.0),
            photo("c", 0.2, 0.0),
            photo("d", 5.0, 0.0),
        ]);
        assert_eq!(names(&stacks), vec![vec!["a", "b", "c"], vec!["d"]]);
        assert_eq!(kinds(&stacks), vec![StackKind::Burst, StackKind::Single]);
        assert_eq!(stacks[0].cover, 0);
    }

    #[test]
    fn burst_with_whole_second_timestamps() {
        let stacks = group_stacks(vec![
            photo("a", 0.0, 0.0),
            photo("b", 0.0, 0.0),
            photo("c", 1.0, 0.0),
            photo("d", 1.0, 0.0),
        ]);
        assert_eq!(kinds(&stacks), vec![StackKind::Burst]);
    }

    #[test]
    fn burst_ends_when_exposure_changes() {
        let stacks = group_stacks(vec![
            photo("a", 0.0, 0.0),
            photo("b", 0.3, 0.0),
            photo("c", 0.6, 0.0),
            photo("d", 0.9, -2.0),
            photo("e", 1.2, 2.0),
        ]);
        assert_eq!(names(&stacks), vec![vec!["a", "b", "c"], vec!["d", "e"]]);
        assert_eq!(kinds(&stacks), vec![StackKind::Burst, StackKind::Bracket]);
    }

    #[test]
    fn slower_shots_at_same_exposure_are_not_a_burst() {
        let stacks = group_stacks(vec![photo("a", 0.0, 0.0), photo("b", 1.5, 0.0)]);
        assert_eq!(kinds(&stacks), vec![StackKind::Single, StackKind::Single]);
    }

    #[test]
    fn back_to_back_brackets_split_on_repeated_exposure() {
        let stacks = group_stacks(vec![
            photo("a", 0.0, 0.0),
            photo("b", 0.2, -1.0),
            photo("c", 0.4, 1.0),
            photo("d", 0.6, 0.0),
            photo("e", 0.8, -1.0),
            photo("f", 1.0, 1.0),
        ]);
        assert_eq!(
            names(&stacks),
            vec![vec!["a", "b", "c"], vec!["d", "e", "f"]]
        );
        assert_eq!(kinds(&stacks), vec![StackKind::Bracket, StackKind::Bracket]);
    }

    #[test]
    fn large_gap_breaks_bracket() {
        let stacks = group_stacks(vec![photo("a", 0.0, 0.0), photo("b", 5.0, -2.0)]);
        assert_eq!(stacks.len(), 2);
    }

    #[test]
    fn manual_bracket_uses_shutter_speed() {
        let mut shots: Vec<Photo> = [1.0 / 500.0, 1.0 / 125.0, 1.0 / 30.0]
            .iter()
            .enumerate()
            .map(|(i, &t)| {
                let mut p = photo(&format!("{i}"), i as f64 * 0.5, 0.0);
                p.meta.exposure_time = Some(t);
                p.meta.manual_exposure = true;
                p
            })
            .collect();
        let stacks = group_stacks(shots.clone());
        assert_eq!(kinds(&stacks), vec![StackKind::Bracket]);
        assert_eq!(stacks[0].cover, 1);

        // Without manual mode the same shutter changes are just metering, so
        // these frames are a burst rather than a bracket.
        for p in &mut shots {
            p.meta.manual_exposure = false;
        }
        assert_eq!(kinds(&group_stacks(shots)), vec![StackKind::Burst]);
    }

    fn grouped(paths: &[&str]) -> Vec<(String, Option<String>, Option<String>)> {
        let name = |p: &PathBuf| p.to_str().unwrap().to_string();
        let mut shots = group_shot_files(paths.iter().map(PathBuf::from).collect());
        shots.sort();
        shots
            .iter()
            .map(|s| {
                (
                    name(&s.display),
                    s.raw_pair.as_ref().map(name),
                    s.live_video.as_ref().map(name),
                )
            })
            .collect()
    }

    #[test]
    fn pairs_raw_with_jpeg_of_same_name() {
        let shots = grouped(&[
            "d/A.ARW", "d/A.JPG", "d/B.arw", "d/C.jpg", "d/D.NEF", "d/D.tif", "d/D.jpeg",
        ]);
        let expected = [
            ("d/A.JPG", Some("d/A.ARW")),
            ("d/B.arw", None),
            ("d/C.jpg", None),
            // The JPEG is preferred over other formats; the rest stay separate.
            ("d/D.jpeg", Some("d/D.NEF")),
            ("d/D.tif", None),
        ];
        assert_eq!(
            shots,
            expected
                .iter()
                .map(|(d, r)| (d.to_string(), r.map(String::from), None))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn pairs_live_photo_videos() {
        let shots = grouped(&["d/IMG_1.HEIC", "d/IMG_1.MOV", "d/IMG_2.jpg", "d/IMG_3.mov"]);
        assert_eq!(
            shots,
            vec![
                ("d/IMG_1.HEIC".into(), None, Some("d/IMG_1.MOV".into())),
                // A video without a photo isn't shown.
                ("d/IMG_2.jpg".into(), None, None),
            ]
        );
    }

    #[test]
    fn raw_file_of_pair_or_raw_only_photo() {
        let mut p = photo("a.jpg", 0.0, 0.0);
        assert_eq!(p.raw_file(), None);
        p.raw_pair = Some("a.arw".into());
        assert_eq!(p.raw_file(), Some(Path::new("a.arw")));
        let raw_only = photo("b.CR3", 0.0, 0.0);
        assert_eq!(raw_only.raw_file(), Some(Path::new("b.CR3")));
    }

    /// A fake camera card at a fresh temporary path.
    fn card(name: &str, files: &[&str]) -> PathBuf {
        let volume =
            std::env::temp_dir().join(format!("1k-words-card-{}-{name}", std::process::id()));
        std::fs::remove_dir_all(&volume).ok();
        for file in files {
            let path = volume.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            image::RgbImage::from_pixel(8, 8, image::Rgb([200, 90, 40]))
                .save(&path)
                .unwrap();
        }
        volume
    }

    fn loaded_paths(stacks: &[Stack], volume: &Path) -> Vec<String> {
        let mut paths: Vec<String> = stacks
            .iter()
            .flat_map(|stack| &stack.photos)
            .map(|photo| {
                let relative = photo.path.strip_prefix(volume).unwrap();
                relative.to_string_lossy().into_owned()
            })
            .collect();
        paths.sort();
        paths
    }

    #[test]
    fn device_scan_reads_all_of_dcim() {
        let volume = card(
            "dcim",
            &[
                "DCIM/100CANON/IMG_0001.JPG",
                "DCIM/101CANON/IMG_0001.JPG",
                "DCIM/101CANON/nested/IMG_0002.JPG",
                "DCIM/.thumbnails/IMG_0001.JPG",
                "MISC/menu.jpg",
                "cover.jpg",
            ],
        );
        let stacks = load_device(&volume).unwrap();
        // Same-named shots in different folders stay separate photos.
        assert_eq!(
            loaded_paths(&stacks, &volume),
            [
                "DCIM/100CANON/IMG_0001.JPG",
                "DCIM/101CANON/IMG_0001.JPG",
                "DCIM/101CANON/nested/IMG_0002.JPG",
            ]
        );
        std::fs::remove_dir_all(&volume).ok();
    }

    #[test]
    fn device_without_dcim_reads_top_level() {
        let volume = card("no-dcim", &["a.jpg", "photos/b.jpg"]);
        let stacks = load_device(&volume).unwrap();
        assert_eq!(loaded_paths(&stacks, &volume), ["a.jpg"]);
        std::fs::remove_dir_all(&volume).ok();
    }

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        for days in [-1, 0, 59, 11_016, 11_017, 20_000, 738_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m as i64, d as i64), days);
        }
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));
    }
}

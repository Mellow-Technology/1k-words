//! Finding objects in a library's photos (see `detect`), and recording
//! them in its catalog.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, AtomicU64, AtomicUsize, Ordering};

use anyhow::{Result, bail};
use image::DynamicImage;

use super::Library;
use crate::detect::{self, DetectFailure, DetectedObject, Detector};
use crate::photos::Photo;

/// A kind of object found in a library's photos, such as "dog".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectKind {
    /// The model's name for it.
    pub label: String,
    /// How many photos it's in.
    pub photos: usize,
}

/// The detector a library uses (see `Library::set_detector`).
pub(super) struct DetectorSlot {
    /// Its key (see `Detector::key`), which is known before the detector
    /// is, as when it's a plugin that's still loading.
    key: String,
    detector: Option<Detector>,
}

impl Default for DetectorSlot {
    fn default() -> Self {
        Self {
            key: detect::MODEL.to_owned(),
            detector: Some(Detector::BuiltIn),
        }
    }
}

/// How a library's look for objects is going (see
/// `Library::detection_status`), updated as it goes.
#[derive(Default)]
pub(super) struct DetectionProgress {
    /// 0 when it isn't running, `DOWNLOADING` or `LOOKING`.
    phase: AtomicU8,
    downloaded: AtomicU64,
    /// Photos looked at so far, of those to look at.
    done: AtomicUsize,
    total: AtomicUsize,
}

const DOWNLOADING: u8 = 1;
const LOOKING: u8 = 2;

/// What a library's detector is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DetectionStatus {
    /// Downloading the built-in model: `done` bytes of `total`.
    Downloading { done: u64, total: u64 },
    /// Looking at photos: `done` of `total`.
    Looking { done: usize, total: usize },
}

/// Marks the look for objects as over, however it ends.
struct Finished<'a>(&'a DetectionProgress);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.phase.store(0, Ordering::Relaxed);
    }
}

/// After this many photos in a row couldn't be looked at, the detector's
/// presumably unreachable, so looking stops until next time.
const MAX_SKIPPED_IN_A_ROW: usize = 5;

impl Library {
    /// Sets what finds objects in the library's photos: the detector with
    /// `key`, which may not be ready to use yet (`None`). What it finds is
    /// kept apart from what other detectors have.
    pub fn set_detector(&self, key: String, detector: Option<Detector>) {
        *self.detector.lock().unwrap_or_else(|e| e.into_inner()) = DetectorSlot { key, detector };
    }

    /// The key of the detector the library uses.
    pub fn detector_key(&self) -> String {
        self.detector
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .key
            .clone()
    }

    fn detector(&self) -> Option<Detector> {
        self.detector
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .detector
            .clone()
    }

    /// Has the library's detector look for objects in the photos it hasn't
    /// looked at, oldest first, returning how many it looked at. Prepares
    /// it first if it's needed (downloading the built-in model). If it's
    /// already running, it's asked to look again when done instead; and
    /// if the detector changes meanwhile, it starts again with the new one.
    /// Blocks until done; it stops early if the library's folder goes (such
    /// as when it's moved), or the detector can't go on.
    pub fn detect_missing_objects(&self) -> Result<usize> {
        self.detecting.run(|| {
            let Some(detector) = self.detector() else {
                return Ok(0);
            };
            let key = detector.key();
            let missing = self.missing_objects(&key)?;
            if missing.is_empty() {
                return Ok(0);
            }
            let progress = &self.detection_progress;
            let _finished = Finished(progress);
            if !detector.ready() {
                progress.downloaded.store(0, Ordering::Relaxed);
                progress.phase.store(DOWNLOADING, Ordering::Relaxed);
            }
            detector.prepare(&progress.downloaded)?;
            progress.done.store(0, Ordering::Relaxed);
            progress.total.store(missing.len(), Ordering::Relaxed);
            progress.phase.store(LOOKING, Ordering::Relaxed);
            let mut scanned = 0;
            let mut skipped_in_a_row = 0;
            for (looked, photo) in missing.into_iter().enumerate() {
                progress.done.store(looked, Ordering::Relaxed);
                if !self.info.path.is_dir() {
                    bail!("{} isn't there", self.info.path.display());
                }
                if self.detector_key() != key {
                    self.detecting.wanted.store(true, Ordering::SeqCst);
                    break;
                }
                let image = match self.detection_image(&photo) {
                    Ok(image) => image,
                    // Such as a damaged file.
                    Err(_) => continue,
                };
                let objects = match detector.detect(&image) {
                    Ok(objects) => objects,
                    // Recorded as having nothing, so it isn't asked again.
                    Err(DetectFailure::Rejected(message)) => {
                        eprintln!("{} wasn't looked at: {message}", photo.path.display());
                        Vec::new()
                    }
                    Err(DetectFailure::Skip(error)) => {
                        skipped_in_a_row += 1;
                        if skipped_in_a_row >= MAX_SKIPPED_IN_A_ROW {
                            return Err(error.context(format!(
                                "{skipped_in_a_row} photos in a row couldn't be looked at"
                            )));
                        }
                        continue;
                    }
                    Err(DetectFailure::Stop(error)) => return Err(error),
                };
                skipped_in_a_row = 0;
                self.catalog
                    .set_objects(&self.relative(&photo.path)?, &key, &objects)?;
                scanned += 1;
            }
            Ok(scanned)
        })
    }

    /// What the library's detector is doing, if it's looking for objects.
    pub fn detection_status(&self) -> Option<DetectionStatus> {
        let progress = &self.detection_progress;
        match progress.phase.load(Ordering::Relaxed) {
            DOWNLOADING => Some(DetectionStatus::Downloading {
                done: progress.downloaded.load(Ordering::Relaxed),
                total: detect::MODEL_SIZE,
            }),
            LOOKING => Some(DetectionStatus::Looking {
                done: progress.done.load(Ordering::Relaxed),
                total: progress.total.load(Ordering::Relaxed),
            }),
            _ => None,
        }
    }

    /// The objects the library's detector found in `photo`, most certain
    /// first, looking now if it hasn't looked yet and it's ready to. None
    /// if it hasn't and can't, as for photos that aren't in the library.
    /// Blocks for as long as that takes.
    pub fn objects(&self, photo: &Photo) -> Result<Option<Vec<DetectedObject>>> {
        let Ok(original) = self.relative(&photo.path) else {
            return Ok(None);
        };
        let key = self.detector_key();
        if let Some(objects) = self.catalog.objects(&original, &key)? {
            return Ok(Some(objects));
        }
        let Some(detector) = self.detector().filter(|d| d.ready() && d.key() == key) else {
            return Ok(None);
        };
        let objects = match detector.detect(&self.detection_image(photo)?) {
            Ok(objects) => objects,
            Err(DetectFailure::Rejected(_)) => Vec::new(),
            Err(failure) => return Err(failure.into()),
        };
        self.catalog.set_objects(&original, &key, &objects)?;
        Ok(Some(objects))
    }

    /// Records objects as if the built-in model had found them in `photo`.
    #[cfg(test)]
    pub fn set_objects(&self, photo: &Photo, objects: &[DetectedObject]) -> Result<()> {
        self.catalog
            .set_objects(&self.relative(&photo.path)?, detect::MODEL, objects)
    }

    /// Every kind of object the library's detector has found, those in
    /// the most photos first.
    pub fn object_kinds(&self) -> Result<Vec<ObjectKind>> {
        self.catalog.object_kinds(&self.detector_key())
    }

    /// The photos with objects that `query` names, such as "dog" or
    /// "people" (see `matching_labels`), as the library's detector found.
    pub fn search_objects(&self, query: &str) -> Result<HashSet<PathBuf>> {
        let labels: Vec<String> = self
            .object_kinds()?
            .into_iter()
            .map(|kind| kind.label)
            .collect();
        let labels = matching_labels(query, &labels);
        Ok(self
            .catalog
            .photos_with_objects(&labels, &self.detector_key())?
            .into_iter()
            .map(|path| self.info.path.join(path))
            .collect())
    }

    /// What the detector looks at: the photo's thumbnail, made if need be.
    /// The built-in model scales what it's given to 640 × 640, so the
    /// original would be no better, and is much slower to decode.
    fn detection_image(&self, photo: &Photo) -> Result<DynamicImage> {
        if let Some(path) = &photo.thumbnail
            && let Ok(image) = image::open(path)
        {
            return Ok(image);
        }
        self.make_thumbnail(photo)
    }

    fn missing_objects(&self, key: &str) -> Result<Vec<Photo>> {
        let scanned = self.catalog.object_scans(key)?;
        Ok(self
            .photos()?
            .into_iter()
            .filter(|photo| {
                self.relative(&photo.path)
                    .is_ok_and(|original| !scanned.contains(&original))
            })
            .collect())
    }
}

/// The labels among `labels` that a search names. One it names exactly,
/// in the singular or plural ("dog", "people"), is all it means: "dog"
/// isn't "hot dog". Otherwise, those with a word that starts with it, in
/// the singular or plural, so "ball" finds "sports ball" and "do" already
/// finds "dog" while it's typed. Case doesn't matter.
pub fn matching_labels(query: &str, labels: &[String]) -> Vec<String> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let named: Vec<String> = labels
        .iter()
        .filter(|label| **label == query || plural(label) == query)
        .cloned()
        .collect();
    if !named.is_empty() {
        return named;
    }
    labels
        .iter()
        .filter(|label| has_word_starting(label, &query))
        .cloned()
        .collect()
}

/// The kinds of object to suggest while `query` is typed: those with a word
/// that starts with it (as `matching_labels`), those whose whole name does
/// first, then those in the most photos.
pub fn suggestions<'a>(query: &str, kinds: &'a [ObjectKind]) -> Vec<&'a ObjectKind> {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return Vec::new();
    }
    let mut found: Vec<&ObjectKind> = kinds
        .iter()
        .filter(|kind| has_word_starting(&kind.label, &query))
        .collect();
    found.sort_by_key(|kind| {
        let whole = kind.label.starts_with(&query) || plural(&kind.label).starts_with(&query);
        (!whole, std::cmp::Reverse(kind.photos))
    });
    found
}

/// Whether a word of `label`, or of its plural, starts with `query`
/// (lowercase); or the whole label, for queries of more than one word.
fn has_word_starting(label: &str, query: &str) -> bool {
    [label.to_owned(), plural(label)]
        .iter()
        .any(|name| name.starts_with(query) || name.split(' ').any(|word| word.starts_with(query)))
}

/// The plural of a label (the model's are all nouns), e.g. "sports balls".
fn plural(label: &str) -> String {
    let (start, last) = match label.rsplit_once(' ') {
        Some((start, last)) => (format!("{start} "), last),
        None => (String::new(), label),
    };
    let last = match last {
        "person" => "people".into(),
        "mouse" => "mice".into(),
        "knife" => "knives".into(),
        "skis" | "scissors" => last.into(),
        _ if ["s", "x", "ch", "sh"].iter().any(|end| last.ends_with(end)) => format!("{last}es"),
        _ => format!("{last}s"),
    };
    start + &last
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_labels_by_word_and_plural() {
        let labels: Vec<String> = [
            "baseball bat",
            "bus",
            "dog",
            "donut",
            "hot dog",
            "knife",
            "person",
            "sports ball",
        ]
        .map(String::from)
        .into();
        let matching = |query| matching_labels(query, &labels);
        // Named exactly, it's only that kind.
        assert_eq!(matching("Dog"), ["dog"]);
        assert_eq!(matching("dogs"), ["dog"]);
        assert_eq!(matching("hot"), ["hot dog"]);
        assert_eq!(matching("do"), ["dog", "donut", "hot dog"]);
        assert_eq!(matching("people"), ["person"]);
        assert_eq!(matching("peo"), ["person"]);
        assert_eq!(matching("buses"), ["bus"]);
        assert_eq!(matching("knives"), ["knife"]);
        assert_eq!(matching(" ball "), ["sports ball"]);
        assert_eq!(matching("baseball bats"), ["baseball bat"]);
        assert!(matching("").is_empty());
        assert!(matching("cat").is_empty());
    }
}

#[cfg(test)]
mod suggestion_tests {
    use super::*;

    #[test]
    fn suggests_whole_names_then_the_most_common() {
        let kinds =
            [("person", 40), ("hot dog", 9), ("dog", 3), ("donut", 1)].map(|(label, photos)| {
                ObjectKind {
                    label: label.into(),
                    photos,
                }
            });
        let suggested = |query| {
            suggestions(query, &kinds)
                .into_iter()
                .map(|kind| kind.label.as_str())
                .collect::<Vec<_>>()
        };
        // "hot dog" is in more photos, but it doesn't start with "do".
        assert_eq!(suggested("do"), ["dog", "donut", "hot dog"]);
        assert_eq!(suggested("D"), ["dog", "donut", "hot dog"]);
        assert_eq!(suggested("peop"), ["person"]);
        assert!(suggested(" ").is_empty());
        assert!(suggested("cat").is_empty());
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    /// A stand-in for an OpenAI-compatible server, with one vision model,
    /// "vision-model", that sees a dog in every photo. Returns its URL and
    /// the requests it gets (method, path, body).
    #[allow(clippy::type_complexity)]
    pub fn fake_vision_server() -> (String, Arc<Mutex<Vec<(String, String, String)>>>) {
        slow_vision_server(std::time::Duration::ZERO)
    }

    /// `fake_vision_server`, taking `delay` to answer each request.
    #[allow(clippy::type_complexity)]
    pub fn slow_vision_server(
        delay: std::time::Duration,
    ) -> (String, Arc<Mutex<Vec<(String, String, String)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = received.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    let Some((name, value)) = line.trim_end().split_once(':') else {
                        break;
                    };
                    if name.eq_ignore_ascii_case("content-length") {
                        length = value.trim().parse().unwrap();
                    }
                }
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                std::thread::sleep(delay);
                let answer = if path.ends_with("/models") {
                    r#"{"data": [{"id": "vision-model"}]}"#.to_owned()
                } else {
                    let content = r#"Here you go: {"objects": [{"label": "Dog", "confidence": 0.9, "box": [0.1, 0.2, 0.5, 0.8]}, {"label": "grass"}]}"#;
                    serde_json::json!({"choices": [{"message": {"content": content}}]}).to_string()
                };
                log.lock().unwrap().push((
                    method,
                    path,
                    String::from_utf8_lossy(&body).into_owned(),
                ));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n{answer}",
                    answer.len()
                )
                .unwrap();
            }
        });
        (endpoint, received)
    }
}

#[cfg(test)]
mod plugin_tests {
    use std::sync::Arc;

    use super::super::{LibraryInfo, test_support};
    use super::test_support::{fake_vision_server, slow_vision_server};
    use super::*;
    use crate::plugins::{self, Plugins};

    #[test]
    fn finds_objects_with_a_detection_plugin() {
        let (config_dir, library_dir) = test_support::temp_dirs("detect-plugin");
        let card = config_dir.parent().unwrap().join("card");
        std::fs::create_dir_all(&card).unwrap();
        for ix in 0..2u8 {
            image::RgbImage::from_pixel(40, 30, image::Rgb([ix * 90, 60, 120]))
                .save(card.join(format!("IMG_{ix}.JPG")))
                .unwrap();
        }
        let library = Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: "detect-plugin".into(),
                    name: "Test".into(),
                    path: library_dir,
                    read_only: true,
                },
                &config_dir,
            )
            .unwrap(),
        );
        library.import(&[card]).unwrap();

        let (endpoint, received) = fake_vision_server();
        let plugins = Plugins::load(None).unwrap();
        assert!(plugins.failed.is_empty(), "{:?}", plugins.failed);
        let plugin = plugins.detector("openai-vision").unwrap().clone();
        let config = |model: &str| {
            let config = vec![
                ("endpoint".to_owned(), endpoint.clone()),
                ("model".to_owned(), model.to_owned()),
            ];
            plugins::complete_config(&plugin.info.config, config).unwrap()
        };
        // The server's asked whether it has the model.
        let mut session = plugin.session(config("nope")).unwrap();
        assert!(session.validate().unwrap().is_err());
        let mut session = plugin.session(config("vision-model")).unwrap();
        assert!(session.validate().unwrap().is_ok());

        let config = config("vision-model");
        let detector = Detector::Plugin { plugin, config };
        library.set_detector(detector.key(), Some(detector));
        assert_eq!(library.detect_missing_objects().unwrap(), 2);
        assert_eq!(
            library.object_kinds().unwrap(),
            [
                ObjectKind {
                    label: "dog".into(),
                    photos: 2
                },
                ObjectKind {
                    label: "grass".into(),
                    photos: 2
                }
            ]
        );
        let photo = &library.photos().unwrap()[0];
        let objects = library.objects(photo).unwrap().unwrap();
        assert_eq!(objects[0].bounds, Some([0.1, 0.2, 0.5, 0.8]));
        assert_eq!(objects[1].bounds, None);
        assert_eq!(library.search_objects("dogs").unwrap().len(), 2);

        // Each photo's thumbnail went to the model.
        let requests: Vec<_> = received
            .lock()
            .unwrap()
            .iter()
            .filter(|(method, ..)| method == "POST")
            .cloned()
            .collect();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].1.ends_with("/v1/chat/completions"));
        assert!(requests[0].2.contains(r#""model":"vision-model""#));
        assert!(requests[0].2.contains("data:image/jpeg;base64,"));

        // The built-in model's findings are apart: it's found nothing.
        library.set_detector(detect::MODEL.into(), None);
        assert!(library.object_kinds().unwrap().is_empty());
        assert_eq!(library.objects(photo).unwrap(), None);
        test_support::remove(config_dir.parent().unwrap());
    }

    #[test]
    fn says_how_looking_for_objects_is_going() {
        let (config_dir, library_dir) = test_support::temp_dirs("detect-status");
        let card = config_dir.parent().unwrap().join("card");
        std::fs::create_dir_all(&card).unwrap();
        for ix in 0..2u8 {
            image::RgbImage::from_pixel(40, 30, image::Rgb([ix * 90, 60, 120]))
                .save(card.join(format!("IMG_{ix}.JPG")))
                .unwrap();
        }
        let library = Arc::new(
            Library::create_or_open(
                LibraryInfo {
                    id: "detect-status".into(),
                    name: "Test".into(),
                    path: library_dir,
                    read_only: true,
                },
                &config_dir,
            )
            .unwrap(),
        );
        library.import(&[card]).unwrap();
        let (endpoint, _) = slow_vision_server(std::time::Duration::from_millis(150));
        let plugins = Plugins::load(None).unwrap();
        let plugin = plugins.detector("openai-vision").unwrap().clone();
        let config = vec![
            ("endpoint".to_owned(), endpoint),
            ("model".to_owned(), "vision-model".to_owned()),
        ];
        let config = plugins::complete_config(&plugin.info.config, config).unwrap();
        let detector = Detector::Plugin { plugin, config };
        library.set_detector(detector.key(), Some(detector));
        assert_eq!(library.detection_status(), None);

        let scanning = std::thread::spawn({
            let library = library.clone();
            move || library.detect_missing_objects().unwrap()
        });
        let mut seen = Vec::new();
        while !scanning.is_finished() {
            if let Some(status) = library.detection_status()
                && seen.last() != Some(&status)
            {
                seen.push(status);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(scanning.join().unwrap(), 2);
        assert_eq!(
            seen,
            [
                DetectionStatus::Looking { done: 0, total: 2 },
                DetectionStatus::Looking { done: 1, total: 2 },
            ]
        );
        assert_eq!(library.detection_status(), None);
        test_support::remove(config_dir.parent().unwrap());
    }
}

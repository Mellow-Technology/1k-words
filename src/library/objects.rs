//! Finding objects in a library's photos (see `detect`), and recording
//! them in its catalog.

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::{Result, bail};
use image::DynamicImage;

use super::Library;
use crate::detect::{self, DetectedObject};
use crate::photos::Photo;

/// A kind of object found in a library's photos, such as "dog".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObjectKind {
    /// The model's name for it.
    pub label: String,
    /// How many photos it's in.
    pub photos: usize,
}

impl Library {
    /// Looks for objects in the photos the current model hasn't looked at,
    /// oldest first, returning how many it looked at. Downloads the model
    /// first if it's needed. If it's already running, it's asked to look
    /// again when done instead. Blocks until done; it stops early if the
    /// library's folder goes (such as when it's moved).
    pub fn detect_missing_objects(&self) -> Result<usize> {
        self.detecting.run(|| {
            let missing = self.missing_objects()?;
            if missing.is_empty() {
                return Ok(0);
            }
            detect::download_model()?;
            let mut scanned = 0;
            for photo in missing {
                if !self.info.path.is_dir() {
                    bail!("{} isn't there", self.info.path.display());
                }
                let image = match self.detection_image(&photo) {
                    Ok(image) => image,
                    // Such as a damaged file.
                    Err(_) => continue,
                };
                let objects = detect::detect(&image)?;
                self.catalog
                    .set_objects(&self.relative(&photo.path)?, detect::MODEL, &objects)?;
                scanned += 1;
            }
            Ok(scanned)
        })
    }

    /// The objects found in `photo`, most certain first, looking now if
    /// they haven't been looked for yet and the model's been downloaded.
    /// None if they haven't been and can't be, as for photos that aren't in
    /// the library. Blocks for as long as that takes.
    pub fn objects(&self, photo: &Photo) -> Result<Option<Vec<DetectedObject>>> {
        let Ok(original) = self.relative(&photo.path) else {
            return Ok(None);
        };
        if let Some(objects) = self.catalog.objects(&original, detect::MODEL)? {
            return Ok(Some(objects));
        }
        if !detect::model_available() {
            return Ok(None);
        }
        let objects = detect::detect(&self.detection_image(photo)?)?;
        self.catalog
            .set_objects(&original, detect::MODEL, &objects)?;
        Ok(Some(objects))
    }

    /// Records objects as if the model had found them in `photo`.
    #[cfg(test)]
    pub fn set_objects(&self, photo: &Photo, objects: &[DetectedObject]) -> Result<()> {
        self.catalog
            .set_objects(&self.relative(&photo.path)?, detect::MODEL, objects)
    }

    /// Every kind of object found in the library, those in the most photos
    /// first.
    pub fn object_kinds(&self) -> Result<Vec<ObjectKind>> {
        self.catalog.object_kinds(detect::MODEL)
    }

    /// The photos with objects that `query` names, such as "dog" or
    /// "people" (see `matching_labels`).
    pub fn search_objects(&self, query: &str) -> Result<HashSet<PathBuf>> {
        let labels: Vec<String> = self
            .object_kinds()?
            .into_iter()
            .map(|kind| kind.label)
            .collect();
        let labels = matching_labels(query, &labels);
        Ok(self
            .catalog
            .photos_with_objects(&labels, detect::MODEL)?
            .into_iter()
            .map(|path| self.info.path.join(path))
            .collect())
    }

    /// What the model looks at: the photo's thumbnail, made if need be. The
    /// model scales what it's given to 640 × 640, so the original would be
    /// no better, and is much slower to decode.
    fn detection_image(&self, photo: &Photo) -> Result<DynamicImage> {
        if let Some(path) = &photo.thumbnail
            && let Ok(image) = image::open(path)
        {
            return Ok(image);
        }
        self.make_thumbnail(photo)
    }

    fn missing_objects(&self) -> Result<Vec<Photo>> {
        let scanned = self.catalog.object_scans(detect::MODEL)?;
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

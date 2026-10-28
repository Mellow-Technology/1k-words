//! Human-readable photo information for the viewer's sidebar.

use std::path::Path;

use exif::{Exif, Tag, Value};
use gpui_kit::SharedString;

use crate::detect::DetectedObject;
use crate::location::Location;
use crate::photos::{Photo, ascii_field, read_exif};

pub struct DetailSection {
    pub title: SharedString,
    pub rows: Vec<(SharedString, SharedString)>,
}

/// Reads the photo's file information and EXIF data. Fields the photo
/// doesn't have are omitted, as are sections that end up empty.
pub fn photo_details(photo: &Photo) -> Vec<DetailSection> {
    let mut file = vec![row("Name", file_name(&photo.path))];
    if let Some(raw_pair) = &photo.raw_pair {
        file.push(row("Raw File", file_name(raw_pair)));
    }
    file.push(row(
        "Dimensions",
        format!("{} × {}", photo.width, photo.height),
    ));
    if let Ok(metadata) = std::fs::metadata(&photo.path) {
        file.push(row("Size", format_bytes(metadata.len())));
    }
    if let Some(raw_pair) = &photo.raw_pair
        && let Ok(metadata) = std::fs::metadata(raw_pair)
    {
        file.push(row("Raw Size", format_bytes(metadata.len())));
    }
    if let Some(video) = &photo.live_video {
        file.push(row("Live Video", file_name(video)));
        if let Ok(metadata) = std::fs::metadata(video) {
            file.push(row("Video Size", format_bytes(metadata.len())));
        }
    }

    let mut sections = vec![DetailSection {
        title: "File".into(),
        rows: file,
    }];
    let exif = read_exif(&photo.path);
    let location = exif
        .as_ref()
        .and_then(Location::from_exif)
        .or_else(|| Location::find(photo.raw_pair.as_deref(), photo.live_video.as_deref()));
    let Some(exif) = exif else {
        sections.extend(location.map(location_section));
        sections.retain(|section| !section.rows.is_empty());
        return sections;
    };

    let camera = [Tag::Make, Tag::Model]
        .into_iter()
        .filter_map(|tag| ascii(&exif, tag))
        .collect::<Vec<_>>()
        .join(" ");
    let lens = ascii(&exif, Tag::LensModel)
        .or_else(|| ascii(&exif, Tag::LensMake))
        .unwrap_or_default();

    sections.push(section(
        "Capture",
        [
            ("Date Taken", display(&exif, Tag::DateTimeOriginal)),
            ("Camera", Some(camera)),
            ("Lens", Some(lens)),
        ],
    ));

    sections.extend(location.map(location_section));

    let focal_length = display(&exif, Tag::FocalLength).map(|focal| {
        match display(&exif, Tag::FocalLengthIn35mmFilm) {
            Some(equivalent) if equivalent != focal => format!("{focal} ({equivalent} equiv.)"),
            _ => focal,
        }
    });
    sections.push(section(
        "Exposure",
        [
            ("Shutter Speed", display(&exif, Tag::ExposureTime)),
            ("Aperture", display(&exif, Tag::FNumber)),
            ("ISO", display(&exif, Tag::PhotographicSensitivity)),
            ("Compensation", exposure_bias(&exif)),
            ("Focal Length", focal_length),
            ("Program", display(&exif, Tag::ExposureProgram)),
            ("Mode", display(&exif, Tag::ExposureMode)),
            ("Metering", display(&exif, Tag::MeteringMode)),
            ("Flash", display(&exif, Tag::Flash)),
            ("White Balance", display(&exif, Tag::WhiteBalance)),
        ],
    ));

    sections.retain(|section| !section.rows.is_empty());
    sections
}

/// The objects found in a photo, one row for each kind, the most common
/// first, e.g. `Person` · `3 (best 91%)`. None if nothing was found.
pub fn objects_section(objects: &[DetectedObject]) -> Option<DetailSection> {
    // Grouped by label, keeping the order they were found in (most certain
    // first) among kinds that are equally common.
    let mut kinds: Vec<(&str, usize, f32)> = Vec::new();
    for object in objects {
        match kinds.iter_mut().find(|(label, ..)| *label == object.label) {
            Some((_, count, best)) => {
                *count += 1;
                *best = best.max(object.confidence);
            }
            None => kinds.push((&object.label, 1, object.confidence)),
        }
    }
    kinds.sort_by(|a, b| b.1.cmp(&a.1));
    let rows: Vec<_> = kinds
        .into_iter()
        .map(|(label, count, best)| {
            let percent = (best * 100.).round();
            let value = match count {
                1 => format!("{percent}%"),
                _ => format!("{count} (best {percent}%)"),
            };
            (capitalize(label).into(), value.into())
        })
        .collect();
    (!rows.is_empty()).then(|| DetailSection {
        title: "Objects".into(),
        rows,
    })
}

pub fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// The main facts about a photo, for the quick preview.
pub struct PhotoSummary {
    pub date_taken: Option<String>,
    /// Camera and lens, e.g. `Apple iPhone 15 Pro · 24 mm f/1.78`.
    pub equipment: Option<String>,
    /// e.g. `["1/125 s", "f/8", "ISO 100", "24 mm", "+0.7 EV"]`.
    pub exposure: Vec<String>,
}

/// Reads the summary from the photo's EXIF data, falling back to the
/// metadata read when the folder was scanned (e.g. for raw files).
pub fn photo_summary(photo: &Photo) -> PhotoSummary {
    let exif = read_exif(&photo.path);
    let exif = exif.as_ref();
    let field = |tag| exif.and_then(|exif| display(exif, tag));
    let meta = &photo.meta;

    let lens = exif.and_then(|exif| ascii(exif, Tag::LensModel));
    let camera = exif
        .map(|exif| {
            [Tag::Make, Tag::Model]
                .into_iter()
                .filter_map(|tag| ascii(exif, tag))
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|camera| !camera.is_empty())
        .or_else(|| meta.camera.clone());
    let equipment = [camera, lens].into_iter().flatten().collect::<Vec<_>>();

    let exposure = [
        meta.exposure_time.map(format_shutter_speed),
        meta.f_number.map(|f| format!("f/{}", format_decimal(f))),
        meta.iso.map(|iso| format!("ISO {iso}")),
        field(Tag::FocalLength),
        meta.exposure_bias.map(format_ev),
    ];
    PhotoSummary {
        date_taken: field(Tag::DateTimeOriginal),
        equipment: (!equipment.is_empty()).then(|| equipment.join(" · ")),
        exposure: exposure.into_iter().flatten().collect(),
    }
}

/// e.g. `1/125 s` or `2.5 s`.
fn format_shutter_speed(seconds: f64) -> String {
    if seconds > 0. && seconds < 0.5 {
        format!("1/{:.0} s", 1. / seconds)
    } else {
        format!("{} s", format_decimal(seconds))
    }
}

/// One decimal place, dropping a trailing `.0`.
fn format_decimal(value: f64) -> String {
    format!("{value:.1}").trim_end_matches(".0").to_string()
}

fn location_section(location: Location) -> DetailSection {
    section(
        "Location",
        [
            ("Coordinates", Some(location.coordinates())),
            (
                "Altitude",
                location.altitude.map(|metres| format_metres(metres, "")),
            ),
            (
                "Accuracy",
                location.accuracy.map(|metres| format_metres(metres, "±")),
            ),
        ],
    )
}

/// Whole metres, e.g. `146 m`, with a real minus sign below sea level.
fn format_metres(metres: f64, prefix: &str) -> String {
    let sign = if metres.round() < 0. { "−" } else { prefix };
    format!("{sign}{:.0} m", metres.abs())
}

fn row(label: &'static str, value: impl Into<SharedString>) -> (SharedString, SharedString) {
    (label.into(), value.into())
}

fn section(
    title: &'static str,
    rows: impl IntoIterator<Item = (&'static str, Option<String>)>,
) -> DetailSection {
    DetailSection {
        title: title.into(),
        rows: rows
            .into_iter()
            .filter_map(|(label, value)| {
                let value = value?;
                (!value.is_empty()).then(|| row(label, value))
            })
            .collect(),
    }
}

fn ascii(exif: &Exif, tag: Tag) -> Option<String> {
    let value = String::from_utf8_lossy(ascii_field(exif, tag)?)
        .trim_matches(|c: char| c.is_whitespace() || c == '\0')
        .to_string();
    (!value.is_empty()).then_some(value)
}

/// Formats a field using kamadak-exif's per-tag formatting and units.
fn display(exif: &Exif, tag: Tag) -> Option<String> {
    let field = exif.get_field(tag, exif::In::PRIMARY)?;
    if let Value::Ascii(_) = field.value
        && tag != Tag::DateTimeOriginal
    {
        return ascii(exif, tag);
    }
    Some(field.display_value().with_unit(exif).to_string())
}

fn exposure_bias(exif: &Exif) -> Option<String> {
    let field = exif.get_field(Tag::ExposureBiasValue, exif::In::PRIMARY)?;
    let Value::SRational(values) = &field.value else {
        return display(exif, Tag::ExposureBiasValue);
    };
    let bias = values.first()?.to_f64();
    Some(format_ev(bias))
}

pub fn format_ev(bias: f64) -> String {
    format!("{} EV", format_ev_value(bias))
}

/// An exposure compensation value without the unit, e.g. `+2` or `−0.7`.
pub fn format_ev_value(bias: f64) -> String {
    if bias.abs() < 0.05 {
        "0".into()
    } else {
        // Use a real minus sign so positive and negative values line up.
        let sign = if bias > 0.0 { "+" } else { "−" };
        let magnitude = format!("{:.1}", bias.abs());
        let magnitude = magnitude.trim_end_matches(".0");
        format!("{sign}{magnitude}")
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000.0 && unit < UNITS.len() - 1 {
        size /= 1000.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_exposure_bias() {
        assert_eq!(format_ev(0.0), "0 EV");
        assert_eq!(format_ev(2.0), "+2 EV");
        assert_eq!(format_ev(-0.7), "−0.7 EV");
        assert_eq!(format_ev_value(2.0), "+2");
    }

    #[test]
    fn formats_exposure_summary() {
        assert_eq!(format_shutter_speed(1. / 125.), "1/125 s");
        assert_eq!(format_shutter_speed(1. / 3.), "1/3 s");
        assert_eq!(format_shutter_speed(2.5), "2.5 s");
        assert_eq!(format_shutter_speed(1.), "1 s");
        assert_eq!(format_decimal(8.0), "8");
        assert_eq!(format_decimal(1.78), "1.8");
    }

    #[test]
    fn formats_metres() {
        assert_eq!(format_metres(146.094, ""), "146 m");
        assert_eq!(format_metres(-12.4, ""), "−12 m");
        assert_eq!(format_metres(17.78, "±"), "±18 m");
    }

    #[test]
    fn groups_objects() {
        let object = |label: &str, confidence| DetectedObject {
            label: label.into(),
            confidence,
            bounds: Some([0., 0., 1., 1.]),
        };
        assert!(objects_section(&[]).is_none());
        let section = objects_section(&[
            object("dog", 0.95),
            object("person", 0.91),
            object("baseball bat", 0.5),
            object("person", 0.66),
            object("person", 0.7),
        ])
        .unwrap();
        let rows: Vec<_> = section
            .rows
            .iter()
            .map(|(label, value)| (label.as_ref(), value.as_ref()))
            .collect();
        assert_eq!(
            rows,
            [
                ("Person", "3 (best 91%)"),
                ("Dog", "95%"),
                ("Baseball bat", "50%"),
            ]
        );
    }

    #[test]
    fn formats_file_sizes() {
        assert_eq!(format_bytes(512), "512 bytes");
        assert_eq!(format_bytes(2_400_000), "2.4 MB");
    }
}

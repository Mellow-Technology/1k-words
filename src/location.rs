//! Where a photo was taken, from its EXIF GPS data or, for Live Photos, the
//! video's QuickTime location tag.

use std::path::Path;

use exif::{Exif, In, Tag, Value};

use crate::photos::read_exif;
use crate::video;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Location {
    /// Degrees, positive north.
    pub latitude: f64,
    /// Degrees, positive east.
    pub longitude: f64,
    /// Metres above sea level.
    pub altitude: Option<f64>,
    /// Horizontal accuracy in metres.
    pub accuracy: Option<f64>,
}

impl Location {
    /// The first location found in the images' EXIF data, then in the
    /// video. EXIF comes first as it's more precise than the video tag.
    pub fn find<'a>(
        images: impl IntoIterator<Item = &'a Path>,
        video: Option<&Path>,
    ) -> Option<Self> {
        images
            .into_iter()
            .find_map(|path| Self::from_exif(&read_exif(path)?))
            .or_else(|| Self::from_video(video?))
    }

    pub fn from_exif(exif: &Exif) -> Option<Self> {
        let coordinate = |tag, ref_tag, negative: u8| {
            let Value::Rational(dms) = &exif.get_field(tag, In::PRIMARY)?.value else {
                return None;
            };
            let [degrees, minutes, seconds] = dms.get(..3)?.try_into().ok()?;
            let value = degrees.to_f64() + minutes.to_f64() / 60. + seconds.to_f64() / 3600.;
            let sign = match &exif.get_field(ref_tag, In::PRIMARY)?.value {
                Value::Ascii(v) if v.first()?.first() == Some(&negative) => -1.,
                _ => 1.,
            };
            Some(sign * value).filter(|v| v.is_finite())
        };
        let rational = |tag| match &exif.get_field(tag, In::PRIMARY)?.value {
            Value::Rational(v) => Some(v.first()?.to_f64()).filter(|v| v.is_finite()),
            _ => None,
        };
        let below_sea_level = exif
            .get_field(Tag::GPSAltitudeRef, In::PRIMARY)
            .and_then(|field| field.value.get_uint(0))
            == Some(1);

        Some(Self {
            latitude: coordinate(Tag::GPSLatitude, Tag::GPSLatitudeRef, b'S')?,
            longitude: coordinate(Tag::GPSLongitude, Tag::GPSLongitudeRef, b'W')?,
            altitude: rational(Tag::GPSAltitude).map(|a| if below_sea_level { -a } else { a }),
            accuracy: rational(Tag::GPSHPositioningError),
        })
    }

    /// Reads Apple's QuickTime location tag, or the generic one other
    /// phones write.
    pub fn from_video(path: &Path) -> Option<Self> {
        let tags = video::metadata(path).ok()?;
        let tag = |key: &str| {
            tags.iter()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.as_str())
        };
        let iso6709 = tag("com.apple.quicktime.location.ISO6709").or_else(|| tag("location"))?;
        let (latitude, longitude, altitude) = parse_iso6709(iso6709)?;
        Some(Self {
            latitude,
            longitude,
            altitude,
            accuracy: tag("com.apple.quicktime.location.accuracy.horizontal")
                .and_then(|value| value.parse().ok())
                .filter(|value: &f64| value.is_finite()),
        })
    }

    /// E.g. `38.61628° N, 90.25038° W`. Five decimal places is about a metre.
    pub fn coordinates(&self) -> String {
        let hemisphere = |value: f64, positive, negative| {
            format!(
                "{:.5}° {}",
                value.abs(),
                if value < 0. { negative } else { positive }
            )
        };
        format!(
            "{}, {}",
            hemisphere(self.latitude, "N", "S"),
            hemisphere(self.longitude, "E", "W")
        )
    }
}

/// Parses an ISO 6709 point such as `+38.6163-090.2504+146.094/`: latitude,
/// longitude and an optional altitude, with degrees given as `±DD.D`,
/// `±DDMM.M` or `±DDMMSS.S`.
fn parse_iso6709(text: &str) -> Option<(f64, f64, Option<f64>)> {
    let text = text.trim().trim_end_matches('/');
    // Split before each sign, e.g. ["+38.6163", "-090.2504", "+146.094"].
    let mut parts = Vec::new();
    let mut start = 0;
    for (index, c) in text.char_indices().skip(1) {
        if c == '+' || c == '-' {
            parts.push(&text[start..index]);
            start = index;
        }
    }
    parts.push(&text[start..]);
    // A coordinate reference system may follow, e.g. `CRSWGS_84`.
    let altitude = parts
        .get(2)
        .map(|part| part.split("CRS").next().unwrap_or(part));

    let latitude = parse_degrees(parts.first()?, 2)?;
    let longitude = parse_degrees(parts.get(1)?, 3)?;
    let altitude = altitude.and_then(|a| a.parse().ok());
    ((-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude))
        .then_some((latitude, longitude, altitude))
}

/// One signed ISO 6709 angle whose degrees take `degree_digits` digits.
fn parse_degrees(part: &str, degree_digits: usize) -> Option<f64> {
    let (sign, digits) = match part.as_bytes().first()? {
        b'+' => (1., &part[1..]),
        b'-' => (-1., &part[1..]),
        _ => return None,
    };
    let integer_digits = digits.find('.').unwrap_or(digits.len());
    let value: f64 = digits.parse().ok()?;
    let degrees = match integer_digits - degree_digits {
        0 => value,
        2 => (value / 100.).trunc() + (value % 100.) / 60.,
        4 => {
            (value / 10_000.).trunc()
                + ((value / 100.).trunc() % 100.) / 60.
                + (value % 100.) / 3600.
        }
        _ => return None,
    };
    Some(sign * degrees)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_iso6709() {
        assert_eq!(
            parse_iso6709("+38.6163-090.2504+146.094/"),
            Some((38.6163, -90.2504, Some(146.094)))
        );
        assert_eq!(
            parse_iso6709("-33.8688+151.2093/"),
            Some((-33.8688, 151.2093, None))
        );
        assert_eq!(
            parse_iso6709("+40.7128-074.0060+010.5CRSWGS_84/"),
            Some((40.7128, -74.006, Some(10.5)))
        );
        // Degrees and minutes, and degrees, minutes and seconds.
        let (lat, lon, _) = parse_iso6709("+4030.0-07400.0/").unwrap();
        assert!((lat - 40.5).abs() < 1e-9 && (lon + 74.0).abs() < 1e-9);
        let (lat, lon, _) = parse_iso6709("+403000-0740036/").unwrap();
        assert!((lat - 40.5).abs() < 1e-9 && (lon + 74.01).abs() < 1e-9);

        assert_eq!(parse_iso6709(""), None);
        assert_eq!(parse_iso6709("+95.0+000.0/"), None);
    }

    /// Reads both halves of a real Live Photo: set `LIVE_PHOTO_VIDEO` to the
    /// video, with its photo (`.HEIC`) next to it.
    #[test]
    fn reads_live_photo_location() {
        let Some(video) = std::env::var_os("LIVE_PHOTO_VIDEO") else {
            eprintln!("skipping: LIVE_PHOTO_VIDEO not set");
            return;
        };
        let video = std::path::PathBuf::from(video);
        let from_video = Location::from_video(&video).expect("video location");
        let from_photo = Location::from_exif(&read_exif(&video.with_extension("HEIC")).unwrap())
            .expect("photo location");
        eprintln!("video: {from_video:?}\nphoto: {from_photo:?}");
        // The video tag has 4 decimal places (about 10 m).
        assert!((from_video.latitude - from_photo.latitude).abs() < 1e-4);
        assert!((from_video.longitude - from_photo.longitude).abs() < 1e-4);
        assert!(from_photo.altitude.is_some() && from_video.accuracy.is_some());
    }

    #[test]
    fn formats_coordinates() {
        let location = Location {
            latitude: -33.868_82,
            longitude: 151.209_29,
            altitude: None,
            accuracy: None,
        };
        assert_eq!(location.coordinates(), "33.86882° S, 151.20929° E");
    }
}

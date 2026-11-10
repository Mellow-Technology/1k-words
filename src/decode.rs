//! Decoding photos into GPU-ready textures.
//!
//! Every photo can be shown at two qualities:
//!
//! - An **embedded preview**: the JPEG a camera stores inside a raw file
//!   (typically 1-2 megapixels), the thumbnail inside a HEIC file, or the
//!   small EXIF thumbnail inside a JPEG.
//!   It takes milliseconds to read.
//! - The **full image**: the decoded JPEG, or the developed raw file. It
//!   takes a few hundred milliseconds for a 24 megapixel photo.
//!
//! The gallery and viewer show the preview first and replace it with an
//! image generated from the full decode.
//!
//! Full decodes take a lot of memory while they run (a few hundred MB for
//! a 24 megapixel raw file), so those running at once are held to
//! `DECODE_BUDGET` between them.

use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};

use anyhow::Result;
use gpui_kit::{App, RenderImage};
use image::metadata::Orientation;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader, RgbaImage};

use crate::heif::{self, is_heif};
use crate::photos::{Photo, read_exif};
use crate::raw::{self, is_raw};
use crate::video;

/// Gallery thumbnails are decoded at this width (in device pixels), which
/// is enough for a sharp masonry column on a 2x display.
pub const THUMBNAIL_WIDTH: u32 = 560;
/// Filmstrip thumbnails in the viewer are decoded at this height.
pub const FILMSTRIP_THUMBNAIL_HEIGHT: u32 = 180;
/// The quick preview shows photos scaled to fit this size, and the previews
/// and posters a library keeps are made to fit it.
pub const VIEWER_PREVIEW_SIZE: u32 = 2048;
/// Embedded previews smaller than this (on their long side) aren't worth
/// showing in the viewer; the gallery thumbnail is about as good.
const MIN_VIEWER_PREVIEW_SIZE: u32 = 1000;
/// The largest texture GPUI's atlas can hold.
const MAX_TEXTURE_SIZE: u32 = 16384;

/// The most memory, roughly, that full decodes running at once may take
/// between them: about three 24 megapixel raw files, or a dozen JPEGs.
/// Decodes wait for room, but one always runs, however big.
const DECODE_BUDGET: u64 = 1536 << 20;
/// Bytes of decodes running (estimated by `decode_cost`).
static DECODING: Mutex<u64> = Mutex::new(0);
static DECODE_DONE: Condvar = Condvar::new();

/// The photo's embedded preview, oriented, if it has one. Raw+JPEG pairs use
/// the raw file's preview, which is much larger than a JPEG's EXIF
/// thumbnail.
pub fn embedded_preview(photo: &Photo) -> Option<DynamicImage> {
    if photo.raw_file().is_none() && is_heif(&photo.path) {
        // Already oriented by libheif.
        return heif::thumbnail(&photo.path).ok().flatten();
    }
    let (source, preview) = match photo.raw_file() {
        Some(raw_file) => (raw_file, raw::embedded_preview(raw_file).ok().flatten()?),
        None => (photo.path.as_path(), exif_thumbnail(&photo.path)?),
    };
    let mut preview = preview;
    preview.apply_orientation(orientation(source));
    Some(preview)
}

/// Decodes the photo's displayed file at full resolution, oriented. Raw
/// files are developed with rawler.
pub fn decode_full(photo: &Photo) -> Result<DynamicImage> {
    let _room = DecodeRoom::wait(decode_cost(photo));
    let path = photo.path.as_path();
    if is_raw(path) {
        let mut image = raw::develop(path)?;
        image.apply_orientation(orientation(path));
        return Ok(image);
    }
    if is_heif(path) {
        return heif::decode(path);
    }
    let mut decoder = ImageReader::open(path)?
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    Ok(image)
}

/// A gallery thumbnail from the embedded preview, if the photo has one.
pub fn preview_thumbnail(photo: &Photo) -> Option<Arc<RenderImage>> {
    let preview = embedded_preview(photo)?;
    Some(to_render_image(fit(preview, THUMBNAIL_WIDTH, u32::MAX)))
}

/// A gallery thumbnail as a library would keep it (see `thumbnail_image`).
pub fn full_thumbnail(photo: &Photo) -> Result<Arc<RenderImage>> {
    Ok(to_render_image(thumbnail_image(photo)?))
}

/// The thumbnail a library keeps for a photo (see `library::derivatives`):
/// the embedded preview when it's at least as wide as a thumbnail, as a raw
/// file's is, otherwise made from the full image.
pub fn thumbnail_image(photo: &Photo) -> Result<DynamicImage> {
    let image = match embedded_preview(photo) {
        Some(preview) if preview.width() >= THUMBNAIL_WIDTH => preview,
        _ => decode_full(photo)?,
    };
    Ok(fit(image, THUMBNAIL_WIDTH, u32::MAX))
}

/// The preview a library keeps for a photo (see `library::derivatives`):
/// the embedded preview only when it's as big as a preview, as this may end
/// up being the best copy on the device, otherwise made from the full image.
pub fn preview_image(photo: &Photo) -> Result<DynamicImage> {
    let image = match embedded_preview(photo) {
        Some(preview) if preview.width().max(preview.height()) >= VIEWER_PREVIEW_SIZE => preview,
        _ => decode_full(photo)?,
    };
    Ok(fit(image, VIEWER_PREVIEW_SIZE, VIEWER_PREVIEW_SIZE))
}

/// The poster a library keeps for a Live Photo's video (see
/// `library::derivatives`): its first frame.
pub fn poster_image(video: &Path) -> Result<DynamicImage> {
    Ok(fit(
        video::first_frame(video)?,
        VIEWER_PREVIEW_SIZE,
        VIEWER_PREVIEW_SIZE,
    ))
}

/// A thumbnail the library made earlier, if it's there.
pub fn kept_thumbnail(path: &Path) -> Option<Arc<RenderImage>> {
    let image = ImageReader::open(path).ok()?.decode().ok()?;
    Some(to_render_image(image))
}

/// A filmstrip thumbnail, from the embedded preview when it's large enough.
pub fn filmstrip_thumbnail(photo: &Photo) -> Result<Arc<RenderImage>> {
    let image = match embedded_preview(photo) {
        Some(preview) if preview.height().min(preview.width()) >= FILMSTRIP_THUMBNAIL_HEIGHT => {
            preview
        }
        _ => decode_full(photo)?,
    };
    Ok(to_render_image(fit(
        image,
        u32::MAX,
        FILMSTRIP_THUMBNAIL_HEIGHT,
    )))
}

/// The embedded preview at viewer preview size, if it's large enough to be
/// worth showing.
pub fn viewer_embedded_preview(photo: &Photo) -> Option<Arc<RenderImage>> {
    let preview = embedded_preview(photo)?;
    if preview.width().max(preview.height()) < MIN_VIEWER_PREVIEW_SIZE {
        return None;
    }
    Some(to_render_image(fit(
        preview,
        VIEWER_PREVIEW_SIZE,
        VIEWER_PREVIEW_SIZE,
    )))
}

/// The quick preview's image: the embedded preview when it's large enough,
/// otherwise the photo scaled to fit `VIEWER_PREVIEW_SIZE`.
pub fn quick_preview(photo: &Photo) -> Result<Arc<RenderImage>> {
    if let Some(preview) = viewer_embedded_preview(photo) {
        return Ok(preview);
    }
    Ok(to_render_image(fit(
        decode_full(photo)?,
        VIEWER_PREVIEW_SIZE,
        VIEWER_PREVIEW_SIZE,
    )))
}

/// The full image scaled down (never up) to fit `max_width` x
/// `max_height`, or at full resolution, if that's no bigger (and doesn't
/// exceed the maximum texture size). Returns whether it's at full
/// resolution.
pub fn viewer_image(
    image: DynamicImage,
    max_width: u32,
    max_height: u32,
) -> (Arc<RenderImage>, bool) {
    let full = image.width() <= max_width && image.height() <= max_height;
    let image = fit(
        image,
        max_width.min(MAX_TEXTURE_SIZE),
        max_height.min(MAX_TEXTURE_SIZE),
    );
    (to_render_image(image), full)
}

/// Scales `image` down (never up) to fit within `max_width` x `max_height`.
fn fit(image: DynamicImage, max_width: u32, max_height: u32) -> DynamicImage {
    let (width, height) = (image.width() as f64, image.height() as f64);
    let scale = (max_width as f64 / width).min(max_height as f64 / height);
    if scale >= 1.0 {
        return image;
    }
    let new_width = ((width * scale).round() as u32).max(1);
    let new_height = ((height * scale).round() as u32).max(1);
    image.thumbnail_exact(new_width, new_height)
}

/// Removes `image` from every window's GPU atlas once it's no longer
/// shown. `App::drop_image` skips the window being updated, as in a key
/// handler, unless it's given; deferred, every window's there.
pub fn release(image: Arc<RenderImage>, cx: &mut App) {
    cx.defer(move |cx| cx.drop_image(image, None));
}

/// Converts to the BGRA layout GPUI expects. Images without alpha are
/// widened in place, rather than copied, which would briefly take both.
pub fn to_render_image(image: DynamicImage) -> Arc<RenderImage> {
    Arc::new(RenderImage::new([Frame::new(to_bgra(image))]))
}

/// The image's pixels in the BGRA layout GPUI expects (see
/// `to_render_image`).
pub fn to_bgra(image: DynamicImage) -> RgbaImage {
    if image.color().has_alpha() {
        let mut buffer = image.into_rgba8();
        for pixel in buffer.as_chunks_mut::<4>().0 {
            pixel.swap(0, 2);
        }
        buffer
    } else {
        rgb_to_bgra(image.into_rgb8())
    }
}

fn rgb_to_bgra(image: image::RgbImage) -> image::RgbaImage {
    let (width, height) = image.dimensions();
    let pixels = width as usize * height as usize;
    let mut data = image.into_raw();
    data.resize(pixels * 4, 0);
    // From the end, so each pixel's read before it's written over: pixel
    // `ix` is written at `4 * ix`, at or after where it, and every pixel
    // before it, was read from.
    for ix in (0..pixels).rev() {
        let [r, g, b] = [data[3 * ix], data[3 * ix + 1], data[3 * ix + 2]];
        data[4 * ix..4 * ix + 4].copy_from_slice(&[b, g, r, u8::MAX]);
    }
    image::RgbaImage::from_raw(width, height, data).expect("sized above")
}

/// Roughly how much memory decoding the photo takes at its peak, from its
/// size: developing a raw file holds the sensor data, a floating-point
/// copy and the 16-bit result at once.
fn decode_cost(photo: &Photo) -> u64 {
    let pixels = match u64::from(photo.width) * u64::from(photo.height) {
        // Not known: guess at a typical camera's.
        0 => 24_000_000,
        pixels => pixels,
    };
    let bytes_per_pixel = if is_raw(&photo.path) { 20 } else { 5 };
    pixels * bytes_per_pixel
}

/// Room for a decode in `DECODE_BUDGET`, while it runs.
struct DecodeRoom(u64);

impl DecodeRoom {
    /// Waits until there's room for `cost`, or nothing else is decoding.
    fn wait(cost: u64) -> Self {
        let mut decoding = DECODING.lock().unwrap_or_else(|e| e.into_inner());
        while *decoding > 0 && *decoding + cost > DECODE_BUDGET {
            decoding = DECODE_DONE
                .wait(decoding)
                .unwrap_or_else(|e| e.into_inner());
        }
        *decoding += cost;
        Self(cost)
    }
}

impl Drop for DecodeRoom {
    fn drop(&mut self) {
        *DECODING.lock().unwrap_or_else(|e| e.into_inner()) -= self.0;
        DECODE_DONE.notify_all();
    }
}

/// The EXIF orientation of `path`. Raw files whose EXIF the EXIF reader
/// can't parse fall back to rawler's metadata.
fn orientation(path: &Path) -> Orientation {
    let value = match read_exif(path) {
        Some(exif) => exif
            .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
            .and_then(|field| field.value.get_uint(0)),
        None if is_raw(path) => raw::metadata(path)
            .ok()
            .and_then(|metadata| metadata.exif.orientation)
            .map(u32::from),
        None => None,
    };
    value
        .and_then(|value| Orientation::from_exif(value as u8))
        .unwrap_or(Orientation::NoTransforms)
}

/// The thumbnail stored in a JPEG's EXIF data (typically 160x120).
fn exif_thumbnail(path: &Path) -> Option<DynamicImage> {
    let exif = read_exif(path)?;
    let field = |tag| {
        exif.get_field(tag, exif::In::THUMBNAIL)?
            .value
            .get_uint(0)
            .map(|v| v as usize)
    };
    let offset = field(exif::Tag::JPEGInterchangeFormat)?;
    let length = field(exif::Tag::JPEGInterchangeFormatLength)?;
    let data = exif.buf().get(offset..offset.checked_add(length)?)?;
    image::load_from_memory_with_format(data, image::ImageFormat::Jpeg).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widens_rgb_to_bgra() {
        let rgb = image::RgbImage::from_fn(3, 2, |x, y| {
            image::Rgb([x as u8 * 10 + y as u8, 100 + x as u8, 200 + y as u8])
        });
        let bgra = rgb_to_bgra(rgb.clone());
        for (x, y, &image::Rgb([r, g, b])) in rgb.enumerate_pixels() {
            assert_eq!(bgra.get_pixel(x, y).0, [b, g, r, u8::MAX]);
        }
    }

    #[test]
    fn decodes_wait_for_room() {
        let first = DecodeRoom::wait(DECODE_BUDGET);
        let (sender, receiver) = std::sync::mpsc::channel();
        let waiting = std::thread::spawn(move || {
            let _second = DecodeRoom::wait(1);
            sender.send(()).unwrap();
        });
        assert!(
            receiver
                .recv_timeout(std::time::Duration::from_millis(100))
                .is_err()
        );
        drop(first);
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        waiting.join().unwrap();
    }
}

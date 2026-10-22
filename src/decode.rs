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

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use gpui_kit::RenderImage;
use image::metadata::Orientation;
use image::{DynamicImage, Frame, ImageDecoder, ImageReader};

use crate::heif::{self, is_heif};
use crate::photos::{Photo, read_exif};
use crate::raw::{self, is_raw};

/// Gallery thumbnails are decoded at this width (in device pixels), which
/// is enough for a sharp masonry column on a 2x display.
pub const THUMBNAIL_WIDTH: u32 = 560;
/// Filmstrip thumbnails in the viewer are decoded at this height.
pub const FILMSTRIP_THUMBNAIL_HEIGHT: u32 = 180;
/// The viewer first shows the photo scaled to fit this size, then swaps in
/// the full resolution image.
pub const VIEWER_PREVIEW_SIZE: u32 = 2048;
/// Embedded previews smaller than this (on their long side) aren't worth
/// showing in the viewer; the gallery thumbnail is about as good.
const MIN_VIEWER_PREVIEW_SIZE: u32 = 1000;
/// The largest texture GPUI's atlas can hold.
const MAX_TEXTURE_SIZE: u32 = 16384;

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

/// A gallery thumbnail generated from the full image.
pub fn full_thumbnail(photo: &Photo) -> Result<Arc<RenderImage>> {
    Ok(to_render_image(fit(
        decode_full(photo)?,
        THUMBNAIL_WIDTH,
        u32::MAX,
    )))
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

/// The full image scaled down to viewer preview size.
pub fn viewer_preview(image: &DynamicImage) -> Arc<RenderImage> {
    to_render_image(fit(image.clone(), VIEWER_PREVIEW_SIZE, VIEWER_PREVIEW_SIZE))
}

/// The full image at full resolution (unless it exceeds the maximum
/// texture size).
pub fn viewer_full(image: DynamicImage) -> Arc<RenderImage> {
    to_render_image(fit(image, MAX_TEXTURE_SIZE, MAX_TEXTURE_SIZE))
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

/// Converts to the BGRA layout GPUI expects.
pub fn to_render_image(image: DynamicImage) -> Arc<RenderImage> {
    let mut buffer = image.into_rgba8();
    for pixel in buffer.as_chunks_mut::<4>().0 {
        pixel.swap(0, 2);
    }
    Arc::new(RenderImage::new([Frame::new(buffer)]))
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

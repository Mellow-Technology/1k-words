//! HEIC/HEIF support via libheif.
//!
//! HEIF stores rotation and mirroring as container properties, and libheif
//! applies them while decoding. The EXIF orientation in these files must
//! therefore *not* be applied again.

use std::path::Path;

use anyhow::{Context as _, Result, anyhow};
use image::{DynamicImage, RgbaImage};
use libheif_rs::{ColorSpace, HeifContext, ImageHandle, LibHeif, RgbChroma};

pub const HEIF_EXTENSIONS: &[&str] = &["heic", "heif", "hif"];

pub fn is_heif(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| HEIF_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// Display dimensions, with rotation applied. Only parses the container.
pub fn dimensions(path: &Path) -> Result<(u32, u32)> {
    let context = open(path)?;
    let handle = context.primary_image_handle()?;
    Ok((handle.width(), handle.height()))
}

/// Decodes the primary image, oriented.
pub fn decode(path: &Path) -> Result<DynamicImage> {
    let context = open(path)?;
    to_image(&context.primary_image_handle()?)
}

/// The embedded thumbnail (about 320 pixels on iPhones), oriented.
pub fn thumbnail(path: &Path) -> Result<Option<DynamicImage>> {
    let context = open(path)?;
    let handle = context.primary_image_handle()?;
    let mut ids = vec![0; handle.number_of_thumbnails()];
    handle.thumbnail_ids(&mut ids);
    match ids.first() {
        Some(&id) => Ok(Some(to_image(&handle.thumbnail(id)?)?)),
        None => Ok(None),
    }
}

fn open(path: &Path) -> Result<HeifContext<'static>> {
    let path_str = path
        .to_str()
        .ok_or_else(|| anyhow!("non UTF-8 path: {}", path.display()))?;
    HeifContext::read_from_file(path_str).with_context(|| format!("reading {}", path.display()))
}

fn to_image(handle: &ImageHandle) -> Result<DynamicImage> {
    let image = LibHeif::new().decode(handle, ColorSpace::Rgb(RgbChroma::Rgba), None)?;
    let plane = image
        .planes()
        .interleaved
        .ok_or_else(|| anyhow!("no interleaved RGBA plane"))?;
    let row_bytes = plane.width as usize * 4;
    let mut pixels = Vec::with_capacity(row_bytes * plane.height as usize);
    for row in plane.data.chunks(plane.stride).take(plane.height as usize) {
        pixels.extend_from_slice(&row[..row_bytes]);
    }
    let buffer = RgbaImage::from_raw(plane.width, plane.height, pixels)
        .ok_or_else(|| anyhow!("unexpected HEIF plane size"))?;
    Ok(DynamicImage::ImageRgba8(buffer))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The app icon at 512 px, made HEIC by macOS's encoder (`sips -s format
    /// heic`), so decoding is checked against whichever libheif the app's
    /// built with: Homebrew's, or the LGPL one it ships
    /// (`scripts/build-libheif.sh`).
    const ICON: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/heic/icon.heic");

    #[test]
    fn decodes_heic() {
        let path = Path::new(ICON);
        assert!(is_heif(path));
        assert_eq!(dimensions(path).unwrap(), (512, 512));
        let image = decode(path).unwrap().to_rgb8();
        assert_eq!(image.dimensions(), (512, 512));
        // The icon's graphite, beside its top-left crop mark, and the mark's
        // amber.
        let [r, g, b] = image.get_pixel(80, 256).0;
        assert!(r < 50 && g < 50 && b < 60, "graphite: {r} {g} {b}");
        let [r, g, b] = image.get_pixel(115, 140).0;
        assert!(r > 200 && g > 150 && b < 130, "amber: {r} {g} {b}");
    }
}

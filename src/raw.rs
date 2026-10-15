//! Camera raw support via `rawler`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use anyhow::{Result, anyhow};
use image::DynamicImage;
use rawler::decoders::{Decoder, RawDecodeParams, RawMetadata};
use rawler::imgop::develop::RawDevelop;
use rawler::rawsource::RawSource;

pub const RAW_EXTENSIONS: &[&str] = &[
    "3fr", "ari", "arw", "cr2", "cr3", "crw", "dcr", "dng", "erf", "iiq", "kdc", "mef", "mos",
    "mrw", "nef", "nrw", "orf", "pef", "raf", "rw2", "rwl", "sr2", "srf", "srw", "x3f",
];

pub fn is_raw(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| RAW_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
}

/// The largest JPEG preview the camera embedded in the file, if any. These
/// are typically 1-2 megapixels and take a few milliseconds to decode.
/// Orientation is *not* applied.
pub fn embedded_preview(path: &Path) -> Result<Option<DynamicImage>> {
    with_decoder(path, |decoder, source| {
        let params = RawDecodeParams::default();
        if let Some(preview) = decoder.preview_image(source, &params)? {
            return Ok(Some(preview));
        }
        Ok(decoder.thumbnail_image(source, &params)?)
    })
}

/// Demosaics and develops the raw image to sRGB with rawler's default
/// pipeline. Orientation is *not* applied.
pub fn develop(path: &Path) -> Result<DynamicImage> {
    with_decoder(path, |decoder, source| {
        let raw = decoder.raw_image(source, &RawDecodeParams::default(), false)?;
        RawDevelop::default()
            .develop_intermediate(&raw)?
            .to_dynamic_image()
            .ok_or_else(|| anyhow!("unsupported raw pixel layout"))
    })
}

/// Metadata parsed by rawler, for raw formats the EXIF reader can't parse
/// (e.g. CR3 and RAF).
pub fn metadata(path: &Path) -> Result<RawMetadata> {
    with_decoder(path, |decoder, source| {
        Ok(decoder.raw_metadata(source, &RawDecodeParams::default())?)
    })
}

/// rawler panics (`todo!()`) on some unusual sensor layouts, so contain
/// panics to the file being decoded.
fn with_decoder<T>(
    path: &Path,
    f: impl FnOnce(&dyn Decoder, &RawSource) -> Result<T>,
) -> Result<T> {
    catch_unwind(AssertUnwindSafe(|| {
        let source = RawSource::new(path)?;
        let decoder = rawler::get_decoder(&source)?;
        f(decoder.as_ref(), &source)
    }))
    .unwrap_or_else(|_| Err(anyhow!("raw decoder crashed on {}", path.display())))
}

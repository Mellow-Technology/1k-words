//! The Dock icon. A bundled .app gets it from its `Info.plist`, but a bare
//! binary (`cargo run`) shows the generic executable icon unless it sets one
//! itself. The PNG already has the macOS icon grid's margin, so it sits in
//! the Dock like a bundled icon.
//!
//! While photos upload, the icon's bottom crop marks become a progress bar:
//! a dim track along the frame's bottom edge that fills with amber.

use std::cell::{Cell, OnceCell};
use std::io::Cursor;

use image::{ImageFormat, Rgba, RgbaImage};
use objc2::{AllocAnyThread as _, MainThreadMarker};
use objc2_app_kit::{NSApplication, NSImage};
use objc2_foundation::NSData;

const ICON: &[u8] = include_bytes!("../assets/app-icon/1024x1024.png");
/// The base for progress icons: the Dock draws at most 256pt, so 512px is
/// enough even on Retina, and keeps redrawing cheap.
const PROGRESS_BASE: &[u8] = include_bytes!("../assets/app-icon/512x512.png");

/// The bar on the 1024 grid of `icon.svg`: the frame's bottom edge, from the
/// outside of the left bracket to the outside of the right one, a little
/// thicker than the brackets so it reads at Dock sizes.
const BAR_LEFT: f32 = 212.0;
const BAR_RIGHT: f32 = 812.0;
const BAR_TOP: f32 = 768.0;
const BAR_BOTTOM: f32 = 820.0;
const TRACK: Rgba<u8> = Rgba([0x3A, 0x39, 0x33, 0xFF]);
const FILL: Rgba<u8> = Rgba([0xFF, 0xC2, 0x4B, 0xFF]);

thread_local! {
    static BASE: OnceCell<Option<RgbaImage>> = const { OnceCell::new() };
    /// The whole percent shown, so the icon is only redrawn when it changes.
    static SHOWN: Cell<Option<u32>> = const { Cell::new(None) };
}

/// Sets the Dock icon. Call on the main thread once the app has launched.
pub fn set() {
    set_png(ICON);
}

/// Shows upload progress (0 to 100) on the Dock icon, or the plain icon for
/// `None`. Cheap to call often: it only redraws when the whole percent changes.
pub fn show_progress(percent: Option<f32>) {
    let percent = percent.map(|p| p.clamp(0.0, 100.0).floor() as u32);
    if SHOWN.replace(percent) == percent {
        return;
    }
    let Some(percent) = percent else {
        set();
        return;
    };
    let Some(icon) = BASE.with(|base| {
        let base = base.get_or_init(|| {
            image::load_from_memory_with_format(PROGRESS_BASE, ImageFormat::Png)
                .inspect_err(|error| eprintln!("couldn't decode the Dock icon: {error}"))
                .ok()
                .map(|image| image.to_rgba8())
        });
        base.as_ref().map(|base| with_progress(base, percent as f32 / 100.0))
    }) else {
        return;
    };
    let mut png = Vec::new();
    match icon.write_to(&mut Cursor::new(&mut png), ImageFormat::Png) {
        Ok(()) => set_png(&png),
        Err(error) => eprintln!("couldn't encode the Dock icon: {error}"),
    }
}

/// The icon with a progress bar along its bottom crop marks, `fraction` (0
/// to 1) of it filled.
fn with_progress(base: &RgbaImage, fraction: f32) -> RgbaImage {
    let mut icon = base.clone();
    let scale = icon.width() as f32 / 1024.0;
    let (left, right) = (BAR_LEFT * scale, BAR_RIGHT * scale);
    let (top, bottom) = (
        (BAR_TOP * scale).round() as u32,
        (BAR_BOTTOM * scale).round() as u32,
    );
    let filled_to = left + (right - left) * fraction.clamp(0.0, 1.0);
    for x in left.floor() as u32..right.ceil() as u32 {
        // How much of this pixel column the bar, and the fill, cover, so the
        // bar's ends and the fill's edge are antialiased.
        let span = |end: f32| (end.min(x as f32 + 1.0) - left.max(x as f32)).clamp(0.0, 1.0);
        let (bar, fill) = (span(right), span(filled_to));
        let color = mix(TRACK, FILL, if bar > 0.0 { fill / bar } else { 0.0 });
        for y in top..bottom {
            let pixel = icon.get_pixel_mut(x, y);
            *pixel = mix(*pixel, color, bar);
        }
    }
    icon
}

fn mix(from: Rgba<u8>, to: Rgba<u8>, amount: f32) -> Rgba<u8> {
    Rgba(std::array::from_fn(|i| {
        (from.0[i] as f32 + (to.0[i] as f32 - from.0[i] as f32) * amount).round() as u8
    }))
}

fn set_png(png: &[u8]) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let data = NSData::with_bytes(png);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        eprintln!("couldn't decode the Dock icon");
        return;
    };
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> RgbaImage {
        image::load_from_memory_with_format(PROGRESS_BASE, ImageFormat::Png)
            .unwrap()
            .to_rgba8()
    }

    /// A pixel in the middle of the bar's height, `along` (0 to 1) its length.
    fn bar_pixel(icon: &RgbaImage, along: f32) -> Rgba<u8> {
        let scale = icon.width() as f32 / 1024.0;
        let x = (BAR_LEFT + (BAR_RIGHT - BAR_LEFT) * along) * scale;
        let y = (BAR_TOP + BAR_BOTTOM) / 2.0 * scale;
        *icon.get_pixel(x as u32, y as u32)
    }

    #[test]
    fn fills_the_bar_from_the_left() {
        let icon = with_progress(&base(), 0.5);
        assert_eq!(bar_pixel(&icon, 0.1), FILL);
        assert_eq!(bar_pixel(&icon, 0.45), FILL);
        assert_eq!(bar_pixel(&icon, 0.55), TRACK);
        assert_eq!(bar_pixel(&icon, 0.9), TRACK);
    }

    #[test]
    fn empty_and_full() {
        let base = base();
        let empty = with_progress(&base, 0.0);
        let full = with_progress(&base, 1.0);
        for along in [0.02, 0.5, 0.98] {
            assert_eq!(bar_pixel(&empty, along), TRACK);
            assert_eq!(bar_pixel(&full, along), FILL);
        }
    }

    #[test]
    fn leaves_the_rest_of_the_icon_alone() {
        let base = base();
        let icon = with_progress(&base, 0.5);
        let scale = base.width() as f32 / 1024.0;
        let below = ((BAR_BOTTOM + 8.0) * scale) as u32;
        let above = ((BAR_TOP - 8.0) * scale) as u32;
        for y in [below, above, 0, base.height() / 2] {
            for x in (0..base.width()).step_by(7) {
                assert_eq!(icon.get_pixel(x, y), base.get_pixel(x, y), "({x}, {y})");
            }
        }
    }
}

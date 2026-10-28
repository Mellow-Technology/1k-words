//! A photo at full resolution, shown in tiles.
//!
//! Zoomed in further than the image fitted to the window has pixels for,
//! the viewer decodes the photo at full resolution and keeps its pixels
//! here, on the CPU. Only the tiles in view, and a margin around them, are
//! made into images and so uploaded to the GPU; they're drawn over the
//! fitted image, which shows wherever a tile isn't ready yet. GPU memory is
//! bounded by the window's size rather than the photo's, and photos larger
//! than the largest texture show at full resolution too.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::{Bounds, RenderImage, point, size};
use image::{DynamicImage, Frame, RgbaImage};

use crate::decode;

/// Tiles' width and height in image pixels: big enough that a window needs
/// few, small enough that panning adds little at a time.
pub const TILE_SIZE: u32 = 512;
/// Tiles kept beyond those in view, on every side, so panning a little
/// shows them straight away.
const MARGIN: u32 = 1;
/// Tiles further out than this are released.
const RELEASE_MARGIN: u32 = 2;

/// A tile's column and row.
pub type TileKey = (u32, u32);

/// The photo's pixels at full resolution, in the BGRA layout GPUI draws.
pub struct FullImage {
    pixels: RgbaImage,
}

impl FullImage {
    pub fn new(image: DynamicImage) -> Self {
        Self {
            pixels: decode::to_bgra(image),
        }
    }

    pub fn width(&self) -> u32 {
        self.pixels.width()
    }

    pub fn height(&self) -> u32 {
        self.pixels.height()
    }

    /// The part of the image a tile covers, as fractions of its size.
    pub fn tile_rect(&self, (column, row): TileKey) -> Bounds<f32> {
        let (width, height) = (self.width() as f32, self.height() as f32);
        let x = (column * TILE_SIZE) as f32;
        let y = (row * TILE_SIZE) as f32;
        let right = ((column + 1) * TILE_SIZE).min(self.width()) as f32;
        let bottom = ((row + 1) * TILE_SIZE).min(self.height()) as f32;
        Bounds {
            origin: point(x / width, y / height),
            size: size((right - x) / width, (bottom - y) / height),
        }
    }

    /// Copies a tile's pixels into an image of its own.
    pub fn cut(&self, (column, row): TileKey) -> Arc<RenderImage> {
        let (x, y) = (column * TILE_SIZE, row * TILE_SIZE);
        let width = TILE_SIZE.min(self.width() - x);
        let height = TILE_SIZE.min(self.height() - y);
        let tile = image::imageops::crop_imm(&self.pixels, x, y, width, height).to_image();
        Arc::new(RenderImage::new([Frame::new(tile)]))
    }

    fn covering(&self, visible: Bounds<f32>, margin: u32) -> impl Iterator<Item = TileKey> {
        covering(self.width(), self.height(), visible, margin)
    }
}

/// The tiles of a `width` x `height` image covering `visible` (fractions of
/// its size), plus `margin` more on every side.
fn covering(
    width: u32,
    height: u32,
    visible: Bounds<f32>,
    margin: u32,
) -> impl Iterator<Item = TileKey> {
    let tile_at = |fraction: f32, pixels: u32, last: u32| {
        ((fraction.clamp(0.0, 1.0) * pixels as f32) as u32 / TILE_SIZE).min(last)
    };
    let last_column = width.div_ceil(TILE_SIZE) - 1;
    let last_row = height.div_ceil(TILE_SIZE) - 1;
    let first_column = tile_at(visible.origin.x, width, last_column);
    let first_row = tile_at(visible.origin.y, height, last_row);
    let end_column = tile_at(visible.origin.x + visible.size.width, width, last_column);
    let end_row = tile_at(visible.origin.y + visible.size.height, height, last_row);
    let columns = first_column.saturating_sub(margin)..=(end_column + margin).min(last_column);
    let rows = first_row.saturating_sub(margin)..=(end_row + margin).min(last_row);
    rows.flat_map(move |row| columns.clone().map(move |column| (column, row)))
}

/// The full-resolution image and the tiles made from it so far.
pub struct Tiles {
    pub source: Arc<FullImage>,
    shown: HashMap<TileKey, Arc<RenderImage>>,
}

impl Tiles {
    pub fn new(source: FullImage) -> Self {
        Self {
            source: Arc::new(source),
            shown: HashMap::new(),
        }
    }

    /// Updates which tiles are kept for the part of the image in view
    /// (fractions of its size; none when it's not zoomed in enough to need
    /// them). Returns the tiles no longer needed, to release, and those to
    /// cut.
    pub fn update(
        &mut self,
        visible: Option<Bounds<f32>>,
    ) -> (Vec<Arc<RenderImage>>, Vec<TileKey>) {
        let Some(visible) = visible else {
            return (
                self.shown.drain().map(|(_, tile)| tile).collect(),
                Vec::new(),
            );
        };
        let keep: Vec<TileKey> = self.source.covering(visible, RELEASE_MARGIN).collect();
        let released = self
            .shown
            .extract_if(|key, _| !keep.contains(key))
            .map(|(_, tile)| tile)
            .collect();
        let missing = self
            .source
            .covering(visible, MARGIN)
            .filter(|key| !self.shown.contains_key(key))
            .collect();
        (released, missing)
    }

    /// Adds tiles that have been cut.
    pub fn insert(&mut self, tiles: Vec<(TileKey, Arc<RenderImage>)>) {
        self.shown.extend(tiles);
    }

    /// The tiles to draw: each with the part of the image it covers, as
    /// fractions of its size.
    pub fn shown(&self) -> Vec<(Bounds<f32>, Arc<RenderImage>)> {
        self.shown
            .iter()
            .map(|(key, tile)| (self.source.tile_rect(*key), tile.clone()))
            .collect()
    }

    /// Every tile, to release.
    pub fn take_all(&mut self) -> Vec<Arc<RenderImage>> {
        self.shown.drain().map(|(_, tile)| tile).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(width: u32, height: u32) -> FullImage {
        FullImage::new(DynamicImage::new_rgb8(width, height))
    }

    fn bounds(x: f32, y: f32, width: f32, height: f32) -> Bounds<f32> {
        Bounds {
            origin: point(x, y),
            size: size(width, height),
        }
    }

    #[test]
    fn edge_tiles_are_cut_to_the_image() {
        let image = image(1200, 600);
        assert_eq!(
            covering(1200, 600, bounds(0.0, 0.0, 1.0, 1.0), 0).count(),
            3 * 2
        );
        assert_eq!(image.cut((2, 1)).size(0).width.0, 176);
        assert_eq!(image.cut((2, 1)).size(0).height.0, 88);
        let rect = image.tile_rect((2, 1));
        assert!((rect.origin.x - 1024.0 / 1200.0).abs() < 1e-6);
        assert!((rect.origin.x + rect.size.width - 1.0).abs() < 1e-6);
        assert!((rect.origin.y + rect.size.height - 1.0).abs() < 1e-6);
    }

    #[test]
    fn keeps_tiles_in_view_and_a_margin() {
        // 8x8 tiles; the view covers tiles (3..=4, 3..=4).
        let mut tiles = Tiles::new(image(4096, 4096));
        let view = bounds(0.40, 0.40, 0.20, 0.20);
        let (released, missing) = tiles.update(Some(view));
        assert!(released.is_empty());
        // One tile around them: 4x4.
        assert_eq!(missing.len(), 16);
        assert!(missing.contains(&(2, 2)) && missing.contains(&(5, 5)));
        let cut = missing
            .iter()
            .map(|&key| (key, tiles.source.cut(key)))
            .collect();
        tiles.insert(cut);

        // Panning by a tile: one column more is needed, none released yet.
        let (released, missing) = tiles.update(Some(bounds(0.525, 0.40, 0.20, 0.20)));
        assert!(released.is_empty());
        assert_eq!(missing, vec![(6, 2), (6, 3), (6, 4), (6, 5)]);

        // In the corner, all go but the one within two tiles of the view.
        let (released, _) = tiles.update(Some(bounds(0.0, 0.0, 0.05, 0.05)));
        assert_eq!(released.len(), 15);
        assert_eq!(tiles.shown().len(), 1);

        // Zoomed out, nothing's needed.
        let (released, missing) = tiles.update(None);
        assert_eq!((released.len(), missing.len()), (1, 0));
        assert!(tiles.shown().is_empty());
    }

    #[test]
    fn tiles_are_bounded_by_the_view_not_the_photo() {
        // A 200 MP photo, seen through a window a few tiles across.
        let view = bounds(0.5, 0.5, 3.0 * 512.0 / 16_384.0, 2.0 * 512.0 / 12_288.0);
        assert!(covering(16_384, 12_288, view, MARGIN).count() <= 6 * 5);
    }
}

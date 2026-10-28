//! Boxes around the objects found in a photo (see `detect`), drawn over it
//! in the viewer. Each box's outline shimmers like GPUI Kit's
//! `ShimmerText`: a soft band of light sweeps across it, built from nested
//! layers of a faint highlight, each clipped to a narrower band, so they add
//! up to a smooth peak in the middle.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{
    Animation, AnimationExt as _, App, BorderStyle, Bounds, ContentMask, Hsla, IntoElement, Pixels,
    RenderOnce, Window, canvas, div, hsla, point, prelude::*, px, quad, relative, size,
    transparent_black,
};

use crate::detect::DetectedObject;

/// One sweep across a box, as `ShimmerText`'s default.
const SWEEP: Duration = Duration::from_secs(2);
/// The highlight's half-width, as a fraction of the box's width.
const SPREAD: f32 = 0.3;
const LAYER_COUNT: usize = 12;
const CORNER_RADIUS: f32 = 4.;
const BORDER_WIDTH: f32 = 1.5;

/// The opacity of the band's peak, where every layer overlaps: the outline,
/// and a glow inside the box.
const OUTLINE_PEAK_OPACITY: f32 = 0.95;
const GLOW_PEAK_OPACITY: f32 = 0.18;

/// Boxes for `objects`, filling the image they were found in.
#[derive(IntoElement)]
pub struct ObjectBoxes {
    objects: Arc<[DetectedObject]>,
}

impl ObjectBoxes {
    pub fn new(objects: Arc<[DetectedObject]>) -> Self {
        Self { objects }
    }
}

impl RenderOnce for ObjectBoxes {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let animate = !cx.reduce_motion();
        div()
            .id("object-box-overlay")
            .relative()
            .size_full()
            // Not every detector says where objects are.
            .children(self.objects.iter().enumerate().filter_map(|(ix, object)| {
                let [left, top, right, bottom] = object.bounds?;
                let outline = BoxOutline { phase: None };
                let outline = if animate {
                    outline
                        .with_animation(
                            ("object-box", ix),
                            Animation::new(SWEEP).repeat_synced(),
                            |mut outline, phase| {
                                outline.phase = Some(phase);
                                outline
                            },
                        )
                        .into_any_element()
                } else {
                    outline.into_any_element()
                };
                let object_box = div()
                    .absolute()
                    .left(relative(left))
                    .top(relative(top))
                    .w(relative(right - left))
                    .h(relative(bottom - top))
                    .child(outline)
                    .child(label(object));
                Some(object_box)
            }))
    }
}

/// What the object is and how sure the model is, inside the box's top left
/// corner (above it, it could be cut off at the top of the photo).
fn label(object: &DetectedObject) -> impl IntoElement {
    let mut name = object.label.clone();
    if let Some(first) = name.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    div()
        .absolute()
        .top(px(BORDER_WIDTH + 3.))
        .left(px(BORDER_WIDTH + 3.))
        .max_w_full()
        .px_1p5()
        .rounded(px(CORNER_RADIUS - 1.))
        .bg(hsla(0., 0., 0., 0.55))
        .text_color(hsla(0., 0., 1., 0.92))
        .text_xs()
        .truncate()
        .child(format!("{name} {:.0}%", (object.confidence * 100.).round()))
}

/// A box's outline, with the shimmer at `phase` (0 to 1 across the box), or
/// none while the system asks for reduced motion.
#[derive(IntoElement)]
struct BoxOutline {
    phase: Option<f32>,
}

impl RenderOnce for BoxOutline {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let phase = self.phase;
        canvas(
            |_, _, _| {},
            move |bounds, _, window, _| paint_outline(bounds, phase, window),
        )
        .size_full()
    }
}

fn paint_outline(bounds: Bounds<Pixels>, phase: Option<f32>, window: &mut Window) {
    let outline = |color: Hsla, background: Hsla, bounds: Bounds<Pixels>| {
        quad(
            bounds,
            px(CORNER_RADIUS),
            background,
            px(BORDER_WIDTH),
            color,
            BorderStyle::Solid,
        )
    };
    // A dark edge outside the light one, so it shows on light photos too.
    let shadow = Bounds {
        origin: bounds.origin - point(px(1.), px(1.)),
        size: bounds.size + size(px(2.), px(2.)),
    };
    window.paint_quad(quad(
        shadow,
        px(CORNER_RADIUS + 1.),
        transparent_black(),
        px(1.),
        hsla(0., 0., 0., 0.35),
        BorderStyle::Solid,
    ));
    window.paint_quad(outline(hsla(0., 0., 1., 0.6), transparent_black(), bounds));

    let Some(phase) = phase else {
        return;
    };
    let bands = (0..LAYER_COUNT).filter_map(|layer| band_bounds(bounds, phase, layer));
    let highlight = hsla(0., 0., 1., layer_opacity(OUTLINE_PEAK_OPACITY));
    let glow = hsla(0., 0., 1., layer_opacity(GLOW_PEAK_OPACITY));
    window.paint_layer(shadow, |window| {
        for band in bands {
            window.with_content_mask(Some(ContentMask { bounds: band }), |window| {
                window.paint_quad(outline(highlight, glow, bounds));
            });
        }
    });
}

/// The opacity of each layer for all of them together to reach `peak`.
fn layer_opacity(peak: f32) -> f32 {
    1. - (1. - peak).powf(1. / LAYER_COUNT as f32)
}

/// Where `layer` of the highlight is at `phase`: a band centred on the
/// highlight, narrower for each layer, so the layers overlap most in the
/// middle. The band starts and ends just outside the box, so each sweep
/// fades in and out. None when it's outside the box altogether.
fn band_bounds(bounds: Bounds<Pixels>, phase: f32, layer: usize) -> Option<Bounds<Pixels>> {
    let width = f32::from(bounds.size.width);
    if width <= 0. || bounds.size.height <= px(0.) || layer >= LAYER_COUNT {
        return None;
    }
    // Room for a whole band beyond each side.
    let padding = SPREAD + 0.05;
    let center = phase.mul_add(1. + padding * 2., -padding) * width;
    let radius = width * SPREAD * (1. - layer as f32 / LAYER_COUNT as f32);
    // A little beyond the box, to take in its shadow.
    let left = (center - radius).max(-1.);
    let right = (center + radius).min(width + 1.);
    (right > left).then(|| Bounds {
        origin: point(bounds.origin.x + px(left), bounds.origin.y - px(1.)),
        size: size(px(right - left), bounds.size.height + px(2.)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highlight_sweeps_across_the_box() {
        let bounds = Bounds {
            origin: point(px(10.), px(20.)),
            size: size(px(100.), px(50.)),
        };
        // Off the box at either end of the sweep.
        assert!(band_bounds(bounds, 0., 0).is_none());
        assert!(band_bounds(bounds, 1., 0).is_none());

        let early = band_bounds(bounds, 0.35, 0).unwrap();
        let late = band_bounds(bounds, 0.65, 0).unwrap();
        assert!(early.origin.x < late.origin.x);

        // Inner layers are narrower, around the same middle.
        let outer = band_bounds(bounds, 0.5, 0).unwrap();
        let inner = band_bounds(bounds, 0.5, LAYER_COUNT - 1).unwrap();
        assert!(inner.origin.x > outer.origin.x);
        assert!(inner.size.width < outer.size.width);
        assert_eq!(
            outer.origin.x + outer.size.width / 2.,
            inner.origin.x + inner.size.width / 2.
        );
        assert!(band_bounds(bounds, 0.5, LAYER_COUNT).is_none());
    }

    #[test]
    fn layers_add_up_to_the_peak() {
        let layer = layer_opacity(OUTLINE_PEAK_OPACITY);
        let total = 1. - (1. - layer).powi(LAYER_COUNT as i32);
        assert!((total - OUTLINE_PEAK_OPACITY).abs() < 1e-4);
    }
}

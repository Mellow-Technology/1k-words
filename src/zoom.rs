//! Zoom and pan state for the photo viewer, and the element that draws an
//! image according to it.
//!
//! The zoom level is relative to "fit": at `zoom == 1.0` the whole image fits
//! the viewport. The pan offset is the distance of the image's center from
//! the viewport's center, so a resized window keeps the image centered.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, ObjectFit, Pixels, Point, RenderImage, Size, Style,
    Window, div, img, point, prelude::*, px, relative, size,
};

/// The furthest the viewer zooms in, in device pixels per image pixel.
const MAX_DEVICE_PIXELS_PER_IMAGE_PIXEL: f32 = 8.0;

#[derive(Debug)]
pub struct ZoomState {
    zoom: f32,
    pan: Point<Pixels>,
    /// Size of the image in image pixels.
    image_size: Size<f32>,
    /// The pane the image is drawn in, captured during the last prepaint.
    viewport: Bounds<Pixels>,
    scale_factor: f32,
    /// Mouse position and pan offset when the current drag started.
    drag_start: Option<(Point<Pixels>, Point<Pixels>)>,
}

impl ZoomState {
    pub fn new(image_size: Size<f32>) -> Self {
        Self {
            zoom: 1.0,
            pan: Point::default(),
            image_size,
            viewport: Bounds::default(),
            scale_factor: 2.0,
            drag_start: None,
        }
    }

    /// Shows a different image, keeping the zoom level and pan offset, so
    /// switching between frames of a bracket or burst shows the same part
    /// of the scene.
    pub fn set_image_size(&mut self, image_size: Size<f32>) {
        self.image_size = image_size;
        self.drag_start = None;
        self.clamp_pan();
    }

    pub fn zoom_to_fit(&mut self) {
        self.zoom = 1.0;
        self.pan = Point::default();
        self.drag_start = None;
    }

    pub fn is_fit(&self) -> bool {
        self.zoom <= 1.0
    }

    /// Zoom level as a percentage of the image's actual pixel size.
    pub fn actual_percent(&self) -> f32 {
        self.scale() * self.scale_factor * 100.0
    }

    /// Multiplies the zoom level, keeping the image point under `anchor`
    /// (in window coordinates) in place. Zooms about the viewport center if
    /// no anchor is given.
    pub fn zoom_by(&mut self, factor: f32, anchor: Option<Point<Pixels>>) {
        self.zoom_to(self.zoom * factor, anchor);
    }

    /// Toggles between fitting the image and showing it at actual size
    /// (one image pixel per device pixel), centered on `anchor`.
    pub fn toggle_actual_size(&mut self, anchor: Option<Point<Pixels>>) {
        let actual_size = 1.0 / (self.scale_factor * self.fit_scale());
        if self.is_fit() && actual_size > 1.0 {
            self.zoom_to(actual_size, anchor);
        } else {
            self.zoom_to_fit();
        }
    }

    pub fn zoom_to_actual_size(&mut self) {
        self.zoom_to(1.0 / (self.scale_factor * self.fit_scale()), None);
    }

    pub fn pan_by(&mut self, delta: Point<Pixels>) {
        self.pan += delta;
        self.clamp_pan();
    }

    pub fn start_drag(&mut self, position: Point<Pixels>) {
        self.drag_start = Some((position, self.pan));
    }

    /// Returns whether a drag is in progress.
    pub fn drag_to(&mut self, position: Point<Pixels>) -> bool {
        let Some((start, start_pan)) = self.drag_start else {
            return false;
        };
        self.pan = start_pan + (position - start);
        self.clamp_pan();
        true
    }

    pub fn end_drag(&mut self) {
        self.drag_start = None;
    }

    pub fn is_dragging(&self) -> bool {
        self.drag_start.is_some()
    }

    fn zoom_to(&mut self, zoom: f32, anchor: Option<Point<Pixels>>) {
        let zoom = zoom.clamp(1.0, self.max_zoom());
        let anchor = anchor.unwrap_or_else(|| self.viewport.center());
        // Keep the image point under the anchor fixed:
        // anchor = center + pan + image_point * scale.
        let from_center = anchor - self.viewport.center();
        let ratio = zoom / self.zoom;
        self.pan = from_center - (from_center - self.pan) * ratio;
        self.zoom = zoom;
        self.clamp_pan();
    }

    /// Screen points per image pixel when the image fits the viewport.
    fn fit_scale(&self) -> f32 {
        let viewport = self.viewport.size;
        if self.image_size.width <= 0.0 || self.image_size.height <= 0.0 {
            return 1.0;
        }
        (f32::from(viewport.width) / self.image_size.width)
            .min(f32::from(viewport.height) / self.image_size.height)
            .max(f32::EPSILON)
    }

    fn scale(&self) -> f32 {
        self.fit_scale() * self.zoom
    }

    fn max_zoom(&self) -> f32 {
        (MAX_DEVICE_PIXELS_PER_IMAGE_PIXEL / (self.scale_factor * self.fit_scale())).max(1.0)
    }

    fn displayed_size(&self) -> Size<Pixels> {
        let scale = self.scale();
        size(
            px(self.image_size.width * scale),
            px(self.image_size.height * scale),
        )
    }

    /// Stops the image from being panned past its edges. Along an axis
    /// where the image is smaller than the viewport, it stays centered.
    fn clamp_pan(&mut self) {
        let displayed = self.displayed_size();
        let viewport = self.viewport.size;
        let limit = |displayed: Pixels, viewport: Pixels| ((displayed - viewport) / 2.).max(px(0.));
        let max_x = limit(displayed.width, viewport.width);
        let max_y = limit(displayed.height, viewport.height);
        self.pan = point(
            self.pan.x.clamp(-max_x, max_x),
            self.pan.y.clamp(-max_y, max_y),
        );
    }

    fn image_bounds(&self) -> Bounds<Pixels> {
        let displayed = self.displayed_size();
        let center = self.viewport.center() + self.pan;
        Bounds {
            origin: point(
                center.x - displayed.width / 2.,
                center.y - displayed.height / 2.,
            ),
            size: displayed,
        }
    }
}

/// Draws `image` filling its parent according to the shared zoom state.
/// The parent should clip its overflow.
pub fn zoomed_image(image: Arc<RenderImage>, state: Rc<RefCell<ZoomState>>) -> ZoomedImage {
    ZoomedImage {
        image,
        state,
        overlay: None,
    }
}

pub struct ZoomedImage {
    image: Arc<RenderImage>,
    state: Rc<RefCell<ZoomState>>,
    overlay: Option<AnyElement>,
}

impl ZoomedImage {
    /// Draws `overlay` over the image, zoomed and panned with it: it's laid
    /// out at the image's displayed size.
    pub fn overlay(mut self, overlay: impl IntoElement) -> Self {
        self.overlay = Some(overlay.into_any_element());
        self
    }
}

impl IntoElement for ZoomedImage {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ZoomedImage {
    type RequestLayoutState = ();
    type PrepaintState = (AnyElement, Option<AnyElement>);

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, None, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let image_bounds = {
            let mut state = self.state.borrow_mut();
            state.viewport = bounds;
            state.scale_factor = window.scale_factor();
            state.clamp_pan();
            state.image_bounds()
        };

        let mut image = img(self.image.clone())
            .w(image_bounds.size.width)
            .h(image_bounds.size.height)
            .object_fit(ObjectFit::Fill)
            .into_any_element();
        image.layout_as_root(
            size(
                AvailableSpace::Definite(image_bounds.size.width),
                AvailableSpace::Definite(image_bounds.size.height),
            ),
            window,
            cx,
        );
        image.prepaint_at(image_bounds.origin, window, cx);

        let overlay = self.overlay.take().map(|overlay| {
            let mut overlay = div()
                .w(image_bounds.size.width)
                .h(image_bounds.size.height)
                .child(overlay)
                .into_any_element();
            overlay.layout_as_root(
                size(
                    AvailableSpace::Definite(image_bounds.size.width),
                    AvailableSpace::Definite(image_bounds.size.height),
                ),
                window,
                cx,
            );
            overlay.prepaint_at(image_bounds.origin, window, cx);
            overlay
        });
        (image, overlay)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        (image, overlay): &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        image.paint(window, cx);
        if let Some(overlay) = overlay {
            overlay.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ZoomState {
        let mut state = ZoomState::new(size(4000., 2000.));
        state.viewport = Bounds {
            origin: point(px(100.), px(50.)),
            size: size(px(800.), px(800.)),
        };
        state.scale_factor = 2.0;
        state
    }

    #[test]
    fn fits_and_centers_image() {
        let state = state();
        // 4000x2000 into 800x800 -> 800x400, centered vertically.
        assert_eq!(
            state.image_bounds(),
            Bounds {
                origin: point(px(100.), px(250.)),
                size: size(px(800.), px(400.)),
            }
        );
        assert_eq!(state.actual_percent(), 40.0);
    }

    #[test]
    fn zooming_keeps_anchor_fixed() {
        let mut state = state();
        // Right quarter of the image, vertically centered.
        let anchor = point(px(700.), px(450.));
        state.zoom_by(2.0, Some(anchor));
        let bounds = state.image_bounds();
        // The anchor was 3/4 across the image; it still should be.
        let fraction = (anchor.x - bounds.origin.x) / bounds.size.width;
        assert!((fraction - 0.75).abs() < 1e-4, "{fraction}");
    }

    #[test]
    fn pan_is_clamped_to_image_edges() {
        let mut state = state();
        state.pan_by(point(px(500.), px(500.)));
        assert_eq!(state.pan, Point::default());

        state.zoom_by(2.0, None); // 1600x800 in an 800x800 viewport.
        state.pan_by(point(px(10_000.), px(10_000.)));
        assert_eq!(state.pan, point(px(400.), px(0.)));
    }

    #[test]
    fn changing_image_keeps_zoom_and_pan() {
        let mut state = state();
        state.zoom_by(2.0, Some(point(px(700.), px(450.))));
        let (zoom, pan) = (state.zoom, state.pan);
        state.set_image_size(size(4000., 2000.));
        assert_eq!((state.zoom, state.pan), (zoom, pan));
    }

    #[test]
    fn zoom_is_limited() {
        let mut state = state();
        state.zoom_by(0.1, None);
        assert_eq!(state.zoom, 1.0);
        state.zoom_by(1000.0, None);
        assert_eq!(state.actual_percent(), 800.0);
    }
}

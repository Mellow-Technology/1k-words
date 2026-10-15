//! An element that fills its parent and lays out its child at the largest
//! size with a given aspect ratio that fits, centred. Flexbox can't size a
//! box to an aspect ratio within both a maximum width and height.

use gpui_kit::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Style, Window, point, relative, size,
};

/// Fits `child` within the parent at `aspect_ratio` (width / height).
pub fn aspect_fit(aspect_ratio: f32, child: impl IntoElement) -> AspectFit {
    AspectFit {
        aspect_ratio,
        child: child.into_any_element(),
    }
}

pub struct AspectFit {
    aspect_ratio: f32,
    child: AnyElement,
}

impl IntoElement for AspectFit {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

/// The largest `aspect_ratio` rectangle that fits in `available`, centred.
fn fit(available: Bounds<f32>, aspect_ratio: f32) -> Bounds<f32> {
    let aspect_ratio = aspect_ratio.clamp(0.05, 20.);
    let width = available
        .size
        .width
        .min(available.size.height * aspect_ratio);
    let height = width / aspect_ratio;
    Bounds {
        origin: point(
            available.origin.x + (available.size.width - width) / 2.,
            available.origin.y + (available.size.height - height) / 2.,
        ),
        size: size(width, height),
    }
}

impl Element for AspectFit {
    type RequestLayoutState = ();
    type PrepaintState = ();

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
        (window.request_layout(style, [], cx), ())
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
        let available = Bounds {
            origin: point(f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
            size: size(f32::from(bounds.size.width), f32::from(bounds.size.height)),
        };
        let fitted = fit(available, self.aspect_ratio);
        self.child.layout_as_root(
            size(
                AvailableSpace::Definite(fitted.size.width.into()),
                AvailableSpace::Definite(fitted.size.height.into()),
            ),
            window,
            cx,
        );
        self.child.prepaint_at(
            point(fitted.origin.x.into(), fitted.origin.y.into()),
            window,
            cx,
        );
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_and_centres() {
        let available = Bounds {
            origin: point(10., 20.),
            size: size(800., 600.),
        };
        // A portrait photo is limited by the height.
        let portrait = fit(available, 3. / 4.);
        assert_eq!(portrait.size, size(450., 600.));
        assert_eq!(portrait.origin, point(185., 20.));
        // A panorama is limited by the width.
        let wide = fit(available, 2.);
        assert_eq!(wide.size, size(800., 400.));
        assert_eq!(wide.origin, point(10., 120.));
    }
}

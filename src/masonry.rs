//! A virtualized masonry grid element.
//!
//! The element sizes itself to the full height of the masonry layout so that
//! it can live inside an ordinary `overflow_y_scroll` container. During
//! prepaint it intersects its bounds with the current content mask (the
//! scroll container's viewport) and only builds, lays out, and paints the
//! items that fall inside it.

use std::cell::RefCell;
use std::rc::Rc;

use gpui_kit::{
    AnyElement, App, AvailableSpace, Bounds, Element, ElementId, GlobalElementId,
    InspectorElementId, IntoElement, LayoutId, Pixels, Style, Window, point, px, relative, size,
};

#[derive(Clone, Copy, Debug)]
pub struct MasonryOptions {
    /// Columns are added as the container grows, keeping them roughly this
    /// wide.
    pub target_column_width: f32,
    pub gap: f32,
    /// Extra distance above and below the viewport in which items are
    /// still rendered, so that thumbnails are loaded before they scroll in.
    pub overscan: f32,
}

impl Default for MasonryOptions {
    fn default() -> Self {
        Self {
            target_column_width: 260.0,
            gap: 8.0,
            overscan: 600.0,
        }
    }
}

/// A direction to move the selection in the grid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// Placement of every item for a particular container width.
#[derive(Debug)]
pub struct MasonryLayout {
    width: f32,
    /// Item rectangles relative to the top-left of the grid.
    rects: Vec<Bounds<f32>>,
    /// Item indices per column, in increasing `y` order.
    columns: Vec<Vec<usize>>,
    /// Each item's column.
    item_columns: Vec<usize>,
    height: f32,
}

impl MasonryLayout {
    pub fn compute(aspect_ratios: &[f32], width: f32, options: &MasonryOptions) -> Self {
        let gap = options.gap;
        let column_count =
            (((width + gap) / (options.target_column_width + gap)).round() as usize).max(1);
        let column_width =
            ((width - gap * (column_count - 1) as f32) / column_count as f32).max(1.0);

        let mut column_heights = vec![0.0f32; column_count];
        let mut columns = vec![Vec::new(); column_count];
        let mut rects = Vec::with_capacity(aspect_ratios.len());
        let mut item_columns = Vec::with_capacity(aspect_ratios.len());

        for (ix, &aspect) in aspect_ratios.iter().enumerate() {
            let (column, &y) = column_heights
                .iter()
                .enumerate()
                .min_by(|a, b| a.1.total_cmp(b.1))
                .unwrap();
            let height = (column_width / aspect.clamp(0.1, 10.0)).round();
            let x = column as f32 * (column_width + gap);
            rects.push(Bounds {
                origin: point(x, y),
                size: size(column_width, height),
            });
            columns[column].push(ix);
            item_columns.push(column);
            column_heights[column] = y + height + gap;
        }

        let height = column_heights.iter().copied().fold(0.0, f32::max).max(gap) - gap;

        Self {
            width,
            rects,
            columns,
            item_columns,
            height,
        }
    }

    /// The item's rectangle relative to the top-left of the grid.
    pub fn rect(&self, ix: usize) -> Option<Bounds<f32>> {
        self.rects.get(ix).copied()
    }

    /// The item next to `ix` in `direction`: the one above or below in the
    /// same column, or the one in the next column whose middle is closest
    /// vertically.
    pub fn neighbor(&self, ix: usize, direction: Direction) -> Option<usize> {
        let column = *self.item_columns.get(ix)?;
        let items = &self.columns[column];
        let position = items.iter().position(|&item| item == ix)?;
        let other_column = match direction {
            Direction::Up => return position.checked_sub(1).map(|p| items[p]),
            Direction::Down => return items.get(position + 1).copied(),
            Direction::Left => column.checked_sub(1)?,
            Direction::Right => column + 1,
        };
        let middle = |ix: usize| self.rects[ix].origin.y + self.rects[ix].size.height / 2.;
        let target = middle(ix);
        self.columns
            .get(other_column)?
            .iter()
            .copied()
            .min_by(|&a, &b| {
                (middle(a) - target)
                    .abs()
                    .total_cmp(&(middle(b) - target).abs())
            })
    }

    pub fn height(&self) -> f32 {
        self.height
    }

    /// The container width this layout is for.
    pub fn width(&self) -> f32 {
        self.width
    }

    /// Indices of items overlapping the vertical range `top..bottom`, in
    /// ascending order.
    pub fn items_in_range(&self, top: f32, bottom: f32) -> Vec<usize> {
        let mut items = Vec::new();
        for column in &self.columns {
            let start = column.partition_point(|&ix| {
                let rect = &self.rects[ix];
                rect.origin.y + rect.size.height < top
            });
            items.extend(
                column[start..]
                    .iter()
                    .copied()
                    .take_while(|&ix| self.rects[ix].origin.y <= bottom),
            );
        }
        items.sort_unstable();
        items
    }
}

/// Item data shared between the owning view and the element, so the layout
/// is only recomputed when the width or the items change.
pub struct MasonryState {
    aspect_ratios: Vec<f32>,
    options: MasonryOptions,
    layout: RefCell<Option<Rc<MasonryLayout>>>,
}

impl MasonryState {
    pub fn new(aspect_ratios: Vec<f32>, options: MasonryOptions) -> Self {
        Self {
            aspect_ratios,
            options,
            layout: RefCell::new(None),
        }
    }

    /// The layout from the last time the grid was laid out.
    pub fn layout(&self) -> Option<Rc<MasonryLayout>> {
        self.layout.borrow().clone()
    }

    pub fn layout_for_width(&self, width: f32) -> Rc<MasonryLayout> {
        let mut cached = self.layout.borrow_mut();
        match cached.as_ref() {
            Some(layout) if (layout.width - width).abs() < 0.5 => layout.clone(),
            _ => {
                let layout = Rc::new(MasonryLayout::compute(
                    &self.aspect_ratios,
                    width,
                    &self.options,
                ));
                *cached = Some(layout.clone());
                layout
            }
        }
    }
}

type RenderItems = Box<dyn Fn(&[usize], &mut Window, &mut App) -> Vec<AnyElement>>;

/// Creates a masonry grid. `render_items` is called each frame with the
/// indices of the items that are (nearly) visible and must return one
/// element per index, in the same order. Each element is laid out to fill
/// its cell.
pub fn masonry<E: IntoElement>(
    state: Rc<MasonryState>,
    render_items: impl Fn(&[usize], &mut Window, &mut App) -> Vec<E> + 'static,
) -> Masonry {
    Masonry {
        state,
        render_items: Box::new(move |indices, window, cx| {
            render_items(indices, window, cx)
                .into_iter()
                .map(IntoElement::into_any_element)
                .collect()
        }),
    }
}

pub struct Masonry {
    state: Rc<MasonryState>,
    render_items: RenderItems,
}

impl IntoElement for Masonry {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Masonry {
    type RequestLayoutState = ();
    type PrepaintState = Vec<AnyElement>;

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
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        let state = self.state.clone();
        let layout_id =
            window.request_measured_layout(style, move |known, available, _window, _cx| {
                let width = known.width.unwrap_or(match available.width {
                    AvailableSpace::Definite(width) => width,
                    _ => px(state.options.target_column_width),
                });
                let height = state.layout_for_width(width.into()).height();
                size(width, px(height))
            });
        (layout_id, ())
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
        let layout = self.state.layout_for_width(bounds.size.width.into());
        let viewport = window.content_mask().bounds;
        let overscan = self.state.options.overscan;
        let top = f32::from(viewport.top() - bounds.top()) - overscan;
        let bottom = f32::from(viewport.bottom() - bounds.top()) + overscan;

        let indices = layout.items_in_range(top, bottom);
        let mut items = (self.render_items)(&indices, window, cx);
        debug_assert_eq!(items.len(), indices.len());

        for (item, &ix) in items.iter_mut().zip(&indices) {
            let rect = layout.rects[ix];
            item.layout_as_root(
                size(
                    AvailableSpace::Definite(px(rect.size.width)),
                    AvailableSpace::Definite(px(rect.size.height)),
                ),
                window,
                cx,
            );
            item.prepaint_at(
                bounds.origin + point(px(rect.origin.x), px(rect.origin.y)),
                window,
                cx,
            );
        }
        items
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        items: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        for item in items {
            item.paint(window, cx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_items_in_shortest_column() {
        let options = MasonryOptions {
            target_column_width: 100.0,
            gap: 10.0,
            overscan: 0.0,
        };
        // 210px wide -> two 100px columns.
        let layout = MasonryLayout::compute(&[1.0, 0.5, 2.0, 1.0], 210.0, &options);
        assert_eq!(layout.columns.len(), 2);
        // Item 0 (100 tall) in col 0, item 1 (200 tall) in col 1,
        // item 2 (50 tall) in col 0 at y=110, item 3 in col 0 at y=170.
        assert_eq!(layout.rects[2].origin, point(0.0, 110.0));
        assert_eq!(layout.rects[3].origin, point(0.0, 170.0));
        assert_eq!(layout.height(), 270.0);
    }

    #[test]
    fn finds_neighbors() {
        let options = MasonryOptions {
            target_column_width: 100.0,
            gap: 10.0,
            overscan: 0.0,
        };
        // Column 0: items 0 (y 0..100), 2 (110..160), 3 (170..270).
        // Column 1: item 1 (y 0..200).
        let layout = MasonryLayout::compute(&[1.0, 0.5, 2.0, 1.0], 210.0, &options);
        assert_eq!(layout.neighbor(0, Direction::Down), Some(2));
        assert_eq!(layout.neighbor(2, Direction::Down), Some(3));
        assert_eq!(layout.neighbor(3, Direction::Down), None);
        assert_eq!(layout.neighbor(2, Direction::Up), Some(0));
        assert_eq!(layout.neighbor(0, Direction::Up), None);
        assert_eq!(layout.neighbor(3, Direction::Right), Some(1));
        assert_eq!(layout.neighbor(1, Direction::Right), None);
        assert_eq!(layout.neighbor(0, Direction::Left), None);
        // Item 1's middle (y 100) is closest to item 2's (y 135).
        assert_eq!(layout.neighbor(1, Direction::Left), Some(2));
    }

    #[test]
    fn range_query_only_returns_overlapping_items() {
        let options = MasonryOptions {
            target_column_width: 100.0,
            gap: 0.0,
            overscan: 0.0,
        };
        let aspects = vec![1.0; 100];
        let layout = MasonryLayout::compute(&aspects, 100.0, &options);
        assert_eq!(layout.items_in_range(250.0, 420.0), vec![2, 3, 4]);
        assert!(layout.items_in_range(20_000.0, 30_000.0).is_empty());
    }
}

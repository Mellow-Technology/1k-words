//! A quick look at the selected gallery item, floating over the grid, for
//! reviewing photos without opening the viewer: the photo, its main
//! exposure details and, for brackets and bursts, every frame. The panel
//! takes the photo's aspect ratio, with the details floating over it.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{
    App, Context, Div, ObjectFit, RenderImage, SharedString, Task, Window, div, img, prelude::*,
    px, rgba, svg, transparent_black,
};

use gpui_kit::component::ActiveTheme;

use crate::assets::{BRACKET_ICON, BURST_ICON, LIVE_ICON};
use crate::decode;
use crate::details::{PhotoSummary, format_ev_value, photo_summary};
use crate::photos::{Stack, StackKind, StartFrame};

/// Moving through the grid quickly shows only thumbnails; larger images
/// load once the selection rests this long.
const LOAD_DELAY: Duration = Duration::from_millis(80);
const FILMSTRIP_THUMBNAIL_HEIGHT: f32 = 48.;

pub struct QuickPreview {
    stack: Stack,
    selected: usize,
    /// The gallery thumbnail of the stack's cover, shown while the cover's
    /// larger image loads.
    cover_thumbnail: Option<Arc<RenderImage>>,
    image: Option<Arc<RenderImage>>,
    failed: bool,
    summary: Option<PhotoSummary>,
    filmstrip: Vec<Option<Arc<RenderImage>>>,
    _load_image: Task<()>,
    _load_filmstrip: Task<()>,
}

impl QuickPreview {
    pub fn new(
        stack: Stack,
        cover_thumbnail: Option<Arc<RenderImage>>,
        start: StartFrame,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut this = Self {
            selected: stack.start_frame(start),
            filmstrip: vec![None; stack.photos.len()],
            stack,
            cover_thumbnail,
            image: None,
            failed: false,
            summary: None,
            _load_image: Task::ready(()),
            _load_filmstrip: Task::ready(()),
        };
        this.load_filmstrip(cx);
        this.select(this.selected, cx);
        this
    }

    fn select(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(photo) = self.stack.photos.get(ix).cloned() else {
            return;
        };
        if let Some(image) = self.image.take() {
            decode::release(image, cx);
        }
        self.selected = ix;
        self.failed = false;
        self.summary = None;
        self._load_image = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOAD_DELAY).await;
            let summary = cx.background_spawn({
                let photo = photo.clone();
                async move { photo_summary(&photo) }
            });
            let image = cx.background_spawn(async move { decode::quick_preview(&photo) });
            let summary = summary.await;
            this.update(cx, |this, cx| {
                this.summary = Some(summary);
                cx.notify();
            })
            .ok();
            let image = image.await;
            this.update(cx, |this, cx| {
                match image {
                    Ok(image) => this.image = Some(image),
                    Err(_) => this.failed = true,
                }
                cx.notify();
            })
            .ok();
        });
        cx.notify();
    }

    /// Steps `delta` frames through a bracket or burst. Returns false at
    /// either end, or for a single photo.
    pub fn step(&mut self, delta: isize, cx: &mut Context<Self>) -> bool {
        let ix = self.selected as isize + delta;
        if ix < 0 || ix as usize >= self.stack.photos.len() {
            return false;
        }
        self.select(ix as usize, cx);
        true
    }

    /// The shown frame's width / height, which the panel takes.
    pub fn aspect_ratio(&self) -> f32 {
        self.stack.photos[self.selected].aspect_ratio()
    }

    fn load_filmstrip(&mut self, cx: &mut Context<Self>) {
        if self.stack.photos.len() < 2 {
            return;
        }
        let photos = self.stack.photos.clone();
        self._load_filmstrip = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LOAD_DELAY).await;
            for (ix, photo) in photos.into_iter().enumerate() {
                let thumbnail = cx
                    .background_spawn(async move { decode::filmstrip_thumbnail(&photo) })
                    .await;
                let updated = this.update(cx, |this, cx| {
                    this.filmstrip[ix] = thumbnail.ok();
                    cx.notify();
                });
                if updated.is_err() {
                    return;
                }
            }
        });
    }

    #[cfg(test)]
    pub fn cover_path(&self) -> std::path::PathBuf {
        self.stack.cover().path.clone()
    }

    #[cfg(test)]
    pub fn image(&self) -> Option<Arc<RenderImage>> {
        self.image.clone()
    }

    #[cfg(test)]
    pub fn selected_frame(&self) -> usize {
        self.selected
    }

    /// Removes this preview's images from the GPU atlas.
    pub fn release_images(&mut self, cx: &mut App) {
        for image in self
            .image
            .take()
            .into_iter()
            .chain(self.filmstrip.iter_mut().filter_map(Option::take))
        {
            decode::release(image, cx);
        }
    }

    fn render_badges(&self) -> Option<Div> {
        let photo = &self.stack.photos[self.selected];
        let mut badges = Vec::new();
        if photo.raw_pair.is_some() {
            badges.push(pill().child("RAW+"));
        }
        if photo.live_video.is_some() {
            badges.push(
                pill()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon(LIVE_ICON))
                    .child("LIVE"),
            );
        }
        let kind = match self.stack.kind {
            StackKind::Single => None,
            StackKind::Bracket => Some((BRACKET_ICON, "Bracket")),
            StackKind::Burst => Some((BURST_ICON, "Burst")),
        };
        if let Some((kind_icon, name)) = kind {
            badges.push(
                pill()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon(kind_icon))
                    .child(format!(
                        "{name} · {} of {}",
                        self.selected + 1,
                        self.stack.photos.len()
                    )),
            );
        }
        (!badges.is_empty()).then(|| {
            div()
                .absolute()
                .top_3()
                .left_3()
                .flex()
                .gap_1p5()
                .children(badges)
        })
    }

    /// Every frame of a bracket or burst. Clicking one shows it.
    fn render_filmstrip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (primary, muted, foreground) = (theme.primary, theme.muted, theme.popover_foreground);
        div().flex().justify_center().child(
            div()
                .id("quick-preview-filmstrip")
                .max_w_full()
                .flex()
                .gap_2()
                .overflow_x_scroll()
                .children(self.stack.photos.iter().enumerate().map(|(ix, photo)| {
                    let selected = ix == self.selected;
                    let label = match self.stack.kind {
                        StackKind::Bracket => format!(
                            "{} EV",
                            format_ev_value(photo.meta.exposure_bias.unwrap_or(0.0))
                        ),
                        _ => format!("{}", ix + 1),
                    };
                    div()
                        .id(("quick-preview-frame", ix))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .items_center()
                        .gap_1()
                        .cursor_pointer()
                        .when(!selected, |frame| {
                            frame.opacity(0.7).hover(|s| s.opacity(1.))
                        })
                        .child(
                            div()
                                .h(px(FILMSTRIP_THUMBNAIL_HEIGHT))
                                .w(px(FILMSTRIP_THUMBNAIL_HEIGHT * photo.aspect_ratio()))
                                .rounded_md()
                                .border_2()
                                .border_color(if selected {
                                    primary
                                } else {
                                    transparent_black()
                                })
                                .bg(muted)
                                .overflow_hidden()
                                .children(self.filmstrip[ix].clone().map(|image| {
                                    img(image)
                                        .size_full()
                                        .rounded_sm()
                                        .object_fit(ObjectFit::Cover)
                                })),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(if selected {
                                    foreground
                                } else {
                                    foreground.opacity(0.6)
                                })
                                .child(label),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.select(ix, cx)))
                })),
        )
    }

    fn render_info(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let secondary_color = cx.theme().popover_foreground.opacity(0.65);
        let photo = &self.stack.photos[self.selected];
        let name: SharedString = photo
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let summary = self.summary.as_ref();
        let secondary = summary
            .map(|summary| {
                [summary.date_taken.clone(), summary.equipment.clone()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default();
        let exposure = summary.map(|s| s.exposure.clone()).unwrap_or_default();

        // The exposure details wrap below the name when the panel is narrow.
        div()
            .flex()
            .flex_wrap()
            .items_end()
            .gap_x_6()
            .gap_y_1p5()
            .child(
                div()
                    .flex_1()
                    .min_w(px(200.))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(div().text_sm().truncate().child(name))
                    .child(
                        div()
                            .text_xs()
                            .truncate()
                            .text_color(secondary_color)
                            .child(secondary),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .flex_col()
                    .items_end()
                    .gap_0p5()
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .text_sm()
                            .children(exposure.into_iter().map(|value| div().child(value))),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(secondary_color)
                            .child("Space to close · Return to open"),
                    ),
            )
    }
}

fn icon(path: &'static str) -> impl IntoElement {
    svg().path(path).size_3p5().text_color(rgba(0xffffffff))
}

// Badges sit on top of photos, so they use fixed colors that read on any
// image rather than theme colors.
fn pill() -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(rgba(0x000000a6))
        .text_color(rgba(0xffffffff))
        .text_xs()
}

impl Render for QuickPreview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Until the larger image loads, stretch a thumbnail we already have.
        let image = self
            .image
            .clone()
            .or_else(|| self.filmstrip[self.selected].clone())
            .or_else(|| {
                (self.selected == self.stack.cover)
                    .then(|| self.cover_thumbnail.clone())
                    .flatten()
            });
        let failed = self.failed && self.image.is_none();
        let theme = cx.theme();
        let (popover, foreground, border, muted_foreground) = (
            theme.popover,
            theme.popover_foreground,
            theme.border,
            theme.muted_foreground,
        );

        div()
            .relative()
            .size_full()
            .rounded_xl()
            .overflow_hidden()
            .bg(popover)
            .shadow_2xl()
            .text_color(foreground)
            // The panel has the photo's aspect ratio, so the photo fills it.
            .children(image.map(|image| {
                img(image)
                    .size_full()
                    .rounded_xl()
                    .object_fit(ObjectFit::Contain)
            }))
            .when(failed, |panel| {
                panel.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .text_sm()
                        .text_color(muted_foreground)
                        .child("Can't display image"),
                )
            })
            .children(self.render_badges())
            // The details float over the bottom of the photo on a
            // translucent card, so they read on any photo.
            .child(
                div()
                    .absolute()
                    .left_3()
                    .right_3()
                    .bottom_3()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .px_3()
                    .py_2p5()
                    .rounded_lg()
                    .bg(popover.opacity(0.78))
                    .border_1()
                    .border_color(border.opacity(0.6))
                    .shadow_lg()
                    .when(self.stack.photos.len() > 1, |card| {
                        card.child(self.render_filmstrip(cx))
                    })
                    .child(self.render_info(cx)),
            )
            // The border goes on top, so the photo doesn't cover it.
            .child(
                div()
                    .absolute()
                    .inset_0()
                    .rounded_xl()
                    .border_1()
                    .border_color(border),
            )
    }
}

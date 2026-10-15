//! The main gallery view: a toolbar and a virtualized masonry grid of
//! thumbnails for the currently open folder.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{
    App, Context, Div, Entity, FocusHandle, Focusable, KeyBinding, MouseButton, ObjectFit,
    PathPromptOptions, PromptLevel, RenderImage, ScrollHandle, SharedString, Stateful,
    Subscription, Task, Window, div, img, point, prelude::*, px, rgb, rgba, svg,
};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{Disableable as _, Icon, IconName, Selectable as _, Sizable};

use crate::aspect_fit::aspect_fit;
use crate::assets::{BRACKET_ICON, BURST_ICON, LIVE_ICON};
use crate::decode;
use crate::devices::{self, Device};
use crate::library::{self, ImportSession, ImportSummary, Imported, Library, PreparedImport};
use crate::masonry::{Direction, MasonryOptions, MasonryState, masonry};
use crate::navigation::{self, NavigationContext, OpenSettings, Page, navigation_sidebar};
use crate::photos::{self, Photo, Stack, StackKind, StartFrame};
use crate::quick_preview::QuickPreview;
use crate::settings::{self, GridLayout};
use crate::video::{PlaybackState, VideoPlayer};
use crate::viewer::{PhotoViewer, ViewerEvent};

gpui_kit::actions!(
    gallery,
    [
        ImportPhotos,
        NewLibrary,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        OpenSelected,
        ToggleQuickPreview,
        Dismiss
    ]
);

const KEY_CONTEXT: &str = "Gallery";
/// The corner radius of gallery tiles.
const TILE_RADIUS: f32 = 6.;
/// The width of the ring covering a playing video's corners; matches
/// `border_4`.
const CORNER_COVER_WIDTH: f32 = 4.;

/// Where a point in the grid was, to put it back after the layout changes.
struct ScrollAnchor {
    item: usize,
    /// How far down the item the point is, from 0 to 1.
    fraction: f32,
    /// How far down the view the point was.
    view_y: f32,
    /// The grid's width and the view's height.
    width: f32,
    viewport: f32,
}

/// Roughly how wide square tiles are; columns are added as the window
/// grows.
const SQUARE_TILE_SIZE: f32 = 180.;
/// The grid's padding inside its scroll container.
const GRID_PADDING: f32 = 8.;
/// Space kept between a selected tile and the edge of the scroll area.
const SCROLL_MARGIN: f32 = 16.;

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("left", SelectLeft, context),
        KeyBinding::new("right", SelectRight, context),
        KeyBinding::new("up", SelectUp, context),
        KeyBinding::new("down", SelectDown, context),
        KeyBinding::new("enter", OpenSelected, context),
        KeyBinding::new("space", ToggleQuickPreview, context),
        KeyBinding::new("escape", Dismiss, context),
    ]);
}

/// Thumbnails beyond this count are evicted (least recently shown first) to
/// bound memory and GPU atlas usage in large folders.
const MAX_CACHED_THUMBNAILS: usize = 400;

/// Thumbnails load in two steps: a quick one from the photo's embedded
/// preview, then one generated from the full image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    Preview,
    Full,
}

impl Step {
    const ALL: [Step; 2] = [Step::Preview, Step::Full];
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThumbnailState {
    Loading(Step),
    /// The preview step is done; the full step hasn't started.
    NeedsFull,
    Done,
    Failed,
}

struct Thumbnail {
    /// The best image loaded so far.
    image: Option<Arc<RenderImage>>,
    state: ThumbnailState,
    last_shown: u64,
}

/// Hovering a Live Photo's tile waits this long before playing its video,
/// so moving the pointer across the grid doesn't start every one.
const LIVE_HOVER_DELAY: Duration = Duration::from_millis(150);

struct LivePreview {
    ix: usize,
    player: Entity<VideoPlayer>,
    _observe: Subscription,
}

/// What the gallery shows.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    /// The open library.
    Library,
    /// A device's photos, before they're imported.
    Device(Device),
}

enum Status {
    Loading,
    Loaded,
    Error(SharedString),
}

/// What to import.
enum ImportJob {
    /// Files and folders (read recursively).
    Paths(Vec<PathBuf>),
    /// Everything on the device mounted here.
    Device(PathBuf),
}

/// Imports from different drives run at the same time, up to this many.
const MAX_IMPORT_WORKERS: usize = 3;

/// Imports waiting and under way. Each import's photos are found as soon
/// as it's added, then queued by the drive they're on: each drive gets a
/// worker that copies its photos one at a time, in the order they were
/// added, while other drives' workers copy theirs.
#[derive(Default)]
struct ImportQueue {
    /// In the order they were first added to.
    lanes: Vec<ImportLane>,
    /// Imports still finding and reading their photos.
    preparing: usize,
    /// Photos handled (imported, skipped or failed) since the queue was last
    /// empty, out of `total` added.
    done: usize,
    total: usize,
    summary: ImportSummary,
    /// Ids of the libraries photos were added to.
    added_to: HashSet<String>,
}

impl ImportQueue {
    fn is_active(&self) -> bool {
        self.preparing > 0 || !self.lanes.is_empty()
    }

    fn lane(&mut self, key: &LaneKey) -> Option<&mut ImportLane> {
        self.lanes.iter_mut().find(|lane| &lane.key == key)
    }
}

/// A library's id and a source drive.
type LaneKey = (String, u64);

/// Photos waiting to be copied from one drive into one library, and the
/// worker copying them.
struct ImportLane {
    key: LaneKey,
    library: Arc<Library>,
    pending: VecDeque<Photo>,
    worker: Option<Task<()>>,
}

pub struct Gallery {
    page: Page,
    _appearance_subscription: Subscription,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    source: Source,
    status: Status,
    stacks: Arc<Vec<Stack>>,
    masonry: Option<Rc<MasonryState>>,
    /// Bumped whenever something new is shown, so results from in-flight
    /// work for what was shown before are ignored.
    generation: u64,
    thumbnails: HashMap<usize, Thumbnail>,
    /// Stack indices waiting for each step, in priority order.
    queues: [VecDeque<usize>; 2],
    in_flight: [usize; 2],
    max_in_flight: [usize; 2],
    frame: u64,
    /// The stack selected with the keyboard (or last opened).
    selected: Option<usize>,
    quick_preview: Option<Entity<QuickPreview>>,
    viewer: Option<Entity<PhotoViewer>>,
    _viewer_subscription: Option<Subscription>,
    /// The Live Photo playing in place of its tile's thumbnail.
    live_preview: Option<LivePreview>,
    _live_hover: Option<Task<()>>,
    _load_task: Option<Task<()>>,
    imports: ImportQueue,
    /// The library changed while the viewer was open, so it's reloaded when
    /// the viewer closes.
    stale: bool,
    /// The outcome of the last import, or why the gallery changed, shown in
    /// the status bar.
    notice: Option<SharedString>,
    _devices_subscription: Subscription,
}

impl Gallery {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut gallery = Self {
            page: Page::Library,
            _appearance_subscription: settings::observe_system_appearance(window),
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            source: Source::Library,
            status: Status::Loading,
            stacks: Arc::default(),
            masonry: None,
            generation: 0,
            thumbnails: HashMap::new(),
            queues: Default::default(),
            in_flight: [0; 2],
            max_in_flight: {
                let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
                // Full decodes of 24 megapixel raw files need a few hundred
                // MB each while they run, so run fewer of them at once.
                [cores, (cores / 2).clamp(2, 6)]
            },
            frame: 0,
            selected: None,
            quick_preview: None,
            viewer: None,
            _viewer_subscription: None,
            live_preview: None,
            _live_hover: None,
            _load_task: None,
            imports: ImportQueue::default(),
            stale: false,
            notice: None,
            _devices_subscription: cx.observe_global::<devices::Devices>(Self::devices_changed),
        };
        gallery.load(Source::Library, cx);
        gallery
    }

    /// Shows a library from the sidebar, opening it if it isn't open.
    fn show_library(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Library;
        let is_current = library::current(cx).is_some_and(|l| l.info.id == id);
        if is_current && self.source == Source::Library && !matches!(self.status, Status::Error(_))
        {
            self.close_viewer(window, cx);
            return;
        }
        self.notice = None;
        if let Err(error) = library::open(id, cx) {
            show_error("The library couldn't be opened.", &error, window, cx);
            return;
        }
        self.load(Source::Library, cx);
    }

    /// Shows the photos on a device, ready to import.
    fn show_device(&mut self, device: &Device, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Library;
        if self.source == Source::Device(device.clone()) {
            self.close_viewer(window, cx);
        } else {
            self.notice = None;
            self.load(Source::Device(device.clone()), cx);
        }
    }

    /// Shows `source`, reading its photos in the background.
    fn load(&mut self, source: Source, cx: &mut Context<Self>) {
        self.close(cx);
        let generation = self.generation;
        self.source = source.clone();
        let load = match source {
            Source::Library => {
                let Some(library) = library::current(cx) else {
                    let error = library::error(cx).unwrap_or_else(|| "No library is open.".into());
                    self.status = Status::Error(error);
                    return;
                };
                cx.background_spawn(async move { library.photos().map(photos::stacks) })
            }
            Source::Device(device) => {
                cx.background_spawn(async move { photos::load_device(&device.path) })
            }
        };
        self._load_task = Some(cx.spawn(async move |this, cx| {
            let result = load.await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                match result {
                    Ok(stacks) => {
                        this.stacks = Arc::new(stacks);
                        this.build_grid(cx);
                        this.status = Status::Loaded;
                    }
                    Err(error) => this.status = Status::Error(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Lays out the grid for the stacks shown, in the chosen layout. A square
    /// grid is a masonry grid of square tiles, which fill it row by row.
    fn build_grid(&mut self, cx: &App) {
        let (aspect_ratios, options) = match settings::get(cx).grid_layout {
            GridLayout::Masonry => (
                self.stacks
                    .iter()
                    .map(|s| s.cover().aspect_ratio())
                    .collect(),
                MasonryOptions::default(),
            ),
            GridLayout::Square => (
                vec![1.0; self.stacks.len()],
                MasonryOptions {
                    target_column_width: SQUARE_TILE_SIZE,
                    ..MasonryOptions::default()
                },
            ),
        };
        self.masonry = Some(Rc::new(MasonryState::new(aspect_ratios, options)));
    }

    fn set_grid_layout(&mut self, layout: GridLayout, cx: &mut Context<Self>) {
        if settings::get(cx).grid_layout == layout {
            return;
        }
        settings::set_grid_layout(layout, cx);
        if self.masonry.is_some() {
            let anchor = self.scroll_anchor();
            self.build_grid(cx);
            match anchor {
                Some(anchor) => self.restore_scroll_anchor(anchor),
                None => self.scroll_handle.set_offset(point(px(0.), px(0.))),
            }
        }
        cx.notify();
    }

    /// A point in the grid to keep in place when the layout changes: the
    /// middle of the selected tile if it's in view, or else whatever is in
    /// the middle of the view.
    fn scroll_anchor(&self) -> Option<ScrollAnchor> {
        let layout = self.masonry.as_ref()?.layout()?;
        let viewport = f32::from(self.scroll_handle.bounds().size.height);
        let visible_top = -f32::from(self.scroll_handle.offset().y);
        // In the grid's coordinates, without its padding.
        let (top, bottom) = (
            visible_top - GRID_PADDING,
            visible_top - GRID_PADDING + viewport,
        );

        let selected = self.selected.and_then(|ix| {
            let rect = layout.rect(ix)?;
            let middle = rect.origin.y + rect.size.height / 2.;
            (top..=bottom).contains(&middle).then_some((ix, middle))
        });
        let (item, y) = match selected {
            Some(selected) => selected,
            None => {
                let middle = (top + bottom) / 2.;
                let center_x = layout.width() / 2.;
                // The tile at the middle of the view (or nearest it, if the
                // middle falls in a gap), favoring the middle column.
                let distance = |ix: usize| {
                    let rect = layout.rect(ix).unwrap();
                    let dy = (rect.origin.y - middle)
                        .max(middle - (rect.origin.y + rect.size.height))
                        .max(0.);
                    let dx = (rect.origin.x + rect.size.width / 2. - center_x).abs();
                    (dy, dx)
                };
                let ix = layout
                    .items_in_range(top, bottom)
                    .into_iter()
                    .min_by(|&a, &b| {
                        let (a, b) = (distance(a), distance(b));
                        a.0.total_cmp(&b.0).then(a.1.total_cmp(&b.1))
                    })?;
                (ix, middle)
            }
        };
        let rect = layout.rect(item)?;
        Some(ScrollAnchor {
            item,
            fraction: ((y - rect.origin.y) / rect.size.height).clamp(0., 1.),
            view_y: y + GRID_PADDING - visible_top,
            width: layout.width(),
            viewport,
        })
    }

    /// Scrolls the new layout so the anchor's point is where it was in the
    /// view, as near as the grid's height allows.
    fn restore_scroll_anchor(&self, anchor: ScrollAnchor) {
        let Some(state) = self.masonry.as_ref() else {
            return;
        };
        let layout = state.layout_for_width(anchor.width);
        let Some(rect) = layout.rect(anchor.item) else {
            return;
        };
        let y = GRID_PADDING + rect.origin.y + anchor.fraction * rect.size.height;
        let max = (layout.height() + 2. * GRID_PADDING - anchor.viewport).max(0.);
        let scroll_top = (y - anchor.view_y).clamp(0., max);
        self.scroll_handle
            .set_offset(point(px(0.), px(-scroll_top)));
    }

    /// Lets go of the photos shown, ignoring work still in flight for them.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.generation += 1;
        self._load_task = None;
        self.stale = false;
        self._live_hover = None;
        self.stop_live_preview(cx);
        self.close_quick_preview(cx);
        self.selected = None;

        if let Some(viewer) = self.viewer.take() {
            viewer.update(cx, |viewer, cx| viewer.release_images(None, cx));
        }
        self._viewer_subscription = None;

        for (_, thumbnail) in self.thumbnails.drain() {
            if let Some(image) = thumbnail.image {
                cx.drop_image(image, None);
            }
        }
        self.queues = Default::default();
        self.in_flight = [0; 2];
        self.stacks = Arc::default();
        self.masonry = None;
        self.status = Status::Loading;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// Goes back to the library if the device shown has gone away (or is
    /// about to unmount).
    fn devices_changed(&mut self, cx: &mut Context<Self>) {
        let Source::Device(device) = &self.source else {
            return;
        };
        if devices::list(cx).iter().any(|d| d.path == device.path) {
            return;
        }
        self.notice = Some(format!("“{}” was disconnected.", device.name).into());
        self.load(Source::Library, cx);
    }

    fn eject_device(&mut self, device: &Device, window: &mut Window, cx: &mut Context<Self>) {
        // Let go of the device's files first, or it's "in use".
        if self.source == Source::Device(device.clone()) {
            self.load(Source::Library, cx);
        }
        let eject = devices::eject(device, cx);
        let name = device.name.clone();
        cx.spawn_in(window, async move |_, cx| {
            let Err(error) = eject.await else {
                return;
            };
            cx.update(|window, cx| {
                show_error(
                    &format!("“{name}” couldn't be ejected."),
                    &error,
                    window,
                    cx,
                );
            })
            .ok();
        })
        .detach();
    }

    fn prompt_for_import(&mut self, _: &ImportPhotos, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: true,
            multiple: true,
            prompt: Some("Import".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = paths.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.queue_import(ImportJob::Paths(paths), window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn import_device(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Source::Device(device) = &self.source {
            self.queue_import(ImportJob::Device(device.path.clone()), window, cx);
        }
    }

    /// Adds an import to the queue, into the open library. Its photos are
    /// found in the background, then queued to be copied.
    fn queue_import(&mut self, job: ImportJob, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        if !self.imports.is_active() {
            self.notice = None;
        }
        self.imports.preparing += 1;
        cx.notify();
        let prepare_library = library.clone();
        let prepare = cx.background_spawn(async move {
            let prepared = match job {
                ImportJob::Paths(paths) => library::prepare(photos::images_in(&paths)),
                ImportJob::Device(volume) => match photos::device_images(&volume) {
                    Ok(images) => library::prepare(images),
                    Err(error) => PreparedImport {
                        shots: Vec::new(),
                        failed: vec![(volume, format!("{error:#}"))],
                    },
                },
            };
            let id = prepare_library.info.id.clone();
            let shots: Vec<(LaneKey, Photo)> = prepared
                .shots
                .into_iter()
                .map(|shot| ((id.clone(), library::source_drive(&shot.path)), shot))
                .collect();
            (shots, prepared.failed)
        });
        cx.spawn_in(window, async move |this, cx| {
            let (shots, failed) = prepare.await;
            this.update_in(cx, |this, window, cx| {
                let imports = &mut this.imports;
                imports.preparing -= 1;
                imports.total += shots.len();
                imports.summary.failed.extend(failed);
                for (key, shot) in shots {
                    match imports.lane(&key) {
                        Some(lane) => lane.pending.push_back(shot),
                        None => imports.lanes.push(ImportLane {
                            key,
                            library: library.clone(),
                            pending: VecDeque::from([shot]),
                            worker: None,
                        }),
                    }
                }
                this.run_imports(window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Starts workers for drives with photos waiting (up to
    /// `MAX_IMPORT_WORKERS` at once), or wraps up once the queue is empty.
    fn run_imports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.imports.is_active() {
            self.finish_imports(window, cx);
            return;
        }
        let running = self
            .imports
            .lanes
            .iter()
            .filter(|lane| lane.worker.is_some())
            .count();
        let waiting: Vec<(LaneKey, Arc<Library>)> = self
            .imports
            .lanes
            .iter()
            .filter(|lane| lane.worker.is_none() && !lane.pending.is_empty())
            .take(MAX_IMPORT_WORKERS.saturating_sub(running))
            .map(|lane| (lane.key.clone(), lane.library.clone()))
            .collect();
        for (key, library) in waiting {
            let worker = self.import_worker(key.clone(), library, window, cx);
            if let Some(lane) = self.imports.lane(&key) {
                lane.worker = Some(worker);
            }
        }
    }

    /// Copies a lane's photos one at a time until it's empty.
    fn import_worker(
        &mut self,
        key: LaneKey,
        library: Arc<Library>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<()> {
        cx.spawn_in(window, async move |this, cx| {
            let mut session: Option<ImportSession> = None;
            loop {
                let next = this
                    .update(cx, |this, _| {
                        this.imports
                            .lane(&key)
                            .and_then(|lane| lane.pending.pop_front())
                    })
                    .ok()
                    .flatten();
                let Some(shot) = next else {
                    break;
                };
                let previous = session.take();
                let library = library.clone();
                let (next_session, path, result) = cx
                    .background_spawn(async move {
                        let mut current = match previous {
                            Some(session) => Ok(session),
                            None => ImportSession::new(library),
                        };
                        let result = match &mut current {
                            Ok(session) => session.import(&shot),
                            Err(error) => Err(anyhow::anyhow!("{error:#}")),
                        };
                        (current.ok(), shot.path, result)
                    })
                    .await;
                session = next_session;
                this.update(cx, |this, cx| {
                    let imports = &mut this.imports;
                    if matches!(result, Ok(Imported::Added)) {
                        imports.added_to.insert(key.0.clone());
                    }
                    imports.summary.record(&path, result);
                    imports.done += 1;
                    cx.notify();
                })
                .ok();
            }
            let finished = match session {
                Some(session) => cx.background_spawn(async move { session.finish() }).await,
                None => Ok(()),
            };
            this.update_in(cx, |this, window, cx| {
                let imports = &mut this.imports;
                if let Err(error) = finished {
                    let failure = (library.info.path.clone(), format!("{error:#}"));
                    imports.summary.failed.push(failure);
                }
                // Photos may have been queued while the session finished;
                // then the lane stays for a new worker.
                if let Some(lane) = imports.lane(&key) {
                    lane.worker = None;
                }
                imports
                    .lanes
                    .retain(|lane| lane.worker.is_some() || !lane.pending.is_empty());
                this.run_imports(window, cx);
            })
            .ok();
        })
    }

    /// Reports how the imports went, and shows the photos they added.
    fn finish_imports(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let imports = std::mem::take(&mut self.imports);
        cx.notify();
        if imports.total == 0 && imports.summary.failed.is_empty() {
            self.notice = Some("No photos found to import.".into());
            return;
        }
        self.notice = Some(summary_text(&imports.summary).into());
        if !imports.summary.failed.is_empty() {
            let detail = failure_list(&imports.summary.failed);
            show_warning("Some photos couldn't be imported.", &detail, window, cx);
        }
        let showing_added_library = self.source == Source::Library
            && library::current(cx).is_some_and(|l| imports.added_to.contains(&l.info.id));
        if showing_added_library {
            if self.viewer.is_some() {
                self.stale = true;
            } else {
                self.load(Source::Library, cx);
            }
        }
    }

    fn prompt_for_new_library(
        &mut self,
        _: &NewLibrary,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(dir) = library::new_library_dir() else {
            return;
        };
        let path = cx.prompt_for_new_path(&dir, Some("New Library"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(path))) = path.await else {
                return;
            };
            this.update_in(cx, |this, window, cx| {
                this.page = Page::Library;
                if let Err(error) = library::add(path, cx) {
                    show_error("The library couldn't be created.", &error, window, cx);
                }
                this.notice = None;
                this.load(Source::Library, cx);
            })
            .ok();
        })
        .detach();
    }

    /// What's shown, as a category and a name, e.g. "Library" and
    /// "1K Words".
    fn source_title(&self, cx: &App) -> (&'static str, SharedString) {
        match &self.source {
            Source::Library => (
                "Library",
                library::current(cx).map_or_else(|| "None".into(), |l| l.info.name.clone().into()),
            ),
            Source::Device(device) => ("Devices", device.name.clone()),
        }
    }

    fn open_viewer(
        &mut self,
        ix: usize,
        start: StartFrame,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self._live_hover = None;
        self.stop_live_preview(cx);
        self.close_quick_preview(cx);
        self.selected = Some(ix);
        let Some(stack) = self.stacks.get(ix).cloned() else {
            return;
        };
        let cover_thumbnail = self.thumbnails.get(&ix).and_then(|t| t.image.clone());
        let viewer = cx.new(|cx| PhotoViewer::new(stack, cover_thumbnail, start, window, cx));
        self._viewer_subscription = Some(cx.subscribe_in(
            &viewer,
            window,
            |this, _, event: &ViewerEvent, window, cx| match event {
                ViewerEvent::Close => this.close_viewer(window, cx),
                ViewerEvent::Previous => this.open_adjacent_viewer(-1, window, cx),
                ViewerEvent::Next => this.open_adjacent_viewer(1, window, cx),
            },
        ));
        let focus_handle = viewer.focus_handle(cx);
        window.focus(&focus_handle, cx);
        self.viewer = Some(viewer);
        cx.notify();
    }

    /// Replaces the viewer with one for the previous (-1) or next (1) item
    /// in the folder, keeping the grid's selection in step.
    fn open_adjacent_viewer(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(current) = self.selected else {
            return;
        };
        let ix = current as isize + delta;
        if ix < 0 || ix as usize >= self.stacks.len() {
            return;
        }
        let ix = ix as usize;
        if let Some(viewer) = self.viewer.take() {
            viewer.update(cx, |viewer, cx| viewer.release_images(Some(window), cx));
        }
        let start = if delta > 0 {
            StartFrame::First
        } else {
            StartFrame::Last
        };
        self.open_viewer(ix, start, window, cx);
        self.scroll_into_view(ix);
    }

    fn close_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(viewer) = self.viewer.take() {
            viewer.update(cx, |viewer, cx| viewer.release_images(Some(window), cx));
        }
        self._viewer_subscription = None;
        window.focus(&self.focus_handle, cx);
        if self.stale {
            self.load(Source::Library, cx);
        }
        cx.notify();
    }

    /// Whether the grid is showing, so keyboard navigation applies. The
    /// viewer is inside the gallery's key context, so its keys reach here
    /// too.
    fn is_browsing(&self) -> bool {
        self.page == Page::Library && self.viewer.is_none() && !self.stacks.is_empty()
    }

    fn move_selection(&mut self, direction: Direction, cx: &mut Context<Self>) {
        if !self.is_browsing() {
            return;
        }
        // In the quick preview, left and right step through a bracket or
        // burst first, then on to the neighboring photos.
        let delta = match direction {
            Direction::Left => -1,
            Direction::Right => 1,
            Direction::Up | Direction::Down => 0,
        };
        if delta != 0
            && let Some(preview) = &self.quick_preview
            && preview.update(cx, |preview, cx| preview.step(delta, cx))
        {
            return;
        }
        let start = match direction {
            Direction::Right => StartFrame::First,
            Direction::Left => StartFrame::Last,
            Direction::Up | Direction::Down => StartFrame::Cover,
        };
        let Some(layout) = self.masonry.as_ref().and_then(|state| state.layout()) else {
            return;
        };
        let next = match self.selected {
            Some(ix) => layout.neighbor(ix, direction),
            None => self.first_visible(),
        };
        if let Some(ix) = next {
            self.select(ix, start, cx);
        }
    }

    /// The top-left tile that's fully in view, or the first one partly in
    /// view.
    fn first_visible(&self) -> Option<usize> {
        let layout = self.masonry.as_ref()?.layout()?;
        let top = -f32::from(self.scroll_handle.offset().y) - GRID_PADDING;
        let bottom = top + f32::from(self.scroll_handle.bounds().size.height);
        let items = layout.items_in_range(top, bottom);
        let position = |ix: &usize| {
            let rect = layout.rect(*ix).unwrap_or_default();
            (rect.origin.y < top, rect.origin.y, rect.origin.x)
        };
        items
            .into_iter()
            .min_by(|a, b| position(a).partial_cmp(&position(b)).unwrap())
    }

    fn select(&mut self, ix: usize, start: StartFrame, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        self.scroll_into_view(ix);
        if self.quick_preview.is_some() {
            self.open_quick_preview(start, cx);
        }
        cx.notify();
    }

    /// Scrolls the grid just enough to show the tile, plus a margin.
    fn scroll_into_view(&self, ix: usize) {
        let Some(rect) = self
            .masonry
            .as_ref()
            .and_then(|state| state.layout())
            .and_then(|layout| layout.rect(ix))
        else {
            return;
        };
        let offset = self.scroll_handle.offset();
        let viewport = f32::from(self.scroll_handle.bounds().size.height);
        let visible_top = -f32::from(offset.y);
        let top = GRID_PADDING + rect.origin.y - SCROLL_MARGIN;
        let bottom = GRID_PADDING + rect.origin.y + rect.size.height + SCROLL_MARGIN;
        let scroll_top = if top < visible_top {
            top
        } else if bottom > visible_top + viewport {
            // Tiles taller than the view show their top.
            (bottom - viewport).min(top)
        } else {
            return;
        };
        let max = f32::from(self.scroll_handle.max_offset().y);
        self.scroll_handle
            .set_offset(point(offset.x, px(-scroll_top.clamp(0., max.max(0.)))));
    }

    fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_browsing() {
            return;
        }
        if let Some(ix) = self.selected {
            self.open_viewer(ix, StartFrame::Cover, window, cx);
        }
    }

    fn toggle_quick_preview(&mut self, cx: &mut Context<Self>) {
        if self.quick_preview.is_some() {
            self.close_quick_preview(cx);
            return;
        }
        if !self.is_browsing() {
            return;
        }
        if self.selected.is_none() {
            let Some(ix) = self.first_visible() else {
                return;
            };
            self.select(ix, StartFrame::Cover, cx);
        }
        self.open_quick_preview(StartFrame::Cover, cx);
    }

    /// Shows the selected stack in the quick preview, replacing whatever it
    /// was showing.
    fn open_quick_preview(&mut self, start: StartFrame, cx: &mut Context<Self>) {
        let Some(ix) = self.selected else {
            return;
        };
        let Some(stack) = self.stacks.get(ix).cloned() else {
            return;
        };
        self._live_hover = None;
        self.stop_live_preview(cx);
        self.close_quick_preview(cx);
        let thumbnail = self.thumbnails.get(&ix).and_then(|t| t.image.clone());
        self.quick_preview = Some(cx.new(|cx| QuickPreview::new(stack, thumbnail, start, cx)));
        cx.notify();
    }

    fn close_quick_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = self.quick_preview.take() {
            preview.update(cx, |preview, cx| preview.release_images(cx));
            cx.notify();
        }
    }

    /// Escape closes the quick preview, or else clears the selection.
    fn dismiss(&mut self, cx: &mut Context<Self>) {
        if self.quick_preview.is_some() {
            self.close_quick_preview(cx);
        } else if self.is_browsing() && self.selected.take().is_some() {
            cx.notify();
        }
    }

    /// The quick preview floating over the grid, on a backdrop that closes
    /// it when clicked.
    fn render_quick_preview(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let preview = self.quick_preview.clone()?;
        let aspect_ratio = preview.read(cx).aspect_ratio();
        Some(
            div()
                .id("quick-preview")
                .absolute()
                .inset_0()
                .occlude()
                .p_8()
                .bg(rgba(0x00000066))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.close_quick_preview(cx)),
                )
                .child(aspect_fit(
                    aspect_ratio,
                    div()
                        .size_full()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(preview),
                )),
        )
    }

    fn render_settings_title_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let title_bar = div().flex().flex_none().items_center().gap_3().h(px(44.));
        navigation::title_bar_start(title_bar, cx)
            .pr_3()
            .border_b_1()
            .border_color(cx.theme().title_bar_border)
            .bg(cx.theme().title_bar)
            .child(div().text_sm().child("Settings"))
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let gallery = cx.weak_entity();
        let show_device_gallery = gallery.clone();
        let eject_gallery = gallery.clone();
        navigation_sidebar(
            NavigationContext {
                page: self.page,
                showing_library: self.source == Source::Library,
                show_library: Rc::new(move |id, window, cx| {
                    gallery
                        .update(cx, |gallery, cx| gallery.show_library(id, window, cx))
                        .ok();
                }),
                open_device: match &self.source {
                    Source::Device(device) => Some(device.path.clone()),
                    Source::Library => None,
                },
                show_device: Rc::new(move |device, window, cx| {
                    show_device_gallery
                        .update(cx, |gallery, cx| gallery.show_device(device, window, cx))
                        .ok();
                }),
                eject_device: Rc::new(move |device, window, cx| {
                    eject_gallery
                        .update(cx, |gallery, cx| gallery.eject_device(device, window, cx))
                        .ok();
                }),
            },
            cx,
        )
    }

    /// Called by the masonry grid each frame with the items it is about to
    /// render. Queues thumbnails that are missing and evicts ones that have
    /// been off screen the longest.
    fn update_visible(&mut self, visible: &[usize], window: &mut Window, cx: &mut Context<Self>) {
        self.frame += 1;
        for queue in &mut self.queues {
            queue.clear();
        }
        for &ix in visible {
            match self.thumbnails.get_mut(&ix) {
                None => self.queues[Step::Preview as usize].push_back(ix),
                Some(thumbnail) => {
                    thumbnail.last_shown = self.frame;
                    if thumbnail.state == ThumbnailState::NeedsFull {
                        self.queues[Step::Full as usize].push_back(ix);
                    }
                }
            }
        }
        self.load_queued(cx);
        self.evict_thumbnails(visible, window, cx);
    }

    fn load_queued(&mut self, cx: &mut Context<Self>) {
        for step in Step::ALL {
            let s = step as usize;
            while self.in_flight[s] < self.max_in_flight[s] {
                let Some(ix) = self.queues[s].pop_front() else {
                    break;
                };
                let ready = match step {
                    Step::Preview => !self.thumbnails.contains_key(&ix),
                    Step::Full => self
                        .thumbnails
                        .get(&ix)
                        .is_some_and(|t| t.state == ThumbnailState::NeedsFull),
                };
                if ready && let Some(stack) = self.stacks.get(ix) {
                    let photo = stack.cover().clone();
                    self.load_thumbnail(ix, photo, step, cx);
                }
            }
        }
    }

    fn load_thumbnail(&mut self, ix: usize, photo: Photo, step: Step, cx: &mut Context<Self>) {
        let thumbnail = self.thumbnails.entry(ix).or_insert(Thumbnail {
            image: None,
            state: ThumbnailState::Loading(step),
            last_shown: self.frame,
        });
        thumbnail.state = ThumbnailState::Loading(step);
        self.in_flight[step as usize] += 1;
        let generation = self.generation;

        cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    match step {
                        Step::Preview => Ok(decode::preview_thumbnail(&photo)),
                        Step::Full => decode::full_thumbnail(&photo).map(Some),
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.in_flight[step as usize] -= 1;
                // The thumbnail may have been evicted while loading.
                let Some(thumbnail) = this.thumbnails.get_mut(&ix) else {
                    if let Ok(Some(image)) = result {
                        cx.drop_image(image, None);
                    }
                    return;
                };
                if let Ok(Some(image)) = result
                    && let Some(old) = thumbnail.image.replace(image)
                {
                    cx.drop_image(old, None);
                }
                thumbnail.state = match step {
                    Step::Preview => ThumbnailState::NeedsFull,
                    Step::Full if thumbnail.image.is_some() => ThumbnailState::Done,
                    Step::Full => ThumbnailState::Failed,
                };
                this.load_queued(cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn evict_thumbnails(&mut self, visible: &[usize], window: &mut Window, cx: &mut App) {
        let loaded = self
            .thumbnails
            .values()
            .filter(|t| t.image.is_some())
            .count();
        if loaded <= MAX_CACHED_THUMBNAILS {
            return;
        }

        let visible: HashSet<usize> = visible.iter().copied().collect();
        let mut candidates: Vec<(u64, usize)> = self
            .thumbnails
            .iter()
            .filter(|(ix, t)| t.image.is_some() && !visible.contains(ix))
            .map(|(&ix, t)| (t.last_shown, ix))
            .collect();
        candidates.sort_unstable();

        for (_, ix) in candidates.into_iter().take(loaded - MAX_CACHED_THUMBNAILS) {
            if let Some(image) = self.thumbnails.remove(&ix).and_then(|t| t.image) {
                cx.drop_image(image, Some(window));
            }
        }
    }

    fn hover_tile(&mut self, ix: usize, hovered: bool, cx: &mut Context<Self>) {
        if !hovered {
            self._live_hover = None;
            if self.live_preview.as_ref().is_some_and(|p| p.ix == ix) {
                self.stop_live_preview(cx);
            }
            return;
        }
        let Some(video) = self
            .stacks
            .get(ix)
            .and_then(|stack| stack.cover().live_video.clone())
        else {
            return;
        };
        self.stop_live_preview(cx);
        self._live_hover = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIVE_HOVER_DELAY).await;
            this.update(cx, |this, cx| {
                let player = cx.new(|cx| VideoPlayer::new(video, cx));
                // Go back to the still photo once the video has played.
                let observe = cx.observe(&player, |this, player, cx| {
                    if *player.read(cx).state() != PlaybackState::Playing {
                        this.stop_live_preview(cx);
                    }
                    cx.notify();
                });
                this.live_preview = Some(LivePreview {
                    ix,
                    player,
                    _observe: observe,
                });
            })
            .ok();
        }));
    }

    fn stop_live_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(preview) = self.live_preview.take() {
            preview.player.update(cx, |player, cx| player.stop(cx));
            cx.notify();
        }
    }

    fn render_tile(&self, ix: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let tile = div()
            .id(("tile", ix))
            .relative()
            .size_full()
            .rounded(px(TILE_RADIUS))
            .bg(cx.theme().muted)
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_viewer(ix, StartFrame::Cover, window, cx)
            }))
            .on_hover(
                cx.listener(move |this, hovered: &bool, _, cx| this.hover_tile(ix, *hovered, cx)),
            );
        let thumbnail = self.thumbnails.get(&ix);
        let tile = match thumbnail.and_then(|t| t.image.clone()) {
            // Positioned to fill the tile exactly: in flow, a thumbnail of a
            // different shape than its tile (in the square grid) is laid
            // out at its own aspect ratio, overflowing the tile.
            Some(image) => tile.child(
                img(image)
                    .absolute()
                    .inset_0()
                    .size_full()
                    .rounded(px(TILE_RADIUS))
                    .object_fit(ObjectFit::Cover),
            ),
            None if thumbnail.is_some_and(|t| t.state == ThumbnailState::Failed) => tile
                .flex()
                .items_center()
                .justify_center()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Can't display image"),
            _ => tile,
        };
        // A playing Live Photo covers the thumbnail.
        let video = self
            .live_preview
            .as_ref()
            .filter(|preview| preview.ix == ix)
            .and_then(|preview| preview.player.read(cx).render_frame(ObjectFit::Cover));
        // GPUI's video surfaces neither crop when covering nor round their
        // corners, so the video is clipped to the tile (or a frame of a
        // different shape, in the square grid, spills over its neighbours),
        // and its corners are covered with the background. The cover is a
        // ring around the tile whose inner edge follows the tile's rounded
        // corners; clipped to the tile, only the corners of it show.
        let corner_cover = div()
            .absolute()
            .inset(px(-CORNER_COVER_WIDTH))
            .border_4()
            .border_color(cx.theme().background)
            .rounded(px(TILE_RADIUS + CORNER_COVER_WIDTH));
        let tile = tile.children(video.map(|video| {
            div()
                .absolute()
                .inset_0()
                .overflow_hidden()
                .child(video)
                .child(corner_cover)
        }));
        let stack = &self.stacks[ix];
        let stack_icon = match stack.kind {
            StackKind::Single => None,
            StackKind::Bracket => Some(BRACKET_ICON),
            StackKind::Burst => Some(BURST_ICON),
        };
        let raw_plus = stack.cover().raw_pair.is_some();

        let mut segments: Vec<Div> = Vec::new();
        if raw_plus {
            segments.push(div().child("RAW+"));
        }
        if stack.cover().live_video.is_some() {
            segments.push(div().child(badge_icon(LIVE_ICON)));
        }
        if let Some(icon) = stack_icon {
            segments.push(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(badge_icon(icon))
                    .child(stack.photos.len().to_string()),
            );
        }
        let tile = tile.when(!segments.is_empty(), |tile| tile.child(badge(segments)));
        tile.when(self.selected == Some(ix), |tile| {
            // A ring just outside the tile, in the gap between tiles.
            tile.child(
                div()
                    .absolute()
                    .inset(px(-3.))
                    .rounded_lg()
                    .border_2()
                    .border_color(cx.theme().primary),
            )
        })
    }

    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let toolbar = div().flex().flex_none().items_center().gap_3().h(px(44.));
        let no_library = library::current(cx).is_none();
        let layout = settings::get(cx).grid_layout;
        let layout_toggle = ButtonGroup::new("grid-layout")
            .small()
            .outline()
            .child(
                Button::new("masonry")
                    .icon(Icon::new(LucideIcon::LayoutDashboard))
                    .tooltip("Masonry")
                    .selected(layout == GridLayout::Masonry),
            )
            .child(
                Button::new("square")
                    .icon(Icon::new(LucideIcon::Grid2x2))
                    .tooltip("Square Grid")
                    .selected(layout == GridLayout::Square),
            )
            .on_click(cx.listener(|this, clicked: &Vec<usize>, _, cx| {
                let layout = if clicked.contains(&1) {
                    GridLayout::Square
                } else {
                    GridLayout::Masonry
                };
                this.set_grid_layout(layout, cx);
            }));
        let button = match &self.source {
            Source::Library => Button::new("import")
                .icon(Icon::new(LucideIcon::Import))
                .label("Import")
                .small()
                .disabled(no_library)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.prompt_for_import(&ImportPhotos, window, cx)
                })),
            Source::Device(_) => Button::new("import-device")
                .icon(Icon::new(LucideIcon::Import))
                .label("Import All")
                .small()
                .disabled(no_library || self.stacks.is_empty())
                .on_click(cx.listener(|this, _, window, cx| this.import_device(window, cx))),
        };
        let (category, name) = self.source_title(cx);
        let title = div()
            .flex()
            .items_center()
            .gap_1()
            .min_w_0()
            .text_sm()
            .child(
                div()
                    .text_color(cx.theme().muted_foreground)
                    .child(category),
            )
            .child(
                Icon::new(IconName::ChevronRight)
                    .xsmall()
                    .text_color(cx.theme().muted_foreground),
            )
            .child(div().truncate().child(name));
        navigation::title_bar_start(toolbar, cx)
            .pr_3()
            .border_b_1()
            .border_color(cx.theme().title_bar_border)
            .bg(cx.theme().title_bar)
            .child(title)
            .child(div().flex_1())
            .child(layout_toggle)
            .child(button)
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let imports = &self.imports;
        let progress = imports.is_active().then(|| {
            let (label, percent) = import_progress_text(imports.done, imports.total);
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Progress::new("import-progress")
                        .w(px(140.))
                        .small()
                        .loading(percent.is_none())
                        .value(percent.unwrap_or(0.0))
                        .accessibility_label("Import progress"),
                )
                .child(div().text_color(cx.theme().foreground).child(label))
        });
        let detail: Option<SharedString> = if let Some(notice) = &self.notice {
            Some(notice.clone())
        } else {
            match &self.status {
                Status::Loaded => {
                    let photos: usize = self.stacks.iter().map(|s| s.photos.len()).sum();
                    Some(if photos == self.stacks.len() {
                        format!("{photos} photos").into()
                    } else {
                        format!("{photos} photos in {} items", self.stacks.len()).into()
                    })
                }
                Status::Loading => Some("Loading…".into()),
                Status::Error(_) => Some("Error".into()),
            }
        };

        StatusBar::new()
            .flex_none()
            .px_3()
            .when_some(progress, |bar, progress| bar.left(progress))
            .when_some(detail, |bar, detail| bar.right(detail))
    }

    fn render_body(&self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let muted_foreground = cx.theme().muted_foreground;
        let centered = || {
            div()
                .flex_1()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_3()
                .text_color(muted_foreground)
        };

        match &self.status {
            Status::Loading => centered()
                .child(match self.source {
                    Source::Library => "Loading the library…",
                    Source::Device(_) => "Reading the device…",
                })
                .into_any_element(),
            Status::Error(error) => centered().child(error.clone()).into_any_element(),
            Status::Loaded if self.stacks.is_empty() => match self.source {
                Source::Library => centered()
                    .child("No photos yet. Import some to get started.")
                    .child(import_button("empty-import", cx))
                    .into_any_element(),
                Source::Device(_) => centered()
                    .child("No photos on this device.")
                    .into_any_element(),
            },
            Status::Loaded => {
                let state = self
                    .masonry
                    .clone()
                    .expect("masonry state for loaded folder");
                let gallery = cx.entity();
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .id("gallery-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .p_2()
                            // Keep tiles clear of the scrollbar.
                            .pr(px(14.))
                            .child(masonry(state, move |visible, window, cx| {
                                update_and_render(&gallery, visible, window, cx)
                            })),
                    )
                    .vertical_scrollbar(&self.scroll_handle)
                    .into_any_element()
            }
        }
    }
}

fn update_and_render(
    gallery: &Entity<Gallery>,
    visible: &[usize],
    window: &mut Window,
    cx: &mut App,
) -> Vec<Stateful<Div>> {
    gallery.update(cx, |this, cx| {
        this.update_visible(visible, window, cx);
        visible.iter().map(|&ix| this.render_tile(ix, cx)).collect()
    })
}

// Badges sit on top of photos, so they use fixed colors that read on any
// image rather than theme colors.

fn badge_icon(icon: &'static str) -> impl IntoElement {
    svg().path(icon).size_3p5().text_color(rgb(0xffffff))
}

/// The label in a tile's corner, e.g. `RAW+ | {} 3` for a RAW+JPEG bracket
/// of three. Segments are divided by subtle vertical separators.
fn badge(segments: Vec<Div>) -> Div {
    let separator = || div().w(px(1.)).h(px(10.)).bg(rgba(0xffffff4d));
    let mut badge = div()
        .absolute()
        .top_1p5()
        .right_1p5()
        .flex()
        .items_center()
        .gap_1p5()
        .px_1p5()
        .py_0p5()
        .rounded_md()
        .bg(rgba(0x000000a6))
        .text_xs()
        .text_color(rgb(0xffffff));
    for (ix, segment) in segments.into_iter().enumerate() {
        if ix > 0 {
            badge = badge.child(separator());
        }
        badge = badge.child(segment);
    }
    badge
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn import_button(id: &'static str, cx: &mut Context<Gallery>) -> impl IntoElement {
    Button::new(id)
        .icon(Icon::new(LucideIcon::Import))
        .label("Import")
        .small()
        .on_click(
            cx.listener(|this, _, window, cx| this.prompt_for_import(&ImportPhotos, window, cx)),
        )
}

/// The label next to the import progress bar, e.g. "Imported 12 of 340
/// photos · 3%", and the percentage, or none while the total isn't known.
fn import_progress_text(done: usize, total: usize) -> (String, Option<f32>) {
    if total == 0 {
        return ("Preparing import…".into(), None);
    }
    let percent = done as f32 / total as f32 * 100.;
    let photos = if total == 1 { "photo" } else { "photos" };
    let label = format!("Imported {done} of {total} {photos} · {}%", percent.floor());
    (label, Some(percent))
}

/// One line for the status bar, e.g. "Imported 12 photos, skipped 3
/// already in the library".
fn summary_text(summary: &ImportSummary) -> String {
    let plural = |n: usize, one: &str, many: &str| {
        if n == 1 {
            format!("1 {one}")
        } else {
            format!("{n} {many}")
        }
    };
    let mut parts = vec![format!(
        "Imported {}",
        plural(summary.imported, "photo", "photos")
    )];
    if summary.duplicates > 0 {
        parts.push(format!(
            "skipped {} already in the library",
            summary.duplicates
        ));
    }
    if !summary.failed.is_empty() {
        parts.push(format!("{} couldn't be imported", summary.failed.len()));
    }
    parts.join(", ")
}

/// The files that failed, and why, for an alert. Long lists are cut short.
fn failure_list(failed: &[(PathBuf, String)]) -> String {
    const MAX_LISTED: usize = 10;
    let mut lines: Vec<String> = failed
        .iter()
        .take(MAX_LISTED)
        .map(|(path, reason)| format!("{}: {reason}", display_name(path)))
        .collect();
    if failed.len() > MAX_LISTED {
        lines.push(format!("…and {} more", failed.len() - MAX_LISTED));
    }
    lines.join("\n")
}

fn show_error(message: &str, error: &anyhow::Error, window: &mut Window, cx: &mut App) {
    show_warning(message, &format!("{error:#}"), window, cx);
}

fn show_warning(message: &str, detail: &str, window: &mut Window, cx: &mut App) {
    let answer = window.prompt(PromptLevel::Warning, message, Some(detail), &["OK"], cx);
    cx.foreground_executor()
        .spawn(async move {
            answer.await.ok();
        })
        .detach();
}

impl Focusable for Gallery {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for Gallery {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::prompt_for_import))
            .on_action(cx.listener(Self::prompt_for_new_library))
            .on_action(
                cx.listener(|this, _: &SelectLeft, _, cx| this.move_selection(Direction::Left, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectRight, _, cx| {
                    this.move_selection(Direction::Right, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &SelectUp, _, cx| this.move_selection(Direction::Up, cx)),
            )
            .on_action(
                cx.listener(|this, _: &SelectDown, _, cx| this.move_selection(Direction::Down, cx)),
            )
            .on_action(
                cx.listener(|this, _: &OpenSelected, window, cx| this.open_selected(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleQuickPreview, _, cx| this.toggle_quick_preview(cx)),
            )
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| this.dismiss(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.close_quick_preview(cx);
                this.page = Page::Settings;
                cx.notify();
            }))
            .size_full()
            .flex()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_navigation(cx))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .map(|this| match (self.page, &self.viewer) {
                        (Page::Settings, _) => this
                            .child(self.render_settings_title_bar(cx))
                            .child(div().flex_1().min_h_0().child(settings::settings_view())),
                        // The viewer replaces the grid; the grid's scroll
                        // position lives in `scroll_handle`, so it survives
                        // the round trip.
                        (Page::Library, Some(viewer)) => this.child(viewer.clone()),
                        (Page::Library, None) => this
                            .relative()
                            .child(self.render_toolbar(cx))
                            .child(self.render_body(cx))
                            .child(self.render_status_bar(cx))
                            .children(self.render_quick_preview(cx)),
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        AnyWindowHandle, Bounds, TestAppContext, WindowBounds, WindowOptions, point, size,
    };

    use super::*;

    /// A folder of square photos, so the grid fills row by row.
    fn photo_folder(count: usize) -> PathBuf {
        let folder = std::env::temp_dir().join(format!(
            "1k-words-keyboard-test-{}-{count}",
            std::process::id()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        for ix in 0..count {
            image::RgbImage::from_pixel(64, 64, image::Rgb([ix as u8 * 20, 90, 160]))
                .save(folder.join(format!("photo-{ix:02}.jpg")))
                .unwrap();
        }
        folder
    }

    /// Opens a gallery on a fresh library with `folder`'s photos imported.
    fn open_gallery(
        folder: PathBuf,
        cx: &mut TestAppContext,
    ) -> (AnyWindowHandle, Entity<Gallery>) {
        let name = format!("gallery-{}", folder.file_name().unwrap().to_string_lossy());
        let (config_dir, library_dir) = library::test_support::temp_dirs(&name);
        cx.update(|cx| {
            gpui_kit::init(cx);
            settings::init(cx);
            navigation::init(cx);
            library::init_at(config_dir, library_dir, cx);
            bind_keys(cx);
            crate::viewer::bind_keys(cx);
        });
        let library = cx.update(|cx| library::current(cx)).unwrap();
        let summary = library.import(&[folder]).unwrap();
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        let (window, gallery) = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.), px(0.)),
                            size: size(px(1000.), px(700.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |window, cx| {
                        let gallery = cx.new(|cx| Gallery::new(window, cx));
                        window.focus(&gallery.focus_handle(cx), cx);
                        gallery
                    },
                )
            })
            .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        (window, gallery)
    }

    fn press(window: AnyWindowHandle, key: &str, cx: &mut TestAppContext) {
        cx.update_window(window, |_, window, cx| window.press(key, cx))
            .unwrap();
        cx.run_until_parked();
    }

    fn selected(gallery: &Entity<Gallery>, cx: &mut TestAppContext) -> Option<usize> {
        gallery.read_with(cx, |gallery, _| gallery.selected)
    }

    /// Where `direction` should move from `ix`, per the grid's layout.
    fn neighbor(
        gallery: &Entity<Gallery>,
        ix: usize,
        direction: Direction,
        cx: &mut TestAppContext,
    ) -> Option<usize> {
        gallery.read_with(cx, |gallery, _| {
            let layout = gallery.masonry.as_ref()?.layout()?;
            layout.neighbor(ix, direction)
        })
    }

    #[gpui_kit::test]
    fn navigates_and_previews_with_the_keyboard(cx: &mut TestAppContext) {
        let folder = photo_folder(12);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        assert_eq!(gallery.read_with(cx, |g, _| g.stacks.len()), 12);
        assert_eq!(selected(&gallery, cx), None);

        // The first arrow selects the top-left photo.
        press(window, "down", cx);
        assert_eq!(selected(&gallery, cx), Some(0));

        for (key, direction) in [
            ("right", Direction::Right),
            ("down", Direction::Down),
            ("down", Direction::Down),
            ("left", Direction::Left),
            ("up", Direction::Up),
        ] {
            let from = selected(&gallery, cx).unwrap();
            let expected = neighbor(&gallery, from, direction, cx).unwrap_or(from);
            press(window, key, cx);
            assert_eq!(selected(&gallery, cx), Some(expected), "{key} from {from}");
        }
        assert_ne!(selected(&gallery, cx), Some(0), "the selection moved");

        // Space opens the quick preview, which follows the selection.
        press(window, "space", cx);
        let preview_path = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, cx| {
                let preview = gallery.quick_preview.as_ref()?;
                Some(preview.read(cx).cover_path())
            })
        };
        let stack_path = |ix: usize, cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| gallery.stacks[ix].cover().path.clone())
        };
        let ix = selected(&gallery, cx).unwrap();
        assert_eq!(preview_path(cx), Some(stack_path(ix, cx)));
        press(window, "up", cx);
        let ix = selected(&gallery, cx).unwrap();
        assert_eq!(preview_path(cx), Some(stack_path(ix, cx)));

        // Space closes it again, and so does Escape.
        press(window, "space", cx);
        assert_eq!(preview_path(cx), None);
        press(window, "space", cx);
        press(window, "escape", cx);
        assert_eq!(preview_path(cx), None);
        assert_eq!(
            selected(&gallery, cx),
            Some(ix),
            "Escape only closed the preview"
        );

        // Return opens the viewer. Its keys don't move the grid selection,
        // and its Escape closes it.
        press(window, "enter", cx);
        assert!(gallery.read_with(cx, |g, _| g.viewer.is_some()));
        press(window, "down", cx);
        press(window, "space", cx);
        assert_eq!(selected(&gallery, cx), Some(ix));
        assert_eq!(preview_path(cx), None);
        press(window, "escape", cx);
        assert!(gallery.read_with(cx, |g, _| g.viewer.is_none()));

        // Back in the grid, Escape clears the selection.
        press(window, "escape", cx);
        assert_eq!(selected(&gallery, cx), None);
        std::fs::remove_dir_all(folder).ok();
    }

    /// Turns the item at `ix` into a bracket of three, with the middle frame
    /// as its cover.
    fn make_bracket(gallery: &Entity<Gallery>, ix: usize, cx: &mut TestAppContext) {
        gallery.update(cx, |gallery, _| {
            let mut stacks = (*gallery.stacks).clone();
            let mut bracket = stacks[ix].clone();
            bracket.kind = StackKind::Bracket;
            bracket.photos = vec![bracket.photos[0].clone(); 3];
            bracket.cover = 1;
            stacks[ix] = bracket;
            gallery.stacks = Arc::new(stacks);
        });
    }

    #[gpui_kit::test]
    fn moves_between_photos_in_the_viewer(cx: &mut TestAppContext) {
        let folder = photo_folder(4);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        make_bracket(&gallery, 1, cx);
        let shown = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, cx| {
                let viewer = gallery.viewer.as_ref()?;
                let (path, frame) = viewer.read(cx).shown();
                let ix = gallery
                    .stacks
                    .iter()
                    .position(|stack| stack.cover().path == path)?;
                Some((ix, frame, gallery.selected?))
            })
        };

        press(window, "down", cx);
        press(window, "enter", cx);
        assert_eq!(shown(cx), Some((0, 0, 0)));
        // The first photo has no previous one.
        press(window, "left", cx);
        assert_eq!(shown(cx), Some((0, 0, 0)));

        // Right moves to the bracket's first frame, through its frames, and
        // on to the next photo; the grid selection follows.
        for expected in [(1, 0), (1, 1), (1, 2), (2, 0), (3, 0)] {
            press(window, "right", cx);
            let (ix, frame) = expected;
            assert_eq!(shown(cx), Some((ix, frame, ix)));
        }
        // The last photo has no next one.
        press(window, "right", cx);
        assert_eq!(shown(cx), Some((3, 0, 3)));

        // Coming back, the bracket starts on its last frame.
        press(window, "left", cx);
        press(window, "left", cx);
        assert_eq!(shown(cx), Some((1, 2, 1)));

        // Up and down do nothing in the viewer; Escape returns to the grid
        // with the last photo selected.
        press(window, "down", cx);
        assert_eq!(shown(cx), Some((1, 2, 1)));
        press(window, "escape", cx);
        assert_eq!(shown(cx), None);
        assert_eq!(selected(&gallery, cx), Some(1));
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn steps_through_brackets_in_the_quick_preview(cx: &mut TestAppContext) {
        let folder = photo_folder(9);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        make_bracket(&gallery, 1, cx);
        let preview_frame = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, cx| {
                let preview = gallery.quick_preview.as_ref()?;
                Some((gallery.selected?, preview.read(cx).selected_frame()))
            })
        };

        press(window, "down", cx);
        assert_eq!(selected(&gallery, cx), Some(0));
        assert_eq!(neighbor(&gallery, 0, Direction::Right, cx), Some(1));
        press(window, "space", cx);
        assert_eq!(preview_frame(cx), Some((0, 0)));

        // Right from a single photo moves to the bracket, starting on its
        // first frame, then steps through its frames.
        press(window, "right", cx);
        assert_eq!(preview_frame(cx), Some((1, 0)));
        press(window, "right", cx);
        assert_eq!(preview_frame(cx), Some((1, 1)));
        press(window, "right", cx);
        assert_eq!(preview_frame(cx), Some((1, 2)));

        // Past the last frame, it moves on to the next photo.
        let next = neighbor(&gallery, 1, Direction::Right, cx).expect("a third column");
        press(window, "right", cx);
        assert_eq!(preview_frame(cx), Some((next, 0)));

        // Coming back from the right starts on the bracket's last frame.
        press(window, "left", cx);
        assert_eq!(preview_frame(cx), Some((1, 2)));
        press(window, "left", cx);
        assert_eq!(preview_frame(cx), Some((1, 1)));

        // Up and down move through the grid, showing a bracket's cover.
        let below = neighbor(&gallery, 1, Direction::Down, cx).expect("a second row");
        press(window, "down", cx);
        assert_eq!(selected(&gallery, cx), Some(below));
        press(window, "up", cx);
        assert_eq!(preview_frame(cx), Some((1, 1)));
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn shows_a_device_and_returns_to_the_library_when_disconnected(cx: &mut TestAppContext) {
        let folder = photo_folder(2);
        let (window, gallery) = open_gallery(folder.clone(), cx);

        let card =
            std::env::temp_dir().join(format!("1k-words-device-test-{}", std::process::id()));
        for (file, shade) in [
            ("DCIM/100TEST/IMG_0001.JPG", 1),
            ("DCIM/101TEST/IMG_0002.JPG", 2),
        ] {
            let path = card.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            image::RgbImage::from_pixel(64, 64, image::Rgb([40, shade, 160]))
                .save(path)
                .unwrap();
        }
        let device = Device {
            name: "CARD".into(),
            path: card.clone(),
        };
        cx.update(|cx| devices::set_list(vec![device.clone()], cx));

        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.show_device(&device, window, cx));
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        gallery.read_with(cx, |gallery, _| {
            assert_eq!(gallery.source, Source::Device(device.clone()));
            assert!(matches!(gallery.status, Status::Loaded));
            // Photos from every folder under DCIM.
            assert_eq!(gallery.stacks.len(), 2);
        });

        // Import All copies them into the library. Importing again while
        // the first is under way queues it, and skips what's imported.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| {
                gallery.import_device(window, cx);
                gallery.import_device(window, cx);
                assert!(gallery.imports.is_active());
                assert_eq!(gallery.imports.preparing, 2);
            });
        })
        .unwrap();
        cx.run_until_parked();
        gallery.read_with(cx, |gallery, _| {
            assert!(!gallery.imports.is_active());
            assert_eq!(
                gallery.notice.as_deref(),
                Some("Imported 2 photos, skipped 2 already in the library")
            );
        });

        cx.update(|cx| devices::set_list(Vec::new(), cx));
        cx.run_until_parked();
        gallery.read_with(cx, |gallery, _| {
            // Back to the library, with the device's photos added.
            assert_eq!(gallery.source, Source::Library);
            assert_eq!(gallery.stacks.len(), 4);
            assert!(
                gallery
                    .notice
                    .as_ref()
                    .is_some_and(|notice| notice.contains("CARD"))
            );
        });

        std::fs::remove_dir_all(card).ok();
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn toggles_between_masonry_and_square_grids(cx: &mut TestAppContext) {
        // Photos of different shapes, so the layouts differ.
        let folder =
            std::env::temp_dir().join(format!("1k-words-layout-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        for ix in 0..6u32 {
            let (width, height) = if ix % 2 == 0 { (96, 48) } else { (48, 96) };
            image::RgbImage::from_pixel(width, height, image::Rgb([ix as u8 * 30, 90, 160]))
                .save(folder.join(format!("photo-{ix:02}.png")))
                .unwrap();
        }
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let set_layout = |layout, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                gallery.update(cx, |gallery, cx| gallery.set_grid_layout(layout, cx));
                window.render_frame(cx);
            })
            .unwrap();
        };
        let tile_sizes = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                let layout = gallery.masonry.as_ref().unwrap().layout().unwrap();
                (0..6)
                    .map(|ix| {
                        let rect = layout.rect(ix).unwrap();
                        (rect.size.width.round(), rect.size.height.round())
                    })
                    .collect::<Vec<_>>()
            })
        };

        let masonry = tile_sizes(cx);
        assert!(masonry.iter().any(|(w, h)| w != h), "{masonry:?}");

        set_layout(GridLayout::Square, cx);
        assert_eq!(
            cx.update(|cx| settings::get(cx).grid_layout),
            GridLayout::Square
        );
        let square = tile_sizes(cx);
        assert!(square.iter().all(|(w, h)| w == h), "{square:?}");
        // Square tiles fill the grid row by row.
        let right = neighbor(&gallery, 0, Direction::Right, cx);
        assert_eq!(right, Some(1));

        set_layout(GridLayout::Masonry, cx);
        assert_eq!(tile_sizes(cx), masonry);
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn switching_layouts_keeps_roughly_the_same_photos_in_view(cx: &mut TestAppContext) {
        let folder =
            std::env::temp_dir().join(format!("1k-words-scroll-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        for ix in 0..60u32 {
            let (width, height) = [(96, 48), (48, 96), (64, 64)][ix as usize % 3];
            image::RgbImage::from_pixel(width, height, image::Rgb([ix as u8 * 4, 90, 160]))
                .save(folder.join(format!("photo-{ix:02}.png")))
                .unwrap();
        }
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let render = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
        };
        let set_layout = |layout, cx: &mut TestAppContext| {
            gallery.update(cx, |gallery, cx| gallery.set_grid_layout(layout, cx));
            render(cx);
        };
        let scroll_top = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                -f32::from(gallery.scroll_handle.offset().y)
            })
        };
        // The photos in view, and the one in the middle.
        let in_view = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                let layout = gallery.masonry.as_ref().unwrap().layout().unwrap();
                let viewport = f32::from(gallery.scroll_handle.bounds().size.height);
                let top = -f32::from(gallery.scroll_handle.offset().y) - GRID_PADDING;
                let middle = gallery.scroll_anchor().unwrap().item;
                (layout.items_in_range(top, top + viewport), middle)
            })
        };

        // Scroll to the middle of the masonry grid.
        gallery.update(cx, |gallery, _| {
            let max = f32::from(gallery.scroll_handle.max_offset().y);
            assert!(max > 1000., "the grid should scroll");
            gallery
                .scroll_handle
                .set_offset(point(px(0.), px(-(max / 2.).round())));
        });
        render(cx);
        let masonry_top = scroll_top(cx);
        let (masonry_view, middle) = in_view(cx);

        set_layout(GridLayout::Square, cx);
        let (square_view, _) = in_view(cx);
        assert!(
            square_view.contains(&middle),
            "{middle} not in {square_view:?}"
        );
        let kept = masonry_view
            .iter()
            .filter(|ix| square_view.contains(ix))
            .count();
        assert!(
            kept * 2 >= masonry_view.len().min(square_view.len()),
            "kept {kept} of {masonry_view:?} in {square_view:?}"
        );

        // And back again, to about where it was.
        set_layout(GridLayout::Masonry, cx);
        assert!(
            (scroll_top(cx) - masonry_top).abs() < 300.,
            "{} vs {masonry_top}",
            scroll_top(cx)
        );

        // A selected photo in view stays in view.
        let selected = in_view(cx).0[0];
        gallery.update(cx, |gallery, _| gallery.selected = Some(selected));
        set_layout(GridLayout::Square, cx);
        assert!(in_view(cx).0.contains(&selected));
        std::fs::remove_dir_all(folder).ok();
    }

    #[test]
    fn import_progress_label() {
        assert_eq!(
            import_progress_text(0, 0),
            ("Preparing import…".into(), None)
        );
        assert_eq!(
            import_progress_text(12, 340),
            (
                "Imported 12 of 340 photos · 3%".into(),
                Some(12. / 340. * 100.)
            )
        );
        assert_eq!(
            import_progress_text(1, 1),
            ("Imported 1 of 1 photo · 100%".into(), Some(100.))
        );
    }
}

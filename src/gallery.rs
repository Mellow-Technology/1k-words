//! The main gallery view: a toolbar and a virtualized masonry grid of
//! thumbnails for the currently open folder.

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gpui_kit::{
    AnyElement, App, ClickEvent, Context, Div, Entity, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, PathPromptOptions, PromptLevel,
    RenderImage, ScrollHandle, SharedString, Stateful, Subscription, Task, TestSupportExt as _,
    Window, deferred, div, img, point, prelude::*, px, rgb, rgba, svg,
};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonGroup};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Disableable as _, Icon, IconName, Selectable as _, Sizable};

use crate::album_dialogs::{
    ImportAlbum, NameCallback, album_title, prompt_for_album_name, prompt_for_import_album,
};
use crate::aspect_fit::aspect_fit;
use crate::assets::{BRACKET_ICON, BURST_ICON, LIVE_ICON};
use crate::catalog_sync::{self, CatalogChanged};
use crate::decode;
use crate::detection_settings::{self, DetectionSettings};
use crate::devices::{self, Device};
use crate::library::{
    self, Album, AlbumConflict, AlbumId, DetectionStatus, ImportSession, ImportSummary, Imported,
    Library, ObjectKind, PreparedImport, album_path, tree_order,
};
use crate::library_settings::{self, LibraryActions};
use crate::masonry::{Direction, MasonryOptions, MasonryState, masonry};
use crate::navigation::{
    self, DraggedPhotos, NavigationContext, OpenSettings, Page, navigation_sidebar,
};
use crate::photos::{self, Photo, Stack, StackKind, StartFrame};
use crate::quick_preview::QuickPreview;
use crate::settings::{self, GridLayout};
use crate::sync_settings;
use crate::sync_settings::OriginalsChanged;
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
        ExtendLeft,
        ExtendRight,
        ExtendUp,
        ExtendDown,
        SelectAll,
        OpenSelected,
        ToggleQuickPreview,
        Dismiss,
        NewAlbum,
        RemoveFromAlbum,
        NextSuggestion,
        PreviousSuggestion,
        AcceptSuggestion,
        DismissSuggestions
    ]
);

const KEY_CONTEXT: &str = "Gallery";
const SEARCH_WIDTH: f32 = 280.;
/// The key context of the search field, for its suggestions' keys.
const SEARCH_KEY_CONTEXT: &str = "ObjectSearch";
const MAX_SUGGESTIONS: usize = 8;
/// How long typing pauses before the library's searched.
const SEARCH_DELAY: Duration = Duration::from_millis(150);
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
    // Not while typing in the search field, where space types a space.
    let context = Some("Gallery && !Input");
    cx.bind_keys([
        KeyBinding::new("left", SelectLeft, context),
        KeyBinding::new("right", SelectRight, context),
        KeyBinding::new("up", SelectUp, context),
        KeyBinding::new("down", SelectDown, context),
        KeyBinding::new("shift-left", ExtendLeft, context),
        KeyBinding::new("shift-right", ExtendRight, context),
        KeyBinding::new("shift-up", ExtendUp, context),
        KeyBinding::new("shift-down", ExtendDown, context),
        KeyBinding::new("cmd-a", SelectAll, context),
        KeyBinding::new("cmd-backspace", RemoveFromAlbum, context),
        KeyBinding::new("enter", OpenSelected, context),
        KeyBinding::new("space", ToggleQuickPreview, context),
        KeyBinding::new("escape", Dismiss, context),
    ]);
    // In the search field, ahead of its own bindings for these keys (they
    // pass them on when there are no suggestions).
    let search = Some("ObjectSearch > Input");
    cx.bind_keys([
        KeyBinding::new("down", NextSuggestion, search),
        KeyBinding::new("up", PreviousSuggestion, search),
        KeyBinding::new("enter", AcceptSuggestion, search),
        KeyBinding::new("escape", DismissSuggestions, search),
    ]);
}

/// Thumbnails beyond this many bytes of pixels are evicted, least recently
/// shown first, to bound memory and GPU atlas usage in large folders. Each
/// is kept in memory and in the atlas, so they take twice this. It's a few
/// screens of tiles; those on screen are never evicted.
const THUMBNAIL_BUDGET: usize = 150 << 20;

/// Thumbnails load in two steps: a quick one from the photo's embedded
/// preview, then one generated from the full image. A library keeps the
/// thumbnails it's made (see `library::derivatives`), so they're shown in
/// one step once they're there.
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
    /// An album in the open library.
    Album(AlbumId),
    /// A device's photos, before they're imported.
    Device(Device),
}

enum Status {
    Loading,
    Loaded,
    Error(SharedString),
}

/// A box dragged out over the grid to select the tiles it touches.
struct Marquee {
    /// Where it started and where it's dragged to, in the grid's layout
    /// (as `MasonryLayout::rect`), so it stays put as the grid scrolls.
    start: gpui_kit::Point<f32>,
    end: gpui_kit::Point<f32>,
    /// What stays selected as well: what was, if it started with ⌘ or
    /// Shift.
    base: BTreeSet<usize>,
    /// Whether it's moved far enough to be a box rather than a click.
    dragging: bool,
}

/// How far the mouse moves, pressed, before it's dragging out a box.
const MARQUEE_THRESHOLD: f32 = 4.;

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
    /// Each with the album it's to be added to, if any.
    pending: VecDeque<(Photo, Option<AlbumId>)>,
    worker: Option<Task<()>>,
}

pub struct Gallery {
    page: Page,
    _appearance_subscription: Subscription,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    source: Source,
    status: Status,
    /// The open library's albums, for the sidebar.
    albums: Arc<Vec<Album>>,
    /// What's pinned to stay on this device: the albums, and every file of
    /// the photos pinned, themselves or by an album.
    pinned_albums: Arc<Vec<AlbumId>>,
    pinned_files: Arc<HashSet<PathBuf>>,
    _albums_task: Option<Task<()>>,
    /// The files of the photos loaded that aren't on this device, for the
    /// tiles' badges (see `check_on_device`).
    not_here: Arc<HashSet<PathBuf>>,
    _not_here_task: Option<Task<()>>,
    /// Albums deleted on one device but changed on another since, to ask
    /// about one at a time, and whether one's being asked about.
    album_conflicts: VecDeque<AlbumConflict>,
    asking_about_conflict: bool,
    /// Every stack loaded, of which `stacks` are those shown: the ones the
    /// search matches.
    all_stacks: Arc<Vec<Stack>>,
    stacks: Arc<Vec<Stack>>,
    /// Searching the library for kinds of object (see `search_objects`).
    search: Entity<InputState>,
    /// What's searched for, trimmed; empty to show every stack.
    query: String,
    /// Whether a search is underway, so its results aren't reported missing.
    searching: bool,
    _search_task: Option<Task<()>>,
    _search_subscription: Subscription,
    /// The kinds of object found in the library, suggested as a search is
    /// typed. Read again as each new search starts.
    object_kinds: Arc<Vec<ObjectKind>>,
    _object_kinds_task: Option<Task<()>>,
    /// Redraws the status bar while objects are looked for.
    _detection_ticker: Option<Task<()>>,
    /// The suggestion picked with the arrow keys, if any.
    suggestion: Option<usize>,
    /// Hides the suggestions until the search changes, after one's chosen
    /// or they're dismissed.
    suggestions_dismissed: bool,
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
    /// The stack selected last, with the keyboard or a click (or last
    /// opened): where the keyboard moves from, and what the quick preview
    /// shows. It's among `selection` unless it was just ⌘-clicked off.
    selected: Option<usize>,
    /// Every stack selected.
    selection: BTreeSet<usize>,
    /// Where a range selected with Shift starts.
    anchor: Option<usize>,
    /// A box being dragged out over the grid, selecting what it touches.
    marquee: Option<Marquee>,
    /// The last mouse press became a box, so the click it ends with
    /// doesn't select too.
    marquee_dragged: bool,
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
    /// The detector last given to the library (see `detection_changed`):
    /// its key, and whether it was ready.
    applied_detector: Option<(String, bool)>,
    _detection_subscription: Subscription,
    _catalog_subscription: Subscription,
    _originals_subscription: Subscription,
}

impl Gallery {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx
            .new(|cx| InputState::new(window, cx).placeholder("Search for objects, such as dogs"));
        let search_subscription = cx.subscribe_in(
            &search,
            window,
            |this, search, event: &InputEvent, _, cx| match event {
                InputEvent::Change => {
                    this.suggestion = None;
                    this.suggestions_dismissed = false;
                    let query = search.read(cx).value().trim().to_owned();
                    // A new search: what's been found may have changed.
                    if this.query.is_empty() && !query.is_empty() {
                        this.load_object_kinds(cx);
                    }
                    if query != this.query {
                        this.query = query;
                        this.apply_search(SEARCH_DELAY, cx);
                    }
                    cx.notify();
                }
                // Enter is `AcceptSuggestion`.
                _ => {}
            },
        );
        let mut gallery = Self {
            page: Page::Library,
            _appearance_subscription: settings::observe_system_appearance(window),
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            source: Source::Library,
            status: Status::Loading,
            albums: Arc::default(),
            pinned_albums: Arc::default(),
            pinned_files: Arc::default(),
            _albums_task: None,
            not_here: Arc::default(),
            _not_here_task: None,
            album_conflicts: VecDeque::new(),
            asking_about_conflict: false,
            all_stacks: Arc::default(),
            stacks: Arc::default(),
            search,
            query: String::new(),
            searching: false,
            _search_task: None,
            _search_subscription: search_subscription,
            object_kinds: Arc::default(),
            _object_kinds_task: None,
            _detection_ticker: None,
            suggestion: None,
            suggestions_dismissed: false,
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
            selection: BTreeSet::new(),
            anchor: None,
            marquee: None,
            marquee_dragged: false,
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
            applied_detector: None,
            _detection_subscription: cx
                .observe_global::<DetectionSettings>(Self::detection_changed),
            _catalog_subscription: cx.observe_global::<CatalogChanged>(Self::catalog_changed),
            _originals_subscription: cx.observe_global::<OriginalsChanged>(Self::originals_changed),
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

    /// Whether the open library is shown, whole or an album of it.
    fn showing_library(&self) -> bool {
        matches!(self.source, Source::Library | Source::Album(_))
    }

    /// Reads the open library's albums again, for the sidebar.
    fn load_albums(&mut self, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            self.albums = Arc::default();
            return;
        };
        self._albums_task = Some(cx.spawn(async move |this, cx| {
            let (albums, conflicts, pins) = cx
                .background_spawn(async move {
                    let pins = library.pinned_albums().and_then(|albums| {
                        let files = library.pinned_originals()?;
                        let root = &library.info.path;
                        let files: HashSet<PathBuf> =
                            files.iter().map(|file| root.join(file)).collect();
                        Ok((albums, files))
                    });
                    (library.albums(), library.album_conflicts(), pins)
                })
                .await;
            this.update(cx, |this, cx| {
                match albums {
                    Ok(albums) => this.albums = Arc::new(albums),
                    Err(error) => eprintln!("couldn't read the albums: {error:#}"),
                }
                match pins {
                    Ok((albums, files)) => {
                        this.pinned_albums = Arc::new(albums);
                        this.pinned_files = Arc::new(files);
                    }
                    Err(error) => eprintln!("couldn't read what's pinned: {error:#}"),
                }
                match conflicts {
                    Ok(conflicts) => this.album_conflicts = conflicts.into(),
                    Err(error) => eprintln!("couldn't look for album conflicts: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Asks whether to keep an album deleted on one device but changed on
    /// another since (see `Library::album_conflicts`), if there is one.
    fn ask_about_album_conflict(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.asking_about_conflict {
            return;
        }
        let Some(conflict) = self.album_conflicts.pop_front() else {
            return;
        };
        let Some(library) = library::current(cx) else {
            return;
        };
        self.asking_about_conflict = true;
        let (message, detail) = album_conflict_text(&conflict);
        let answer = window.prompt(
            PromptLevel::Warning,
            &message,
            Some(&detail),
            &["Keep Album", "Delete Album"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let keep = match answer.await {
                Ok(0) => true,
                Ok(_) => false,
                // Asked again next time.
                Err(_) => {
                    this.update(cx, |this, _| this.asking_about_conflict = false)
                        .ok();
                    return;
                }
            };
            let result = cx
                .background_spawn(
                    async move { library.resolve_album_conflict(conflict.album, keep) },
                )
                .await;
            this.update_in(cx, |this, window, cx| {
                this.asking_about_conflict = false;
                if let Err(error) = result {
                    show_error("The album couldn't be kept.", &error, window, cx);
                }
                this.load_albums(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Shows an album of the open library.
    fn show_album(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        self.page = Page::Library;
        if self.source == Source::Album(album) && !matches!(self.status, Status::Error(_)) {
            self.close_viewer(window, cx);
            return;
        }
        self.notice = None;
        self.load(Source::Album(album), cx);
    }

    /// Asks for a name for a new album, in `parent` if given, makes it and
    /// shows it.
    fn new_album(&mut self, parent: Option<AlbumId>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let title = match parent {
            Some(parent) => format!("New Album in “{}”", album_title(&self.albums, parent)),
            None => "New Album".into(),
        };
        let gallery = cx.weak_entity();
        let create: NameCallback = Rc::new(move |name, window, cx| {
            let library = library.clone();
            let gallery = gallery.clone();
            let create = cx.background_spawn(async move { library.create_album(&name, parent) });
            window.spawn(cx, async move |cx| {
                let album = create.await?;
                gallery.update_in(cx, |gallery, window, cx| {
                    gallery.show_album(album, window, cx)
                })?;
                Ok(())
            })
        });
        prompt_for_album_name(title, "Create", "", create, window, cx);
    }

    fn rename_album(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let Some(name) = self
            .albums
            .iter()
            .find(|a| a.id == album)
            .map(|a| a.name.clone())
        else {
            return;
        };
        let gallery = cx.weak_entity();
        let rename: NameCallback = Rc::new(move |name, window, cx| {
            let library = library.clone();
            let gallery = gallery.clone();
            let rename = cx.background_spawn(async move { library.rename_album(album, &name) });
            window.spawn(cx, async move |cx| {
                rename.await?;
                gallery.update(cx, |gallery, cx| gallery.load_albums(cx))?;
                Ok(())
            })
        });
        prompt_for_album_name("Rename Album", "Rename", &name, rename, window, cx);
    }

    /// Whether the album shown shows `album`'s photos: it's that album, or
    /// it shows the photos of the albums in it, which `album` is among.
    fn shows_album(&self, album: AlbumId) -> bool {
        match self.source {
            Source::Album(shown) => library::shown_albums(&self.albums, shown).contains(&album),
            _ => false,
        }
    }

    /// Puts an album in another, or at the top level.
    fn move_album(
        &mut self,
        album: AlbumId,
        parent: Option<AlbumId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(library) = library::current(cx) else {
            return;
        };
        // An album showing those in it may show different photos after.
        let affects_shown = match self.source {
            Source::Album(shown) => self.albums.iter().any(|a| a.id == shown && a.show_nested),
            _ => false,
        };
        let moving = cx.background_spawn(async move { library.move_album(album, parent) });
        cx.spawn_in(window, async move |this, cx| {
            let moved = moving.await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = moved {
                    show_error("The album couldn't be moved.", &error, window, cx);
                }
                this.albums_rearranged(affects_shown, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Turns showing the photos of the albums in an album on or off.
    fn toggle_show_nested(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let show = !self.albums.iter().any(|a| a.id == album && a.show_nested);
        let affects_shown = self.source == Source::Album(album);
        let setting =
            cx.background_spawn(async move { library.set_album_show_nested(album, show) });
        cx.spawn_in(window, async move |this, cx| {
            let set = setting.await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = set {
                    show_error("The album couldn't be changed.", &error, window, cx);
                }
                this.albums_rearranged(affects_shown, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Pins an album to stay on this device, downloading what it needs, or
    /// takes the pin off.
    fn toggle_album_pin(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let pin = !self.pinned_albums.contains(&album);
        self.change_pins(pin, window, cx, {
            let library = library.clone();
            move || match pin {
                true => library.pin_album(album),
                false => library.unpin_album(album),
            }
        });
    }

    /// Pins the selected photos to stay on this device, downloading what
    /// they need; or if they're all pinned, takes their pins off.
    fn toggle_selection_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let photos = self.selected_photos();
        self.toggle_photos_pin(photos, window, cx);
    }

    /// Pins `photos` to stay on this device, downloading what they need; or
    /// if they're all pinned, takes their pins off.
    fn toggle_photos_pin(
        &mut self,
        photos: Vec<Photo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let pin = !self.all_pinned(&photos);
        self.change_pins(pin, window, cx, {
            let library = library.clone();
            move || {
                for photo in &photos {
                    match pin {
                        true => library.pin_photo(photo)?,
                        false => library.unpin_photo(photo)?,
                    }
                }
                Ok(())
            }
        });
    }

    /// Whether every selected photo is pinned, itself or by an album.
    fn selection_pinned(&self) -> bool {
        self.all_pinned(&self.selected_photos())
    }

    /// Whether there are photos, and every one is pinned, itself or by an
    /// album.
    fn all_pinned(&self, photos: &[Photo]) -> bool {
        !photos.is_empty() && photos.iter().all(|p| self.pinned_files.contains(&p.path))
    }

    /// Looks for which files of the photos loaded aren't on this device, in
    /// the background, for the tiles' badges. It goes by the disk, as the
    /// viewer does, so it's right for originals removed outside the app
    /// too. Only a library's photos can be elsewhere.
    fn check_on_device(&mut self, cx: &mut Context<Self>) {
        if !self.showing_library() {
            self.not_here = Arc::default();
            self._not_here_task = None;
            return;
        }
        let (stacks, generation) = (self.all_stacks.clone(), self.generation);
        self._not_here_task = Some(cx.spawn(async move |this, cx| {
            let not_here = cx
                .background_spawn(async move { files_not_here(&stacks) })
                .await;
            this.update(cx, |this, cx| {
                if this.generation == generation && *this.not_here != not_here {
                    this.not_here = Arc::new(not_here);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    /// Reads what's on this device again after originals have come or
    /// gone: for the tiles' badges, and the viewer's sidebar.
    fn originals_changed(&mut self, cx: &mut Context<Self>) {
        let library_id = &cx.global::<OriginalsChanged>().library_id;
        let showing_changed_library = self.showing_library()
            && library::current(cx).is_some_and(|l| &l.info.id == library_id);
        if !showing_changed_library {
            return;
        }
        self.check_on_device(cx);
        if let Some(viewer) = &self.viewer {
            viewer.update(cx, |viewer, cx| viewer.load_on_device(cx));
        }
    }

    /// Changes what's pinned with `change`, in the background, then, if
    /// `pinned`, downloads what that needs.
    fn change_pins(
        &mut self,
        pinned: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce() -> anyhow::Result<()> + Send + 'static,
    ) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let change = cx.background_spawn(async move { change() });
        cx.spawn_in(window, async move |this, cx| {
            let changed = change.await;
            let download = this.update_in(cx, |this, window, cx| {
                this.load_albums(cx);
                if let Some(viewer) = &this.viewer {
                    viewer.update(cx, |viewer, cx| viewer.load_on_device(cx));
                }
                if let Err(error) = &changed {
                    show_error("What's kept couldn't be changed.", error, window, cx);
                }
                (pinned && changed.is_ok()).then(|| {
                    this.notice = Some("Downloading what's needed…".into());
                    cx.notify();
                    crate::sync_settings::download_pinned(library, cx)
                })
            });
            let Ok(Some(download)) = download else { return };
            let downloaded = download.await;
            this.update(cx, |this, cx| {
                this.notice = Some(pin_result_text(downloaded).into());
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Reads the albums again after they've been rearranged, and the photos
    /// of the album shown if they may have changed too.
    fn albums_rearranged(&mut self, affects_shown: bool, cx: &mut Context<Self>) {
        self.load_albums(cx);
        if affects_shown {
            if self.viewer.is_some() {
                self.stale = true;
            } else {
                self.load(self.source.clone(), cx);
            }
        }
        cx.notify();
    }

    /// Asks to delete an album, and the albums in it, then does. Their
    /// photos stay in the library.
    fn delete_album(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        let Some(name) = self
            .albums
            .iter()
            .find(|a| a.id == album)
            .map(|a| a.name.clone())
        else {
            return;
        };
        let nested = tree_order(&self.albums)
            .into_iter()
            .filter(|(_, other)| {
                other.id != album
                    && album_path(&self.albums, other.id)
                        .iter()
                        .any(|a| a.id == album)
            })
            .count();
        let detail = match nested {
            0 => "Its photos stay in the library.".to_owned(),
            1 => "The album in it is deleted too. Their photos stay in the library.".to_owned(),
            count => format!(
                "The {count} albums in it are deleted too. Their photos stay in the library."
            ),
        };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete the album “{name}”?"),
            Some(&detail),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            let delete = library.clone();
            let result = cx
                .background_spawn(async move { delete.delete_album(album) })
                .await;
            this.update_in(cx, |this, window, cx| {
                if let Err(error) = result {
                    show_error("The album couldn't be deleted.", &error, window, cx);
                }
                // Showing it, or an album in it: back to the library.
                let showing_deleted = match this.source {
                    Source::Album(shown) => album_path(&this.albums, shown)
                        .iter()
                        .any(|a| a.id == album),
                    _ => false,
                };
                if showing_deleted {
                    this.load(Source::Library, cx);
                } else {
                    this.load_albums(cx);
                }
            })
            .ok();
        })
        .detach();
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
                self.apply_detector(&library, cx);
                self.scan_library(library.clone(), cx);
                self.load_albums(cx);
                cx.background_spawn(async move { library.photos().map(photos::stacks) })
            }
            Source::Album(album) => {
                let Some(library) = library::current(cx) else {
                    self.status = Status::Error("No library is open.".into());
                    return;
                };
                self.load_albums(cx);
                cx.background_spawn(async move { library.album_photos(album).map(photos::stacks) })
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
                        this.all_stacks = Arc::new(stacks);
                        this.status = Status::Loaded;
                        this.apply_search(Duration::ZERO, cx);
                        this.check_on_device(cx);
                    }
                    Err(error) => this.status = Status::Error(format!("{error:#}").into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Shows the stacks with photos of what's searched for, all of them if
    /// nothing is, once they're found (after `delay`, so typing isn't
    /// slowed by a search for each key). Devices aren't searched.
    fn apply_search(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let library = match self.source {
            Source::Library | Source::Album(_) if !self.query.is_empty() => library::current(cx),
            _ => None,
        };
        let Some(library) = library else {
            self._search_task = None;
            self.searching = false;
            self.show_stacks(self.all_stacks.clone(), cx);
            return;
        };
        let query = self.query.clone();
        self.searching = true;
        self._search_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(delay).await;
            let found = cx
                .background_spawn(async move { library.search_objects(&query) })
                .await;
            this.update(cx, |this, cx| {
                let found = found.unwrap_or_else(|error| {
                    eprintln!("couldn't search the library: {error:#}");
                    HashSet::new()
                });
                let stacks = this
                    .all_stacks
                    .iter()
                    .filter(|stack| stack.photos.iter().any(|p| found.contains(&p.path)))
                    .cloned()
                    .collect();
                this.searching = false;
                this.show_stacks(Arc::new(stacks), cx);
            })
            .ok();
        }));
        cx.notify();
    }

    /// Reads the kinds of object found in the library, to suggest.
    fn load_object_kinds(&mut self, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        self._object_kinds_task = Some(cx.spawn(async move |this, cx| {
            let kinds = cx
                .background_spawn(async move { library.object_kinds() })
                .await;
            this.update(cx, |this, cx| {
                match kinds {
                    Ok(kinds) => this.object_kinds = Arc::new(kinds),
                    Err(error) => eprintln!("couldn't read the kinds of object: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// The kinds of object to suggest for what's typed in the search field:
    /// none once one's chosen, or when what's typed already names the only
    /// one.
    /// Only while the search field has focus.
    fn visible_suggestions(&self, window: &Window, cx: &App) -> Vec<&ObjectKind> {
        if !self.showing_library()
            || self.suggestions_dismissed
            || !self.search.focus_handle(cx).is_focused(window)
        {
            return Vec::new();
        }
        let mut found = library::suggestions(&self.query, &self.object_kinds);
        found.truncate(MAX_SUGGESTIONS);
        if let [only] = found.as_slice()
            && only.label.eq_ignore_ascii_case(&self.query)
        {
            return Vec::new();
        }
        found
    }

    /// Moves the highlight through the suggestions by `step`, wrapping
    /// around. Passes the key on to the search field when there are none.
    fn move_suggestion(&mut self, step: isize, window: &Window, cx: &mut Context<Self>) {
        let count = self.visible_suggestions(window, cx).len() as isize;
        if count == 0 {
            cx.propagate();
            return;
        }
        let next = match self.suggestion {
            Some(ix) => (ix as isize + step).rem_euclid(count),
            None if step > 0 => 0,
            None => count - 1,
        };
        self.suggestion = Some(next as usize);
        cx.notify();
    }

    /// Enter in the search field: searches for the highlighted suggestion,
    /// or, with none, goes on to the results, to pick one with the keyboard.
    fn accept_suggestion(
        &mut self,
        _: &AcceptSuggestion,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let label = self.suggestion.and_then(|ix| {
            let suggestions = self.visible_suggestions(window, cx);
            suggestions.get(ix).map(|kind| kind.label.clone())
        });
        match label {
            Some(label) => self.choose_suggestion(label, window, cx),
            None => window.focus(&self.focus_handle, cx),
        }
    }

    /// Searches for the kind of object `label`, and goes on to the results.
    fn choose_suggestion(&mut self, label: String, window: &mut Window, cx: &mut Context<Self>) {
        self.search
            .update(cx, |search, cx| search.set_value(label.clone(), window, cx));
        self.suggestion = None;
        self.suggestions_dismissed = true;
        if label != self.query {
            self.query = label;
            self.apply_search(Duration::ZERO, cx);
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    fn dismiss_suggestions(
        &mut self,
        _: &DismissSuggestions,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.visible_suggestions(window, cx).is_empty() {
            cx.propagate();
            return;
        }
        self.suggestion = None;
        self.suggestions_dismissed = true;
        cx.notify();
    }

    /// Shows `stacks` in place of those shown, once loading's done; the
    /// load shows what the search matches when it finishes.
    fn show_stacks(&mut self, stacks: Arc<Vec<Stack>>, cx: &mut Context<Self>) {
        if !matches!(self.status, Status::Loaded) {
            return;
        }
        // Thumbnails on their way are for the stacks shown before.
        self.generation += 1;
        self._live_hover = None;
        self.stop_live_preview(cx);
        self.close_quick_preview(cx);
        self.clear_selection();
        self.clear_thumbnails(cx);
        self.stacks = stacks;
        self.build_grid(cx);
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn clear_thumbnails(&mut self, cx: &mut Context<Self>) {
        for (_, thumbnail) in self.thumbnails.drain() {
            if let Some(image) = thumbnail.image {
                decode::release(image, cx);
            }
        }
        self.queues = Default::default();
        self.in_flight = [0; 2];
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
        self.clear_selection();

        if let Some(viewer) = self.viewer.take() {
            viewer.update(cx, |viewer, cx| viewer.release_images(None, cx));
        }
        self._viewer_subscription = None;

        self.clear_thumbnails(cx);
        self._search_task = None;
        self.searching = false;
        self.object_kinds = Arc::default();
        self.all_stacks = Arc::default();
        self.stacks = Arc::default();
        self.masonry = None;
        self.status = Status::Loading;
        self.scroll_handle.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    /// Gives the library the chosen detector (see `detection_settings`).
    /// Returns whether it's changed since it was last given.
    fn apply_detector(&mut self, library: &Library, cx: &App) -> bool {
        let key = detection_settings::current_key(cx);
        let detector = detection_settings::current(cx);
        let applied = Some((key.clone(), detector.is_some()));
        let changed = self.applied_detector != applied;
        self.applied_detector = applied;
        library.set_detector(key, detector);
        changed
    }

    /// Makes the library's missing thumbnails, then has its detector find
    /// the objects in the photos it hasn't looked at, in the background,
    /// while the missing previews and posters are made.
    /// Tests don't find objects, so they don't download the model.
    fn scan_library(&mut self, library: Arc<Library>, cx: &mut Context<Self>) {
        let finished = Arc::new(AtomicBool::new(false));
        self._detection_ticker = Some(cx.spawn({
            let (library, finished) = (library.clone(), finished.clone());
            async move |this, cx| {
                while !finished.load(Ordering::Relaxed) {
                    cx.background_executor().timer(DETECTION_TICK).await;
                    if library.detection_status().is_some()
                        && this.update(cx, |_, cx| cx.notify()).is_err()
                    {
                        return;
                    }
                }
            }
        }));
        cx.spawn(async move |this, cx| {
            cx.background_spawn({
                let library = library.clone();
                async move {
                    if let Err(error) = library.refresh_originals() {
                        eprintln!("couldn't check the library's originals: {error:#}");
                    }
                    if let Err(error) = library.make_missing_thumbnails() {
                        eprintln!("couldn't make the library's thumbnails: {error:#}");
                    }
                }
            })
            .await;
            cx.background_spawn({
                let library = library.clone();
                async move {
                    if let Err(error) = library.make_missing_previews() {
                        eprintln!("couldn't make the library's previews: {error:#}");
                    }
                }
            })
            .detach();
            // From the thumbnails, so after them.
            let found = cx
                .background_spawn(
                    async move { (!cfg!(test)).then(|| library.detect_missing_objects()) },
                )
                .await;
            let error = match found {
                Some(Err(error)) => {
                    eprintln!("couldn't find objects in the library's photos: {error:#}");
                    Some(format!("{error:#}"))
                }
                Some(Ok(_)) => None,
                None => return,
            };
            finished.store(true, Ordering::Relaxed);
            cx.update(|cx| detection_settings::set_error(error, cx));
            this.update(cx, |_, cx| cx.notify()).ok();
        })
        .detach();
    }

    /// Uses another detector, or the chosen one now it's ready: looks for
    /// objects with it, and searches what it's found.
    fn detection_changed(&mut self, cx: &mut Context<Self>) {
        let Some(library) = library::current(cx) else {
            return;
        };
        if !self.apply_detector(&library, cx) {
            return;
        }
        if self.showing_library() {
            self.scan_library(library, cx);
        }
        self.load_object_kinds(cx);
        self.apply_search(Duration::ZERO, cx);
    }

    /// Shows changes another device made to the open library, which a sync
    /// has just fetched.
    fn catalog_changed(&mut self, cx: &mut Context<Self>) {
        let library_id = &cx.global::<CatalogChanged>().library_id;
        let showing_changed_library = self.showing_library()
            && library::current(cx).is_some_and(|l| &l.info.id == library_id);
        if !showing_changed_library {
            return;
        }
        // As after an import: the viewer's photos are left as they are.
        if self.viewer.is_some() {
            self.stale = true;
            self.load_albums(cx);
        } else {
            self.load(self.source.clone(), cx);
        }
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
                this.choose_import_album(ImportJob::Paths(paths), window, cx);
            })
            .ok();
        })
        .detach();
    }

    fn import_device(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Source::Device(device) = &self.source {
            let job = ImportJob::Device(device.path.clone());
            self.choose_import_album(job, window, cx);
        }
    }

    /// Asks which album to add the photos being imported to, if any, then
    /// imports them. Starts with the album shown, if one is.
    fn choose_import_album(&mut self, job: ImportJob, window: &mut Window, cx: &mut Context<Self>) {
        if library::current(cx).is_none() {
            return;
        }
        let shown = match self.source {
            Source::Album(album) => Some(album),
            _ => None,
        };
        let job = Rc::new(RefCell::new(Some(job)));
        let gallery = cx.weak_entity();
        let on_import = Rc::new(
            move |album: Option<ImportAlbum>, window: &mut Window, cx: &mut App| {
                let Some(job) = job.borrow_mut().take() else {
                    return;
                };
                gallery
                    .update(cx, |gallery, cx| {
                        gallery.import_into(job, album, window, cx)
                    })
                    .ok();
            },
        );
        prompt_for_import_album(&self.albums, shown, on_import, window, cx);
    }

    /// Imports into `album`, making it first if it's new.
    fn import_into(
        &mut self,
        job: ImportJob,
        album: Option<ImportAlbum>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (name, parent) = match album {
            None => return self.queue_import(job, None, window, cx),
            Some(ImportAlbum::Existing(album)) => {
                return self.queue_import(job, Some(album), window, cx);
            }
            Some(ImportAlbum::New { name, parent }) => (name, parent),
        };
        let Some(library) = library::current(cx) else {
            return;
        };
        let create = cx.background_spawn(async move { library.create_album(&name, parent) });
        cx.spawn_in(window, async move |this, cx| {
            let created = create.await;
            this.update_in(cx, |this, window, cx| match created {
                Ok(album) => {
                    this.load_albums(cx);
                    this.queue_import(job, Some(album), window, cx);
                }
                Err(error) => show_error("The album couldn't be made.", &error, window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Adds an import to the queue, into the open library. Its photos are
    /// found in the background, then queued to be copied.
    fn queue_import(
        &mut self,
        job: ImportJob,
        album: Option<AlbumId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(library) = library::current(cx) else {
            return;
        };
        if library_settings::is_busy(cx) {
            show_warning(
                "Please wait until the library is free.",
                "Photos can't be imported while a library is being moved or deleted.",
                window,
                cx,
            );
            return;
        }
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
                        Some(lane) => lane.pending.push_back((shot, album)),
                        None => imports.lanes.push(ImportLane {
                            key,
                            library: library.clone(),
                            pending: VecDeque::from([(shot, album)]),
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
                let Some((shot, album)) = next else {
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
                            Ok(session) => session.import(&shot, album),
                            Err(error) => Err(anyhow::anyhow!("{error:#}")),
                        };
                        (current.ok(), shot.path, result)
                    })
                    .await;
                session = next_session;
                this.update(cx, |this, cx| {
                    let imports = &mut this.imports;
                    // Changed by what's added, or by an album added to.
                    if matches!(result, Ok(Imported::Added)) || (album.is_some() && result.is_ok())
                    {
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
        if self.page == Page::Settings {
            library_settings::refresh_stats(cx);
        }
        if imports.total == 0 && imports.summary.failed.is_empty() {
            self.notice = Some("No photos found to import.".into());
            return;
        }
        self.notice = Some(summary_text(&imports.summary).into());
        for id in &imports.added_to {
            sync_settings::sync_in_background(id, cx);
            catalog_sync::sync_in_background(id, cx);
        }
        if !imports.summary.failed.is_empty() {
            let detail = failure_list(&imports.summary.failed);
            show_warning("Some photos couldn't be imported.", &detail, window, cx);
        }
        let showing_added_library = self.showing_library()
            && library::current(cx).is_some_and(|l| imports.added_to.contains(&l.info.id));
        if showing_added_library {
            if self.viewer.is_some() {
                self.stale = true;
                self.load_albums(cx);
            } else {
                self.load(self.source.clone(), cx);
            }
        }
    }

    /// Whether a library can be moved or deleted now, warning if not.
    fn can_change_libraries(&self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if self.imports.is_active()
            || library_settings::is_busy(cx)
            || sync_settings::is_syncing(cx)
        {
            show_warning(
                "Please wait until the library is free.",
                "Libraries can't be moved or deleted while photos are being imported or \
                 synced, or another library is being moved or deleted.",
                window,
                cx,
            );
            return false;
        }
        true
    }

    /// Asks where to move a library's folder, then moves it there.
    fn move_library(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change_libraries(window, cx) {
            return;
        }
        let Some(handle) = library::handle(id, cx) else {
            return;
        };
        let Some(parent) = handle.info.path.parent() else {
            return;
        };
        let dest = cx.prompt_for_new_path(parent, Some(&handle.info.name));
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(dest))) = dest.await else {
                return;
            };
            let progress = cx.update(|_, cx| library_settings::start(&id, "Moving", cx));
            let Ok(progress) = progress else {
                return;
            };
            let to = dest.clone();
            let result = cx
                .background_spawn(async move { handle.move_to(&to, &progress) })
                .await;
            this.update_in(cx, |this, window, cx| {
                library_settings::finish(&id, cx);
                match result.and_then(|warning| {
                    library::relocate(&id, dest, cx)?;
                    Ok(warning)
                }) {
                    Ok(warning) => {
                        if let Some(warning) = warning {
                            show_warning("The library was moved.", &warning, window, cx);
                        }
                    }
                    Err(error) => show_error("The library couldn't be moved.", &error, window, cx),
                }
                this.libraries_changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// Asks to confirm, then moves a library's folder to the Trash and
    /// forgets it.
    fn delete_library(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self.can_change_libraries(window, cx) {
            return;
        }
        let Some(handle) = library::handle(id, cx) else {
            return;
        };
        let photos = match library_settings::photo_count(id, cx) {
            Some(1) => "its photo".to_owned(),
            Some(count) => format!("all {count} of its photos"),
            None => "all its photos".to_owned(),
        };
        let detail = format!(
            "The library's folder, with {photos}, will be moved to the Trash from {}. \
             Its catalog and sync settings will be deleted.",
            handle.info.path.display()
        );
        let answer = window.prompt(
            PromptLevel::Critical,
            &format!("Delete “{}”?", handle.info.name),
            Some(&detail),
            &["Move to Trash", "Cancel"],
            cx,
        );
        let id = id.to_owned();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            if cx
                .update(|_, cx| library_settings::start(&id, "Deleting", cx))
                .is_err()
            {
                return;
            }
            let result = cx.background_spawn(async move { handle.delete() }).await;
            this.update_in(cx, |this, window, cx| {
                library_settings::finish(&id, cx);
                if let Err(error) = result.and_then(|()| library::forget(&id, cx)) {
                    show_error("The library couldn't be deleted.", &error, window, cx);
                }
                this.libraries_changed(cx);
            })
            .ok();
        })
        .detach();
    }

    /// After a library's moved or deleted: shows the open library afresh,
    /// and updates the Libraries settings.
    fn libraries_changed(&mut self, cx: &mut Context<Self>) {
        if self.showing_library() {
            self.load(Source::Library, cx);
        }
        library_settings::refresh_stats(cx);
        cx.notify();
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
            Source::Album(album) => ("Albums", album_title(&self.albums, *album).into()),
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
        if self.selection.contains(&ix) {
            self.selected = Some(ix);
        } else {
            self.select_only(ix);
        }
        let Some(stack) = self.stacks.get(ix).cloned() else {
            return;
        };
        let cover_thumbnail = self.thumbnails.get(&ix).and_then(|t| t.image.clone());
        // For its albums menu.
        let library = self
            .showing_library()
            .then(|| library::current(cx))
            .flatten();
        let viewer =
            cx.new(|cx| PhotoViewer::new(stack, cover_thumbnail, start, library, window, cx));
        self._viewer_subscription = Some(cx.subscribe_in(
            &viewer,
            window,
            |this, _, event: &ViewerEvent, window, cx| match event {
                ViewerEvent::Close => this.close_viewer(window, cx),
                ViewerEvent::Previous => this.open_adjacent_viewer(-1, window, cx),
                ViewerEvent::Next => this.open_adjacent_viewer(1, window, cx),
                ViewerEvent::AlbumsChanged => {
                    this.load_albums(cx);
                    if matches!(this.source, Source::Album(_)) {
                        this.stale = true;
                    }
                }
                ViewerEvent::TogglePin(photo) => {
                    this.toggle_photos_pin(vec![(**photo).clone()], window, cx)
                }
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
            self.load(self.source.clone(), cx);
        }
        cx.notify();
    }

    /// Whether the grid is showing, so keyboard navigation applies. The
    /// viewer is inside the gallery's key context, so its keys reach here
    /// too.
    fn is_browsing(&self) -> bool {
        self.page == Page::Library && self.viewer.is_none() && !self.stacks.is_empty()
    }

    /// Moves the selection to the neighbouring tile, or with `extend`,
    /// selects the range from the anchor to it.
    fn move_selection(&mut self, direction: Direction, extend: bool, cx: &mut Context<Self>) {
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
            // Past the grid's left or right edge, on to the previous or next
            // photo.
            Some(ix) => layout.neighbor(ix, direction).or_else(|| {
                ix.checked_add_signed(delta)
                    .filter(|&ix| delta != 0 && ix < self.stacks.len())
            }),
            None => self.first_visible(),
        };
        let Some(ix) = next else {
            return;
        };
        if extend {
            self.select_range(ix, false);
            self.scroll_into_view(ix);
            cx.notify();
        } else {
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
        self.select_only(ix);
        self.scroll_into_view(ix);
        if self.quick_preview.is_some() {
            self.open_quick_preview(start, cx);
        }
        cx.notify();
    }

    /// A point in the window, in the grid's layout.
    fn grid_point(&self, position: gpui_kit::Point<gpui_kit::Pixels>) -> gpui_kit::Point<f32> {
        let bounds = self.scroll_handle.bounds();
        let offset = self.scroll_handle.offset();
        gpui_kit::point(
            f32::from(position.x - bounds.origin.x - offset.x) - GRID_PADDING,
            f32::from(position.y - bounds.origin.y - offset.y) - GRID_PADDING,
        )
    }

    /// A press in the grid: it may become a box, once it's dragged.
    fn start_marquee(&mut self, event: &MouseDownEvent) {
        self.marquee_dragged = false;
        if !self.is_browsing() || self.quick_preview.is_some() {
            return;
        }
        let point = self.grid_point(event.position);
        let keep = event.modifiers.shift || event.modifiers.platform;
        // A press on the selection drags it instead (see
        // `render_tile_with_menu`), unless ⌘ or Shift is held, to add a box
        // to it.
        if self.showing_library() && !keep && self.selected_at(point) {
            return;
        }
        self.marquee = Some(Marquee {
            start: point,
            end: point,
            base: if keep {
                self.selection.clone()
            } else {
                BTreeSet::new()
            },
            dragging: false,
        });
    }

    /// Whether a selected tile is at `point`, in the grid's layout.
    fn selected_at(&self, point: gpui_kit::Point<f32>) -> bool {
        let Some(layout) = self.masonry.as_ref().and_then(|state| state.layout()) else {
            return false;
        };
        layout
            .items_in_range(point.y, point.y)
            .into_iter()
            .filter(|ix| self.selection.contains(ix))
            .any(|ix| {
                layout.rect(ix).is_some_and(|rect| {
                    (rect.origin.x..=rect.origin.x + rect.size.width).contains(&point.x)
                        && (rect.origin.y..=rect.origin.y + rect.size.height).contains(&point.y)
                })
            })
    }

    /// Follows the mouse with the box, selecting the tiles it touches, and
    /// scrolls when it's dragged past the top or bottom of the grid.
    fn drag_marquee(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marquee.is_none() {
            return;
        }
        // Let go somewhere the gallery didn't see.
        if event.pressed_button != Some(MouseButton::Left) {
            self.end_marquee(cx);
            return;
        }
        let bounds = self.scroll_handle.bounds();
        let beyond = if event.position.y < bounds.top() {
            event.position.y - bounds.top()
        } else if event.position.y > bounds.bottom() {
            event.position.y - bounds.bottom()
        } else {
            px(0.)
        };
        if beyond != px(0.) {
            let offset = self.scroll_handle.offset();
            let max = self.scroll_handle.max_offset().y;
            let y = (offset.y - beyond).clamp(-max, px(0.));
            self.scroll_handle.set_offset(point(offset.x, y));
        }
        let end = self.grid_point(event.position);
        let Some(marquee) = self.marquee.as_mut() else {
            return;
        };
        marquee.end = end;
        let moved = (end.x - marquee.start.x).hypot(end.y - marquee.start.y);
        if !marquee.dragging && moved < MARQUEE_THRESHOLD {
            return;
        }
        if !marquee.dragging {
            marquee.dragging = true;
            self.marquee_dragged = true;
            self._live_hover = None;
            self.stop_live_preview(cx);
        }
        // A box started on the selection (with ⌘ or Shift) isn't dragging
        // it too.
        if cx.has_active_drag() {
            cx.stop_active_drag(window);
        }
        self.select_in_marquee();
        cx.notify();
    }

    fn end_marquee(&mut self, cx: &mut Context<Self>) {
        if self.marquee.take().is_some_and(|marquee| marquee.dragging) {
            cx.notify();
        }
    }

    /// Selects the tiles the box touches, as well as what it started with.
    fn select_in_marquee(&mut self) {
        let (Some(marquee), Some(layout)) = (
            self.marquee.as_ref(),
            self.masonry.as_ref().and_then(|state| state.layout()),
        ) else {
            return;
        };
        let (left, right) = (
            marquee.start.x.min(marquee.end.x),
            marquee.start.x.max(marquee.end.x),
        );
        let (top, bottom) = (
            marquee.start.y.min(marquee.end.y),
            marquee.start.y.max(marquee.end.y),
        );
        let touched: Vec<usize> = layout
            .items_in_range(top, bottom)
            .into_iter()
            .filter(|&ix| {
                layout.rect(ix).is_some_and(|rect| {
                    rect.origin.x <= right
                        && rect.origin.x + rect.size.width >= left
                        && rect.origin.y <= bottom
                        && rect.origin.y + rect.size.height >= top
                })
            })
            .collect();
        let mut selection = marquee.base.clone();
        selection.extend(touched.iter().copied());
        self.selection = selection;
        self.selected = touched
            .iter()
            .min()
            .copied()
            .or(self.selected.filter(|ix| self.selection.contains(ix)));
        self.anchor = self.selected;
    }

    /// The box being dragged out, over the grid it's in.
    fn render_marquee(&self, cx: &App) -> Option<impl IntoElement> {
        let marquee = self.marquee.as_ref().filter(|marquee| marquee.dragging)?;
        let offset = self.scroll_handle.offset();
        let size = self.scroll_handle.bounds().size;
        // From the grid's layout to the view, cut to what's in view.
        let x = |x: f32| (x + GRID_PADDING + f32::from(offset.x)).clamp(0., f32::from(size.width));
        let y = |y: f32| (y + GRID_PADDING + f32::from(offset.y)).clamp(0., f32::from(size.height));
        let (left, right) = (
            x(marquee.start.x.min(marquee.end.x)),
            x(marquee.start.x.max(marquee.end.x)),
        );
        let (top, bottom) = (
            y(marquee.start.y.min(marquee.end.y)),
            y(marquee.start.y.max(marquee.end.y)),
        );
        let primary = cx.theme().primary;
        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(right - left))
                .h(px(bottom - top))
                .rounded_sm()
                .border_1()
                .border_color(primary.opacity(0.7))
                .bg(primary.opacity(0.15)),
        )
    }

    /// Selects just the tile at `ix`.
    fn select_only(&mut self, ix: usize) {
        self.selection = BTreeSet::from([ix]);
        self.selected = Some(ix);
        self.anchor = Some(ix);
    }

    fn clear_selection(&mut self) {
        self.selection.clear();
        self.selected = None;
        self.anchor = None;
    }

    /// Selects the tiles from the anchor to `ix`, in the grid's order, in
    /// place of what's selected, or with `add`, as well.
    fn select_range(&mut self, ix: usize, add: bool) {
        let anchor = self.anchor.or(self.selected).unwrap_or(ix);
        let range = anchor.min(ix)..=anchor.max(ix);
        if !add {
            self.selection.clear();
        }
        self.selection.extend(range);
        self.selected = Some(ix);
        self.anchor = Some(anchor);
    }

    /// Selects the tile at `ix`, or deselects it if it's selected.
    fn toggle_selected(&mut self, ix: usize) {
        if !self.selection.remove(&ix) {
            self.selection.insert(ix);
        }
        self.selected = Some(ix);
        self.anchor = Some(ix);
    }

    /// A click on a tile: one selects it alone, with ⌘ toggles it, with
    /// Shift selects the range to it (⌘⇧ adds the range); two open it.
    fn click_tile(
        &mut self,
        ix: usize,
        event: &ClickEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marquee_dragged {
            return;
        }
        if event.click_count() >= 2 {
            self.open_viewer(ix, StartFrame::Cover, window, cx);
            return;
        }
        let modifiers = event.modifiers();
        if modifiers.shift {
            self.select_range(ix, modifiers.platform);
        } else if modifiers.platform {
            self.toggle_selected(ix);
        } else {
            self.select_only(ix);
        }
        if self.quick_preview.is_some() && self.selection.contains(&ix) {
            self.open_quick_preview(StartFrame::Cover, cx);
        }
        cx.notify();
    }

    fn select_all(&mut self, cx: &mut Context<Self>) {
        if !self.is_browsing() {
            return;
        }
        self.selection = (0..self.stacks.len()).collect();
        if self.selected.is_none() {
            self.selected = Some(0);
            self.anchor = Some(0);
        }
        cx.notify();
    }

    /// The photos of every stack selected (each frame of a bracket or
    /// burst), in the grid's order.
    fn selected_photos(&self) -> Vec<Photo> {
        self.selection
            .iter()
            .filter_map(|&ix| self.stacks.get(ix))
            .flat_map(|stack| stack.photos.iter().cloned())
            .collect()
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
        } else if self.is_browsing() && !self.selection.is_empty() {
            self.clear_selection();
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

    fn library_actions(&self, cx: &mut Context<Self>) -> LibraryActions {
        let gallery = cx.weak_entity();
        let delete_gallery = gallery.clone();
        LibraryActions {
            move_library: Rc::new(move |id, window, cx| {
                gallery
                    .update(cx, |gallery, cx| gallery.move_library(id, window, cx))
                    .ok();
            }),
            delete_library: Rc::new(move |id, window, cx| {
                delete_gallery
                    .update(cx, |gallery, cx| gallery.delete_library(id, window, cx))
                    .ok();
            }),
        }
    }

    fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let gallery = cx.weak_entity();
        let show_device_gallery = gallery.clone();
        let eject_gallery = gallery.clone();
        let albums_gallery = gallery.clone();
        let new_album_gallery = gallery.clone();
        let move_album_gallery = gallery.clone();
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
                    _ => None,
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
                albums: self.albums.clone(),
                open_album: match self.source {
                    Source::Album(album) => Some(album),
                    _ => None,
                },
                show_album: album_callback(&albums_gallery, Self::show_album),
                new_album: Rc::new(move |parent, window, cx| {
                    new_album_gallery
                        .update(cx, |gallery, cx| gallery.new_album(parent, window, cx))
                        .ok();
                }),
                rename_album: album_callback(&albums_gallery, Self::rename_album),
                delete_album: album_callback(&albums_gallery, Self::delete_album),
                drop_on_album: album_callback(&albums_gallery, Self::drop_on_album),
                move_album: Rc::new(move |album, parent, window, cx| {
                    move_album_gallery
                        .update(cx, |gallery, cx| {
                            gallery.move_album(album, parent, window, cx)
                        })
                        .ok();
                }),
                toggle_show_nested: album_callback(&albums_gallery, Self::toggle_show_nested),
                pinned_albums: self.pinned_albums.clone(),
                toggle_album_pin: album_callback(&albums_gallery, Self::toggle_album_pin),
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
        // Library photos' thumbnails are kept in the library once made.
        let library = photo
            .thumbnail
            .is_some()
            .then(|| library::current(cx))
            .flatten();

        cx.spawn(async move |this, cx| {
            // The image, and whether it's the finished thumbnail.
            let result = cx
                .background_spawn(async move {
                    match (step, library) {
                        (Step::Preview, _) => {
                            if let Some(kept) =
                                photo.thumbnail.as_deref().and_then(decode::kept_thumbnail)
                            {
                                return Ok((Some(kept), true));
                            }
                            Ok((decode::preview_thumbnail(&photo), false))
                        }
                        (Step::Full, Some(library)) => library
                            .make_thumbnail(&photo)
                            .map(|image| (Some(decode::to_render_image(image)), true)),
                        (Step::Full, None) => {
                            decode::full_thumbnail(&photo).map(|image| (Some(image), true))
                        }
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
                    if let Ok((Some(image), _)) = result {
                        decode::release(image, cx);
                    }
                    return;
                };
                let finished = matches!(result, Ok((_, true)));
                if let Ok((Some(image), _)) = result
                    && let Some(old) = thumbnail.image.replace(image)
                {
                    decode::release(old, cx);
                }
                thumbnail.state = match step {
                    Step::Preview if finished => ThumbnailState::Done,
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
        let loaded = self.thumbnails.iter().filter_map(|(&ix, thumbnail)| {
            let bytes = thumbnail.image.as_ref()?.as_bytes(0).map_or(0, <[u8]>::len);
            Some((ix, thumbnail.last_shown, bytes))
        });
        for ix in evictions(loaded, visible, THUMBNAIL_BUDGET) {
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
            // It may have been evicted, and isn't fetched just for a hover.
            .filter(|video| video.is_file())
        else {
            return;
        };
        self.stop_live_preview(cx);
        self._live_hover = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(LIVE_HOVER_DELAY).await;
            this.update(cx, |this, cx| {
                let player = cx.new(|cx| VideoPlayer::new(video, false, cx));
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

    /// A tile, with a menu on right-click to add the selected photos (this
    /// tile's, if it isn't selected) to an album, if they're the library's.
    fn render_tile_with_menu(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let tile = self.render_tile(ix, cx);
        if !self.showing_library() {
            // Found by UI tests.
            return tile.test_support().into_any_element();
        }
        // The selection is dragged by any of its tiles, onto an album in the
        // sidebar. (Elsewhere, dragging draws a box: see `start_marquee`.)
        let tile = if self.selection.contains(&ix) {
            let dragged = DraggedPhotos {
                count: self.selection.len(),
            };
            tile.on_drag(dragged, |dragged, _, _, cx| cx.new(|_| dragged.clone()))
        } else {
            tile
        };
        let tile = tile.test_support();
        let gallery = cx.weak_entity();
        let albums = self.albums.clone();
        let shown_album = match self.source {
            Source::Album(album) => Some(album),
            _ => None,
        };
        tile.context_menu(move |menu, window, cx| {
            let (gallery, albums) = (gallery.clone(), albums.clone());
            let pinned = gallery
                .read_with(cx, |gallery, _| gallery.selection_pinned())
                .unwrap_or(false);
            let keep = {
                let gallery = gallery.clone();
                PopupMenuItem::new("Keep on This Device")
                    .checked(pinned)
                    .on_click(move |_, window, cx| {
                        gallery
                            .update(cx, |gallery, cx| gallery.toggle_selection_pin(window, cx))
                            .ok();
                    })
            };
            let remove = shown_album.map(|_| {
                let gallery = gallery.clone();
                PopupMenuItem::new("Remove from Album").on_click(move |_, window, cx| {
                    gallery
                        .update(cx, |gallery, cx| {
                            gallery.remove_from_shown_album(window, cx)
                        })
                        .ok();
                })
            });
            let menu = menu.submenu("Add to Album", window, cx, move |mut menu, _, _| {
                for (_, album) in tree_order(&albums) {
                    let album = album.id;
                    let title = album_title(&albums, album);
                    let gallery = gallery.clone();
                    menu = menu.item(PopupMenuItem::new(title.clone()).on_click(
                        move |_, window, cx| {
                            gallery
                                .update(cx, |gallery, cx| {
                                    gallery.add_to_album(album, &title, window, cx)
                                })
                                .ok();
                        },
                    ));
                }
                if !albums.is_empty() {
                    menu = menu.separator();
                }
                let gallery = gallery.clone();
                menu.item(
                    PopupMenuItem::new("New Album…").on_click(move |_, window, cx| {
                        gallery
                            .update(cx, |gallery, cx| gallery.add_to_new_album(window, cx))
                            .ok();
                    }),
                )
            });
            let menu = match remove {
                Some(remove) => menu.item(remove),
                None => menu,
            };
            menu.item(keep)
        })
        .into_any_element()
    }

    /// Photos dragged onto an album in the sidebar: the selection.
    fn drop_on_album(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        let title = album_title(&self.albums, album);
        self.add_to_album(album, &title, window, cx);
    }

    /// Takes the selected photos out of the album shown. They stay in the
    /// library.
    fn remove_from_shown_album(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Source::Album(album) = self.source else {
            return;
        };
        let photos = self.selected_photos();
        let Some(library) = library::current(cx).filter(|_| !photos.is_empty()) else {
            return;
        };
        let title = album_title(&self.albums, album);
        let remove = cx.background_spawn(async move { library.remove_from_album(album, &photos) });
        cx.spawn_in(window, async move |this, cx| {
            let removed = remove.await;
            this.update_in(cx, |this, window, cx| match removed {
                Ok(removed) => {
                    this.load(this.source.clone(), cx);
                    this.notice = Some(removed_from_album_text(&title, removed).into());
                }
                Err(error) => show_error(
                    "The photos couldn't be taken out of the album.",
                    &error,
                    window,
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Adds the selected photos (every frame of a bracket or burst) to an
    /// album called `title`.
    fn add_to_album(
        &mut self,
        album: AlbumId,
        title: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let photos = self.selected_photos();
        let Some(library) = library::current(cx).filter(|_| !photos.is_empty()) else {
            return;
        };
        let count = photos.len();
        let title = title.to_owned();
        let add = cx.background_spawn(async move { library.add_to_album(album, &photos) });
        cx.spawn_in(window, async move |this, cx| {
            let added = add.await;
            this.update_in(cx, |this, window, cx| match added {
                Ok(added) => this.added_to_album(album, &title, count, added, cx),
                Err(error) => show_error(
                    "The photos couldn't be added to the album.",
                    &error,
                    window,
                    cx,
                ),
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a name for a new album, makes it, and adds the selected
    /// photos to it.
    fn add_to_new_album(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let photos = self.selected_photos();
        let Some(library) = library::current(cx).filter(|_| !photos.is_empty()) else {
            return;
        };
        let gallery = cx.weak_entity();
        let create: NameCallback = Rc::new(move |name, window, cx| {
            let (library, photos, gallery) = (library.clone(), photos.clone(), gallery.clone());
            let count = photos.len();
            let title = name.trim().to_owned();
            let create = cx.background_spawn(async move {
                let album = library.create_album(&name, None)?;
                let added = library.add_to_album(album, &photos)?;
                anyhow::Ok((album, added))
            });
            window.spawn(cx, async move |cx| {
                let (album, added) = create.await?;
                gallery.update(cx, |gallery, cx| {
                    gallery.added_to_album(album, &title, count, added, cx)
                })?;
                Ok(())
            })
        });
        prompt_for_album_name("New Album", "Create", "", create, window, cx);
    }

    /// Says how adding `count` photos to an album went, and shows them in
    /// it.
    fn added_to_album(
        &mut self,
        album: AlbumId,
        title: &str,
        count: usize,
        added: usize,
        cx: &mut Context<Self>,
    ) {
        self.notice = Some(added_to_album_text(title, count, added).into());
        self.load_albums(cx);
        if added > 0 && self.shows_album(album) {
            if self.viewer.is_some() {
                self.stale = true;
            } else {
                self.load(self.source.clone(), cx);
            }
        }
        cx.notify();
    }

    fn render_tile(&self, ix: usize, cx: &mut Context<Self>) -> Stateful<Div> {
        let tile = div()
            .id(("tile", ix))
            .relative()
            .size_full()
            .rounded(px(TILE_RADIUS))
            .bg(cx.theme().muted)
            .cursor_pointer()
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                // Not on to the grid, which would clear the selection.
                cx.stop_propagation();
                this.click_tile(ix, event, window, cx)
            }))
            // Right-clicking a tile that isn't selected selects it, for its
            // menu to act on.
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(move |this, _, _, cx| {
                    if !this.selection.contains(&ix) {
                        this.select_only(ix);
                        cx.notify();
                    }
                }),
            )
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
                .child(
                    match self
                        .stacks
                        .get(ix)
                        .is_some_and(|stack| self.not_here.contains(&stack.cover().path))
                    {
                        true => "Not on this device",
                        false => "Can't display image",
                    },
                ),
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
        // Some of its originals aren't on this device, such as a raw file
        // removed to free up space; or they're all pinned to stay here.
        if stack
            .photos
            .iter()
            .flat_map(Photo::files)
            .any(|file| self.not_here.contains(file))
        {
            segments.push(div().child(badge_lucide(LucideIcon::CloudOff)));
        }
        if self.all_pinned(&stack.photos) {
            segments.push(div().child(badge_lucide(LucideIcon::Pin)));
        }
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
        tile.when(self.selection.contains(&ix), |tile| {
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

    fn render_toolbar(&self, window: &Window, cx: &mut Context<Self>) -> impl IntoElement {
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
            Source::Library | Source::Album(_) => Button::new("import")
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
        let search = self.showing_library().then(|| {
            div()
                .relative()
                .w(px(SEARCH_WIDTH))
                .min_w(px(140.))
                .key_context(SEARCH_KEY_CONTEXT)
                .on_action(cx.listener(|this, _: &NextSuggestion, window, cx| {
                    this.move_suggestion(1, window, cx)
                }))
                .on_action(cx.listener(|this, _: &PreviousSuggestion, window, cx| {
                    this.move_suggestion(-1, window, cx)
                }))
                .on_action(cx.listener(Self::accept_suggestion))
                .on_action(cx.listener(Self::dismiss_suggestions))
                .child(
                    Input::new(&self.search)
                        .small()
                        .cleanable(true)
                        .prefix(
                            Icon::new(IconName::Search)
                                .small()
                                .text_color(cx.theme().muted_foreground),
                        )
                        .disabled(no_library),
                )
                .children(self.render_suggestions(window, cx))
        });
        // The sides share what's left equally, so the search is centred.
        navigation::title_bar_start(toolbar, cx)
            .pr_3()
            .border_b_1()
            .border_color(cx.theme().title_bar_border)
            .bg(cx.theme().title_bar)
            .child(div().flex_1().min_w_0().child(title))
            .children(search)
            .child(
                div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_end()
                    .gap_3()
                    .child(layout_toggle)
                    .child(button),
            )
    }

    /// The kinds of object that what's typed could be, under the search
    /// field, with how many photos each is in.
    fn render_suggestions(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let suggestions = self.visible_suggestions(window, cx);
        if suggestions.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (accent, accent_foreground, muted_foreground) = (
            theme.accent,
            theme.accent_foreground,
            theme.muted_foreground,
        );
        let list = div()
            .id("object-suggestions")
            .absolute()
            .top_full()
            .left_0()
            .right_0()
            .mt_1()
            // Keep the mouse from reaching the grid underneath.
            .occlude()
            .p_1()
            .flex()
            .flex_col()
            .rounded_md()
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .text_color(theme.popover_foreground)
            .shadow_md()
            .children(suggestions.into_iter().enumerate().map(|(ix, kind)| {
                let highlighted = self.suggestion == Some(ix);
                let photos = match kind.photos {
                    1 => "1 photo".to_owned(),
                    count => format!("{count} photos"),
                };
                let label = kind.label.clone();
                div()
                    .id(("object-suggestion", ix))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .text_sm()
                    .cursor_pointer()
                    .hover(|row| row.bg(accent))
                    .when(highlighted, |row| {
                        row.bg(accent).text_color(accent_foreground)
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(crate::details::capitalize(&kind.label)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(muted_foreground)
                            .child(photos),
                    )
                    // On mouse down, before the search field loses focus and
                    // the suggestions go.
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.choose_suggestion(label.clone(), window, cx);
                        }),
                    )
                    // Found by UI tests.
                    .test_support()
            }));
        // Over the grid, drawn after it.
        Some(deferred(list).with_priority(1))
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let imports = &self.imports;
        let import_progress = imports.is_active().then(|| {
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
        let sync_progress = sync_settings::progress(cx).map(|sync| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Progress::new("sync-progress")
                        .w(px(140.))
                        .small()
                        .value(sync.percent)
                        .accessibility_label("Sync progress"),
                )
                .child(div().text_color(cx.theme().foreground).child(sync.text()))
        });
        let detection = self.render_detection_status(cx);
        let progress = (import_progress.is_some()
            || sync_progress.is_some()
            || detection.is_some())
        .then(|| {
            div()
                .flex()
                .items_center()
                .gap_4()
                .children(import_progress)
                .children(sync_progress)
                .children(detection)
        });
        let detail: Option<SharedString> = if let Some(notice) = &self.notice {
            Some(notice.clone())
        } else {
            match &self.status {
                Status::Loaded if self.searching => Some("Searching…".into()),
                Status::Loaded => {
                    let photos: usize = self.stacks.iter().map(|s| s.photos.len()).sum();
                    let mut count = if photos == self.stacks.len() {
                        format!("{photos} photos")
                    } else {
                        format!("{photos} photos in {} items", self.stacks.len())
                    };
                    if self.selection.len() > 1 {
                        count = format!("{} selected of {count}", self.selection.len());
                    }
                    Some(match self.is_searched() {
                        true => format!("{count} with “{}”", self.query).into(),
                        false => count.into(),
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

    /// How looking for objects in the library's photos is going, with the
    /// local model or a plugin; or why it stopped.
    fn render_detection_status(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let library = library::current(cx)?;
        let name = detection_settings::current_name(cx);
        let Some(status) = library.detection_status() else {
            let error = detection_settings::error(cx)?;
            let detail = format!("{name}: {error}");
            return Some(
                div()
                    .id("detection-error")
                    .text_color(cx.theme().danger)
                    .child("Finding objects stopped")
                    .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
                    // Found by UI tests.
                    .test_support()
                    .into_any_element(),
            );
        };
        let (label, percent) = detection_progress_text(&name, status);
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Progress::new("detection-progress")
                        .w(px(140.))
                        .small()
                        .value(percent)
                        .accessibility_label("Object detection progress"),
                )
                .child(div().text_color(cx.theme().foreground).child(label))
                .into_any_element(),
        )
    }

    /// Whether the stacks shown are those a search matched.
    fn is_searched(&self) -> bool {
        self.showing_library() && !self.query.is_empty()
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
                    Source::Album(_) => "Loading the album…",
                    Source::Device(_) => "Reading the device…",
                })
                .into_any_element(),
            Status::Error(error) => centered().child(error.clone()).into_any_element(),
            Status::Loaded if self.stacks.is_empty() && self.searching => {
                centered().child("Searching…").into_any_element()
            }
            Status::Loaded if self.stacks.is_empty() && self.is_searched() => centered()
                .child(format!("No photos with “{}”.", self.query))
                .into_any_element(),
            Status::Loaded if self.stacks.is_empty() => match self.source {
                Source::Library => centered()
                    .child("No photos yet. Import some to get started.")
                    .child(import_button("empty-import", cx))
                    .into_any_element(),
                Source::Album(_) => centered()
                    .child("No photos in this album yet. Import some into it.")
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
                            // A click between the tiles clears the selection.
                            .on_click(cx.listener(|this, event: &ClickEvent, _, cx| {
                                let modifiers = event.modifiers();
                                if !modifiers.shift && !modifiers.platform && !this.marquee_dragged
                                {
                                    this.clear_selection();
                                    cx.notify();
                                }
                            }))
                            // Dragging, on the tiles or between them, draws
                            // a box (followed by the gallery's own handlers,
                            // so it can leave the grid).
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, event: &MouseDownEvent, _, _| {
                                    this.start_marquee(event)
                                }),
                            )
                            .overflow_y_scroll()
                            .track_scroll(&self.scroll_handle)
                            .p_2()
                            // Keep tiles clear of the scrollbar.
                            .pr(px(14.))
                            .child(masonry(state, move |visible, window, cx| {
                                update_and_render(&gallery, visible, window, cx)
                            })),
                    )
                    .children(self.render_marquee(cx))
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
) -> Vec<AnyElement> {
    gallery.update(cx, |this, cx| {
        this.update_visible(visible, window, cx);
        visible
            .iter()
            .map(|&ix| this.render_tile_with_menu(ix, cx))
            .collect()
    })
}

// Badges sit on top of photos, so they use fixed colors that read on any
// image rather than theme colors.

/// The files of the photos in `stacks` that aren't on this device.
fn files_not_here(stacks: &[Stack]) -> HashSet<PathBuf> {
    stacks
        .iter()
        .flat_map(|stack| &stack.photos)
        .flat_map(Photo::files)
        .filter(|file| !file.is_file())
        .cloned()
        .collect()
}

fn badge_lucide(icon: LucideIcon) -> impl IntoElement {
    svg().path(icon.path()).size_3p5().text_color(rgb(0xffffff))
}

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
/// How often the status bar's redrawn while objects are looked for.
const DETECTION_TICK: Duration = Duration::from_millis(500);

/// The status bar's text for looking for objects with `name`, e.g.
/// "Finding objects with YOLO11 · 120 of 3,400", and how far along it is.
fn detection_progress_text(name: &str, status: DetectionStatus) -> (String, f32) {
    let percent = |done: f64, total: f64| (done / total.max(1.) * 100.).min(100.) as f32;
    match status {
        DetectionStatus::Downloading { done, total } => {
            let percent = percent(done as f64, total as f64);
            let label = format!("Downloading {name} · {}%", percent.floor());
            (label, percent)
        }
        DetectionStatus::Looking { done, total } => (
            format!(
                "Finding objects with {name} · {} of {}",
                library_settings::format_count(done),
                library_settings::format_count(total)
            ),
            percent(done as f64, total as f64),
        ),
    }
}

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

/// How adding `count` photos to the album `title` went, when `added` of
/// them weren't in it already.
fn added_to_album_text(title: &str, count: usize, added: usize) -> String {
    match (count, added) {
        (1, 0) => format!("It's already in “{title}”."),
        (_, 0) => format!("They're already in “{title}”."),
        (1, _) => format!("Added the photo to “{title}”."),
        (count, added) if added == count => format!("Added {count} photos to “{title}”."),
        (count, added) => {
            format!("Added {added} of {count} photos to “{title}”; the rest were already in it.")
        }
    }
}

fn removed_from_album_text(title: &str, removed: usize) -> String {
    match removed {
        1 => format!("Took the photo out of “{title}”."),
        count => format!("Took {count} photos out of “{title}”."),
    }
}

/// Calls `f` on the gallery with the album the sidebar acted on.
fn album_callback(
    gallery: &gpui_kit::WeakEntity<Gallery>,
    f: fn(&mut Gallery, AlbumId, &mut Window, &mut Context<Gallery>),
) -> navigation::AlbumCallback {
    let gallery = gallery.clone();
    Rc::new(move |album, window, cx| {
        gallery
            .update(cx, |gallery, cx| f(gallery, album, window, cx))
            .ok();
    })
}

pub(crate) fn show_error(message: &str, error: &anyhow::Error, window: &mut Window, cx: &mut App) {
    show_warning(message, &format!("{error:#}"), window, cx);
}

pub(crate) fn show_warning(message: &str, detail: &str, window: &mut Window, cx: &mut App) {
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

/// What to ask about an album in conflict: deleted here, say, but changed
/// on another device since.
fn album_conflict_text(conflict: &AlbumConflict) -> (String, String) {
    let message = format!(
        "“{}” was deleted on {}, but changed on {} since.",
        conflict.name, conflict.deleted_on, conflict.changed_on
    );
    let added = match conflict.photos_added {
        0 => String::new(),
        1 => "1 photo was added to it. ".into(),
        n => format!("{n} photos were added to it. "),
    };
    let detail =
        format!("{added}Keep the album, or delete it? Its photos stay in the library either way.");
    (message, detail)
}

impl Render for Gallery {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.ask_about_album_conflict(window, cx);
        div()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::prompt_for_import))
            .on_action(cx.listener(Self::prompt_for_new_library))
            .on_action(
                cx.listener(|this, _: &NewAlbum, window, cx| this.new_album(None, window, cx)),
            )
            .on_action(cx.listener(|this, _: &SelectLeft, _, cx| {
                this.move_selection(Direction::Left, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectRight, _, cx| {
                this.move_selection(Direction::Right, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectUp, _, cx| {
                this.move_selection(Direction::Up, false, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectDown, _, cx| {
                this.move_selection(Direction::Down, false, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendLeft, _, cx| {
                this.move_selection(Direction::Left, true, cx)
            }))
            .on_action(cx.listener(|this, _: &ExtendRight, _, cx| {
                this.move_selection(Direction::Right, true, cx)
            }))
            .on_action(
                cx.listener(|this, _: &ExtendUp, _, cx| {
                    this.move_selection(Direction::Up, true, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &ExtendDown, _, cx| {
                this.move_selection(Direction::Down, true, cx)
            }))
            .on_action(cx.listener(|this, _: &SelectAll, _, cx| this.select_all(cx)))
            .on_action(cx.listener(|this, _: &RemoveFromAlbum, window, cx| {
                if this.is_browsing() {
                    this.remove_from_shown_album(window, cx)
                }
            }))
            .on_action(
                cx.listener(|this, _: &OpenSelected, window, cx| this.open_selected(window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ToggleQuickPreview, _, cx| this.toggle_quick_preview(cx)),
            )
            .on_action(cx.listener(|this, _: &Dismiss, _, cx| this.dismiss(cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| {
                this.close_quick_preview(cx);
                if this.page != Page::Settings {
                    library_settings::refresh_stats(cx);
                }
                this.page = Page::Settings;
                cx.notify();
            }))
            .on_mouse_move(cx.listener(Self::drag_marquee))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_marquee(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_marquee(cx)),
            )
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
                        (Page::Settings, _) => {
                            this.child(self.render_settings_title_bar(cx)).child(
                                div()
                                    .flex_1()
                                    .min_h_0()
                                    .child(settings::settings_view(self.library_actions(cx), cx)),
                            )
                        }
                        // The viewer replaces the grid; the grid's scroll
                        // position lives in `scroll_handle`, so it survives
                        // the round trip.
                        (Page::Library, Some(viewer)) => this.child(viewer.clone()),
                        (Page::Library, None) => this
                            .relative()
                            .child(self.render_toolbar(window, cx))
                            .child(self.render_body(cx))
                            .child(self.render_status_bar(cx))
                            .children(self.render_quick_preview(cx)),
                    }),
            )
    }
}

/// How downloading what pinned photos need went.
fn pin_result_text(result: anyhow::Result<library::DownloadSummary>) -> String {
    match result {
        Err(error) => format!("Couldn't download what's kept: {error:#}"),
        Ok(summary) => match (summary.failed.last(), summary.stopped) {
            (Some((_, error)), _) => format!(
                "Downloaded {}, but {} failed: {error}",
                summary.downloaded,
                summary.failed.len()
            ),
            (None, Some(stopped)) => stopped,
            (None, None) if summary.unavailable > 0 => format!(
                "Kept, but {} files aren't on a sync target that can download them.",
                summary.unavailable
            ),
            (None, None) => "Kept on this device.".into(),
        },
    }
}

/// The thumbnails to evict, of those `loaded` (each index, when it was last
/// shown, and its size in bytes), to bring them within `budget`: those
/// shown longest ago, but none of those `visible`.
fn evictions(
    loaded: impl Iterator<Item = (usize, u64, usize)>,
    visible: &[usize],
    budget: usize,
) -> Vec<usize> {
    let mut candidates = Vec::new();
    let mut total = 0;
    for (ix, last_shown, bytes) in loaded {
        total += bytes;
        if !visible.contains(&ix) {
            candidates.push((last_shown, ix, bytes));
        }
    }
    if total <= budget {
        return Vec::new();
    }
    candidates.sort_unstable();
    candidates
        .into_iter()
        .take_while(|&(_, _, bytes)| {
            let over = total > budget;
            total -= bytes;
            over
        })
        .map(|(_, ix, _)| ix)
        .collect()
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
            // A different colour each, so none are duplicates.
            let red = (ix as u8).wrapping_mul(20);
            image::RgbImage::from_pixel(64, 64, image::Rgb([red, 90, 160]))
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
    fn loads_full_resolution_only_when_zoomed_in(cx: &mut TestAppContext) {
        let folder = std::env::temp_dir().join(format!(
            "1k-words-full-resolution-test-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&folder).unwrap();
        image::RgbImage::from_fn(3000, 2000, |x, y| {
            image::Rgb([(x / 12) as u8, (y / 8) as u8, 120])
        })
        .save(folder.join("big.jpg"))
        .unwrap();
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let shown = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
            gallery.read_with(cx, |gallery, cx| {
                gallery.viewer.as_ref()?.read(cx).shown_image()
            })
        };

        press(window, "down", cx);
        press(window, "enter", cx);
        // Scaled to fit the 1000 x 700 window.
        let (width, height, full) = shown(cx).unwrap();
        assert!(!full);
        assert!(width < 3000 && height < 2000, "{width} x {height}");
        assert!((width * 2).abs_diff(height * 3) <= 3, "{width} x {height}");

        // Zoomed in to its actual size, it's loaded at full resolution and
        // shown in tiles over the fitted image.
        let tiles = |cx: &mut TestAppContext| {
            shown(cx);
            gallery.read_with(cx, |gallery, cx| {
                gallery.viewer.as_ref()?.read(cx).full_tiles()
            })
        };
        assert_eq!(tiles(cx), None);
        press(window, "cmd-1", cx);
        shown(cx);
        let (full_width, full_height, count) = tiles(cx).unwrap();
        assert_eq!((full_width, full_height), (3000, 2000));
        assert!(count > 0);
        assert_eq!(shown(cx), Some((width, height, false)));

        // Zoomed out again, the tiles are freed.
        press(window, "cmd-0", cx);
        assert_eq!(tiles(cx).map(|(.., count)| count), Some(0));

        std::fs::remove_dir_all(folder).ok();
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
    fn moves_on_to_the_next_photo_past_the_edge_of_the_grid(cx: &mut TestAppContext) {
        let folder = photo_folder(17);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        press(window, "down", cx);
        assert_eq!(selected(&gallery, cx), Some(0));
        press(window, "left", cx);
        assert_eq!(selected(&gallery, cx), Some(0), "there's none before it");

        // Right goes across the grid, then on to the next photo, through
        // all of them.
        let mut wrapped = false;
        for _ in 0..40 {
            let from = selected(&gallery, cx).unwrap();
            let beside = neighbor(&gallery, from, Direction::Right, cx);
            wrapped |= beside.is_none();
            press(window, "right", cx);
            let expected = beside.unwrap_or((from + 1).min(16));
            assert_eq!(selected(&gallery, cx), Some(expected), "right from {from}");
            if expected == 16 {
                break;
            }
        }
        assert!(wrapped, "it reached the right edge");
        assert_eq!(selected(&gallery, cx), Some(16));

        // And left, back.
        let mut wrapped = false;
        while selected(&gallery, cx) != Some(0) {
            let from = selected(&gallery, cx).unwrap();
            let beside = neighbor(&gallery, from, Direction::Left, cx);
            wrapped |= beside.is_none();
            press(window, "left", cx);
            let expected = beside.unwrap_or(from - 1);
            assert_eq!(selected(&gallery, cx), Some(expected), "left from {from}");
        }
        assert!(wrapped, "it reached the left edge");
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn frees_the_quick_previews_images_from_the_atlas(cx: &mut TestAppContext) {
        let folder = photo_folder(18);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let in_atlas = |image: &Arc<RenderImage>, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, _| window.has_image_atlas_entry(image))
                .unwrap()
        };
        let shown = |cx: &mut TestAppContext| {
            cx.executor().advance_clock(Duration::from_millis(500));
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            let image = gallery
                .read_with(cx, |gallery, cx| {
                    gallery.quick_preview.as_ref()?.read(cx).image()
                })
                .expect("the preview's image");
            assert!(in_atlas(&image, cx));
            image
        };

        press(window, "down", cx);
        press(window, "space", cx);
        let first = shown(cx);
        // Moving on, as in a key handler, which updates the window.
        press(window, "right", cx);
        let second = shown(cx);
        assert!(!in_atlas(&first, cx), "the first photo's image was freed");
        press(window, "space", cx);
        assert!(!in_atlas(&second, cx), "closing it freed its image");
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn keeps_the_librarys_thumbnails(cx: &mut TestAppContext) {
        let folder = photo_folder(5);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let library = cx.update(|cx| library::current(cx)).unwrap();
        let root = library
            .info
            .path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let photos = library.photos().unwrap();
        assert_eq!(photos.len(), 5);
        for photo in &photos {
            let thumbnail = photo.thumbnail.as_ref().unwrap();
            assert!(thumbnail.starts_with(library.info.path.join("Thumbnails")));
            assert!(thumbnail.is_file(), "{} is missing", thumbnail.display());
        }

        // Next time they're shown as kept, without decoding the photos.
        let day = photos[0].path.parent().unwrap();
        library::test_support::remove(day);
        assert!(!photos[0].path.exists());
        gallery.update(cx, |gallery, cx| gallery.load(Source::Library, cx));
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
        gallery.read_with(cx, |gallery, _| {
            assert_eq!(gallery.thumbnails.len(), 5);
            for thumbnail in gallery.thumbnails.values() {
                assert_eq!(thumbnail.state, ThumbnailState::Done);
                assert!(thumbnail.image.is_some());
            }
        });

        drop(library);
        library::test_support::remove(&root);
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn moves_and_deletes_the_library_from_settings(cx: &mut TestAppContext) {
        let folder = photo_folder(3);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let library = cx.update(|cx| library::current(cx)).unwrap();
        let (id, old_path) = (library.info.id.clone(), library.info.path.clone());
        let root = old_path.parent().unwrap().parent().unwrap().to_path_buf();
        drop(library);

        // Opening Settings counts the library's photos.
        press(window, "cmd-,", cx);
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        gallery.read_with(cx, |gallery, _| assert_eq!(gallery.page, Page::Settings));
        assert_eq!(
            cx.update(|cx| library_settings::photo_count(&id, cx)),
            Some(3)
        );

        // Moving asks where to, then shows the library from there.
        let dest = root.join("Elsewhere");
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.move_library(&id, window, cx));
        })
        .unwrap();
        cx.simulate_new_path_selection(|_| Some(dest.clone()));
        cx.run_until_parked();
        let library = cx.update(|cx| library::current(cx)).unwrap();
        assert_eq!(library.info.path, dest);
        assert!(!old_path.exists());
        drop(library);
        gallery.read_with(cx, |gallery, _| {
            assert!(matches!(gallery.status, Status::Loaded));
            assert_eq!(gallery.stacks.len(), 3);
            assert!(gallery.stacks[0].cover().path.starts_with(&dest));
        });

        // Deleting asks first; cancelling keeps it.
        let delete = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                gallery.update(cx, |gallery, cx| gallery.delete_library(&id, window, cx));
            })
            .unwrap();
        };
        delete(cx);
        assert_eq!(
            cx.pending_prompt().map(|(title, _)| title),
            Some("Delete “Elsewhere”?".into())
        );
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(dest.exists());
        delete(cx);
        cx.simulate_prompt_answer("Move to Trash");
        cx.run_until_parked();
        assert!(!dest.exists());
        assert!(cx.update(|cx| library::list(cx).is_empty()));
        assert!(cx.update(|cx| library::current(cx)).is_none());
        gallery.read_with(cx, |gallery, _| {
            assert!(matches!(gallery.status, Status::Error(_)));
        });
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();

        library::test_support::remove(&root);
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

        // Import All copies them into the library (once an album's chosen,
        // as `imports_into_albums` tests). Importing again while the first
        // is under way queues it, and skips what's imported.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| {
                let job = || ImportJob::Device(device.path.clone());
                gallery.queue_import(job(), None, window, cx);
                gallery.queue_import(job(), None, window, cx);
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

    #[gpui_kit::test]
    fn searches_for_objects_and_draws_their_boxes(cx: &mut TestAppContext) {
        let folder = photo_folder(6);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let dog = crate::detect::DetectedObject {
            label: "dog".into(),
            confidence: 0.9,
            bounds: Some([0.1, 0.2, 0.6, 0.9]),
        };
        let dog_photo = gallery.read_with(cx, |gallery, cx| {
            let photo = gallery.stacks[2].cover().clone();
            let library = library::current(cx).unwrap();
            library.set_objects(&photo, &[dog]).unwrap();
            photo.path
        });
        let shown = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                gallery
                    .stacks
                    .iter()
                    .map(|stack| stack.cover().path.clone())
                    .collect::<Vec<_>>()
            })
        };
        let wait = |cx: &mut TestAppContext| {
            cx.executor().advance_clock(SEARCH_DELAY);
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
        };

        // Typing a space searches; it doesn't open the quick preview.
        cx.update_window(window, |_, window, cx| {
            let search = gallery.read(cx).search.clone();
            search.focus_handle(cx).focus(window, cx);
            window.input("dog", cx);
        })
        .unwrap();
        press(window, "space", cx);
        wait(cx);
        gallery.read_with(cx, |gallery, cx| {
            assert_eq!(gallery.search.read(cx).value().as_ref(), "dog ");
            assert!(gallery.quick_preview.is_none());
        });
        assert_eq!(shown(cx), [dog_photo.clone()]);

        // Found by the plural too; nothing for what isn't there.
        cx.update_window(window, |_, window, cx| {
            window.press("backspace", cx);
            window.input("s", cx);
        })
        .unwrap();
        wait(cx);
        assert_eq!(shown(cx), [dog_photo.clone()]);
        cx.update_window(window, |_, window, cx| {
            let search = gallery.read(cx).search.clone();
            search.update(cx, |search, cx| search.set_value("", window, cx));
            window.input("cat", cx);
        })
        .unwrap();
        wait(cx);
        assert!(shown(cx).is_empty());

        // Clearing the search shows everything again.
        cx.update_window(window, |_, window, cx| {
            let search = gallery.read(cx).search.clone();
            search.update(cx, |search, cx| search.set_value("c", window, cx));
            window.press("backspace", cx);
        })
        .unwrap();
        wait(cx);
        assert_eq!(shown(cx).len(), 6);

        // In the viewer, the info sidebar's button draws the boxes.
        let ix = shown(cx)
            .iter()
            .position(|path| *path == dog_photo)
            .unwrap();
        gallery.update(cx, |gallery, cx| {
            gallery.selected = Some(ix);
            cx.notify();
        });
        cx.update_window(window, |_, window, cx| {
            let focus_handle = gallery.read(cx).focus_handle.clone();
            window.focus(&focus_handle, cx);
        })
        .unwrap();
        press(window, "enter", cx);
        wait(cx);
        // Drawn without a hitbox, so as not to get in the way of panning,
        // the boxes can't be found in the window; the viewer says.
        let has_boxes = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, cx| {
                let viewer = gallery.viewer.as_ref().unwrap().read(cx);
                viewer
                    .object_boxes(cx)
                    .is_some_and(|boxes| boxes.len() == 1)
            })
        };
        assert!(!has_boxes(cx));
        cx.update_window(window, |_, window, cx| window.click("object-boxes", cx))
            .unwrap();
        wait(cx);
        assert!(cx.update(|cx| settings::get(cx).show_object_boxes));
        assert!(has_boxes(cx));
        // The shimmer moves on.
        cx.executor().advance_clock(Duration::from_millis(500));
        wait(cx);
        assert!(has_boxes(cx));

        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn suggests_kinds_of_object(cx: &mut TestAppContext) {
        let folder = photo_folder(7);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let object = |label: &str| crate::detect::DetectedObject {
            label: label.into(),
            confidence: 0.9,
            bounds: Some([0., 0., 1., 1.]),
        };
        // Dogs in two photos, a donut in one, a hot dog in three.
        let paths = gallery.read_with(cx, |gallery, cx| {
            let library = library::current(cx).unwrap();
            let found = [
                vec![object("dog")],
                vec![object("dog"), object("hot dog")],
                vec![object("donut"), object("hot dog")],
                vec![object("hot dog")],
            ];
            found
                .iter()
                .zip(gallery.stacks.iter())
                .map(|(objects, stack)| {
                    library.set_objects(stack.cover(), objects).unwrap();
                    stack.cover().path.clone()
                })
                .collect::<Vec<_>>()
        });
        let suggested = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                gallery
                    .read(cx)
                    .visible_suggestions(window, cx)
                    .iter()
                    .map(|kind| (kind.label.clone(), kind.photos))
                    .collect::<Vec<_>>()
            })
            .unwrap()
        };
        let shown = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                let mut shown: Vec<_> = gallery
                    .stacks
                    .iter()
                    .map(|stack| stack.cover().path.clone())
                    .collect();
                shown.sort();
                shown
            })
        };
        let wait = |cx: &mut TestAppContext| {
            cx.executor().advance_clock(SEARCH_DELAY);
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let type_in_search = |text: &str, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                let search = gallery.read(cx).search.clone();
                search.update(cx, |search, cx| search.set_value("", window, cx));
                search.focus_handle(cx).focus(window, cx);
                window.input(text, cx);
            })
            .unwrap();
            wait(cx);
        };

        // Names that start with what's typed first, then the most common.
        type_in_search("do", cx);
        let expected = [
            ("dog".into(), 2),
            ("donut".into(), 1),
            ("hot dog".into(), 3),
        ];
        assert_eq!(suggested(cx), expected);

        // Down and Enter choose one, searching for just that kind, and go
        // on to the results.
        press(window, "down", cx);
        press(window, "down", cx);
        assert_eq!(
            gallery.read_with(cx, |gallery, _| gallery.suggestion),
            Some(1)
        );
        press(window, "enter", cx);
        wait(cx);
        gallery.read_with(cx, |gallery, cx| {
            assert_eq!(gallery.search.read(cx).value().as_ref(), "donut");
        });
        assert_eq!(shown(cx), [paths[2].clone()]);
        assert!(suggested(cx).is_empty());
        cx.update_window(window, |_, window, cx| {
            assert!(gallery.read(cx).focus_handle.is_focused(window));
        })
        .unwrap();

        // Clicking one chooses it too. "dog" is a kind of its own, not
        // "hot dog".
        type_in_search("d", cx);
        assert_eq!(suggested(cx).len(), 3);
        cx.update_window(window, |_, window, cx| {
            window.click(("object-suggestion", 0usize), cx)
        })
        .unwrap();
        wait(cx);
        let mut dogs = vec![paths[0].clone(), paths[1].clone()];
        dogs.sort();
        assert_eq!(shown(cx), dogs);

        // Escape hides them; typing brings them back. Up from none goes to
        // the last, and wraps around.
        type_in_search("hot", cx);
        assert_eq!(suggested(cx), [("hot dog".into(), 3)]);
        press(window, "escape", cx);
        assert!(suggested(cx).is_empty());
        cx.update_window(window, |_, window, cx| window.input(" ", cx))
            .unwrap();
        wait(cx);
        assert_eq!(suggested(cx).len(), 1);
        type_in_search("do", cx);
        press(window, "up", cx);
        assert_eq!(
            gallery.read_with(cx, |gallery, _| gallery.suggestion),
            Some(2)
        );
        press(window, "down", cx);
        assert_eq!(
            gallery.read_with(cx, |gallery, _| gallery.suggestion),
            Some(0)
        );

        // Enter with none highlighted searches for what's typed.
        type_in_search("hot dog", cx);
        assert!(suggested(cx).is_empty());
        press(window, "enter", cx);
        wait(cx);
        let mut hot_dogs = vec![paths[1].clone(), paths[2].clone(), paths[3].clone()];
        hot_dogs.sort();
        assert_eq!(shown(cx), hot_dogs);

        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn imports_into_albums(cx: &mut TestAppContext) {
        let folder = photo_folder(8);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let frame = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let albums = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                gallery
                    .albums
                    .iter()
                    .map(|album| (album.name.clone(), album.parent, album.photos))
                    .collect::<Vec<_>>()
            })
        };
        let shown_album = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| match gallery.source {
                Source::Album(album) => album,
                _ => panic!("not showing an album"),
            })
        };
        let in_window = |cx: &mut TestAppContext, f: &dyn Fn(&mut Window, &mut App)| {
            cx.update_window(window, |_, window, cx| f(window, cx))
                .unwrap();
            frame(cx);
        };

        let with_gallery =
            |cx: &mut TestAppContext,
             f: &dyn Fn(&mut Gallery, &mut Window, &mut Context<Gallery>)| {
                cx.update_window(window, |_, window, cx| {
                    gallery.update(cx, |gallery, cx| f(gallery, window, cx))
                })
                .unwrap();
                frame(cx);
            };

        // New Album asks for a name, makes the album and shows it.
        in_window(cx, &|window, cx| {
            window.dispatch_action(Box::new(NewAlbum), cx)
        });
        in_window(cx, &|window, cx| window.input("Trips", cx));
        in_window(cx, &|window, cx| window.click("confirm", cx));
        let trips = shown_album(cx);
        assert_eq!(albums(cx), [("Trips".into(), None, 0)]);
        gallery.read_with(cx, |gallery, _| assert!(gallery.stacks.is_empty()));

        // One inside it, from the sidebar's menu; Enter makes it too.
        with_gallery(cx, &|gallery, window, cx| {
            gallery.new_album(Some(trips), window, cx)
        });
        in_window(cx, &|window, cx| window.input("Japan", cx));
        in_window(cx, &|window, cx| window.press("enter", cx));
        let japan = shown_album(cx);
        assert_eq!(
            albums(cx),
            [("Japan".into(), Some(trips), 0), ("Trips".into(), None, 0)]
        );
        gallery.read_with(cx, |gallery, cx| {
            assert_eq!(gallery.source_title(cx).1.as_ref(), "Trips › Japan");
        });

        // Importing while it's shown adds to it by default: the photos
        // already in the library too.
        with_gallery(cx, &|gallery, window, cx| {
            gallery.choose_import_album(ImportJob::Paths(vec![folder.clone()]), window, cx)
        });
        in_window(cx, &|window, cx| window.click("start-import", cx));
        assert_eq!(albums(cx)[0], ("Japan".into(), Some(trips), 8));
        gallery.read_with(cx, |gallery, _| {
            assert_eq!(gallery.source, Source::Album(japan));
            assert_eq!(gallery.stacks.len(), 8);
        });

        // Or into a new album, made for the import.
        with_gallery(cx, &|gallery, window, cx| {
            let job = ImportJob::Paths(vec![folder.join("photo-00.jpg")]);
            let album = ImportAlbum::New {
                name: "Best".into(),
                parent: Some(trips),
            };
            gallery.import_into(job, Some(album), window, cx)
        });
        assert_eq!(albums(cx)[0], ("Best".into(), Some(trips), 1));

        // Renaming.
        with_gallery(cx, &|gallery, window, cx| {
            gallery.rename_album(trips, window, cx)
        });
        in_window(cx, &|window, cx| window.press("cmd-a", cx));
        in_window(cx, &|window, cx| window.input("Travel", cx));
        in_window(cx, &|window, cx| window.click("confirm", cx));
        assert!(albums(cx).contains(&("Travel".into(), None, 0)));

        // Deleting it deletes the albums in it, and goes back to the
        // library, whose photos stay.
        with_gallery(cx, &|gallery, window, cx| {
            gallery.delete_album(trips, window, cx)
        });
        cx.simulate_prompt_answer("Delete");
        frame(cx);
        assert!(albums(cx).is_empty());
        gallery.read_with(cx, |gallery, _| {
            assert_eq!(gallery.source, Source::Library);
            assert_eq!(gallery.stacks.len(), 8);
        });

        // Another device, not having seen that, added to it. Asked, it's
        // kept, with the albums in it, and their photos.
        let library = cx.update(|cx| library::current(cx)).unwrap();
        library.change_album_elsewhere(trips, "Studio");
        gallery.update(cx, |gallery, cx| gallery.load_albums(cx));
        frame(cx);
        assert!(cx.has_pending_prompt());
        cx.simulate_prompt_answer("Keep Album");
        frame(cx);
        let mut kept = albums(cx);
        kept.sort();
        assert_eq!(
            kept,
            [
                ("Best".into(), Some(trips), 1),
                ("Japan".into(), Some(trips), 8),
                ("Travel".into(), None, 0),
            ]
        );
        assert!(!cx.has_pending_prompt());

        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn adds_photos_to_albums(cx: &mut TestAppContext) {
        let folder = photo_folder(10);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let frame = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let library = cx.update(|cx| library::current(cx)).unwrap();
        let trips = library.create_album("Trips", None).unwrap();
        let japan = library.create_album("Japan", Some(trips)).unwrap();
        gallery.update(cx, |gallery, cx| gallery.load_albums(cx));
        frame(cx);
        let notice = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| gallery.notice.clone().unwrap_or_default())
        };
        // Right-click a tile, then pick from its Add to Album menu: the
        // albums, by their place among the others, then New Album….
        let add_to_album = |tile: usize, item: usize, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.right_click(("tile", tile), cx)
            })
            .unwrap();
            frame(cx);
            cx.update_window(window, |_, window, cx| {
                window
                    .within(("tile", tile))
                    .within("popup-menu")
                    .click(0usize, cx);
            })
            .unwrap();
            frame(cx);
            cx.update_window(window, |_, window, cx| {
                window
                    .within(("tile", tile))
                    .within("submenu")
                    .click(item, cx);
            })
            .unwrap();
            frame(cx);
        };

        add_to_album(1, 1, cx);
        assert_eq!(notice(cx), "Added the photo to “Trips › Japan”.");
        let photo = gallery.read_with(cx, |gallery, _| gallery.stacks[1].cover().path.clone());
        let in_japan = |library: &Library| {
            library
                .album_photos(japan)
                .unwrap()
                .into_iter()
                .map(|photo| photo.path)
                .collect::<Vec<_>>()
        };
        assert_eq!(in_japan(&library), [photo.clone()]);
        // The sidebar's counts follow.
        gallery.read_with(cx, |gallery, _| {
            let japan = gallery.albums.iter().find(|a| a.id == japan).unwrap();
            assert_eq!(japan.photos, 1);
        });
        add_to_album(1, 1, cx);
        assert_eq!(notice(cx), "It's already in “Trips › Japan”.");
        assert_eq!(in_japan(&library), [photo]);

        // New Album… asks for a name, and adds the photo to the new album,
        // staying where it is.
        add_to_album(2, 3, cx);
        cx.update_window(window, |_, window, cx| window.input("Favourites", cx))
            .unwrap();
        frame(cx);
        cx.update_window(window, |_, window, cx| window.click("confirm", cx))
            .unwrap();
        frame(cx);
        assert_eq!(notice(cx), "Added the photo to “Favourites”.");
        let favourites = library
            .albums()
            .unwrap()
            .into_iter()
            .find(|album| album.name == "Favourites")
            .unwrap();
        assert_eq!((favourites.parent, favourites.photos), (None, 1));
        gallery.read_with(cx, |gallery, _| assert_eq!(gallery.source, Source::Library));

        std::fs::remove_dir_all(folder).ok();
    }

    #[test]
    fn describes_adding_to_albums() {
        let text = |count, added| added_to_album_text("Japan", count, added);
        assert_eq!(text(1, 1), "Added the photo to “Japan”.");
        assert_eq!(text(3, 3), "Added 3 photos to “Japan”.");
        assert_eq!(
            text(3, 2),
            "Added 2 of 3 photos to “Japan”; the rest were already in it."
        );
        assert_eq!(text(1, 0), "It's already in “Japan”.");
        assert_eq!(text(3, 0), "They're already in “Japan”.");
    }

    /// Clicks at `position` with `modifiers` held, `count` times.
    fn click_at(
        window: AnyWindowHandle,
        position: gpui_kit::Point<gpui_kit::Pixels>,
        modifiers: gpui_kit::Modifiers,
        count: usize,
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{InputEvent as _, MouseDownEvent, MouseUpEvent};
        cx.update_window(window, |_, window, cx| {
            for click_count in 1..=count {
                let down = MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count,
                    first_mouse: false,
                };
                window.dispatch_event(down.to_platform_input(), cx);
                let up = MouseUpEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers,
                    click_count,
                };
                window.dispatch_event(up.to_platform_input(), cx);
            }
        })
        .unwrap();
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn selects_several_photos_and_opens_on_double_click(cx: &mut TestAppContext) {
        let folder = photo_folder(11);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let tile = |ix: usize, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.find(("tile", ix)).bounds()
            })
            .unwrap()
        };
        let click = |ix: usize, modifiers: gpui_kit::Modifiers, count, cx: &mut TestAppContext| {
            let center = tile(ix, cx).center();
            click_at(window, center, modifiers, count, cx);
        };
        let selection = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                (
                    gallery.selection.iter().copied().collect::<Vec<_>>(),
                    gallery.selected,
                    gallery.viewer.is_some(),
                )
            })
        };
        let none = gpui_kit::Modifiers::default();
        let command = gpui_kit::Modifiers::command();
        let shift = gpui_kit::Modifiers::shift();

        // Three columns: tiles 0 to 5 are fully in view.
        // One click selects just that photo; it doesn't open it.
        click(1, none, 1, cx);
        assert_eq!(selection(cx), (vec![1], Some(1), false));
        click(4, none, 1, cx);
        assert_eq!(selection(cx), (vec![4], Some(4), false));

        // ⌘ adds and removes; Shift selects the range from the last click.
        click(2, command, 1, cx);
        assert_eq!(selection(cx), (vec![2, 4], Some(2), false));
        click(4, command, 1, cx);
        assert_eq!(selection(cx), (vec![2], Some(4), false));
        click(5, shift, 1, cx);
        assert_eq!(selection(cx), (vec![4, 5], Some(5), false));
        // The range runs either way from where it started, in place of what
        // was selected; ⌘⇧ adds it.
        click(0, shift, 1, cx);
        assert_eq!(selection(cx), (vec![0, 1, 2, 3, 4], Some(0), false));
        click(3, none, 1, cx);
        click(5, shift, 1, cx);
        assert_eq!(selection(cx).0, [3, 4, 5]);
        let command_shift = gpui_kit::Modifiers {
            platform: true,
            shift: true,
            ..Default::default()
        };
        click(0, command, 1, cx);
        click(1, command_shift, 1, cx);
        assert_eq!(selection(cx).0, [0, 1, 3, 4, 5]);

        // Shift with the arrows extends it too; ⌘A selects everything, and
        // Escape nothing.
        click(1, none, 1, cx);
        press(window, "shift-right", cx);
        assert_eq!(selection(cx).0, [1, 2]);
        press(window, "cmd-a", cx);
        assert_eq!(selection(cx).0.len(), 11);
        assert_eq!(
            gallery.read_with(cx, |gallery, _| gallery.selected),
            Some(2)
        );
        press(window, "escape", cx);
        assert_eq!(selection(cx), (vec![], None, false));

        // A click between the tiles clears the selection.
        click(3, none, 1, cx);
        let first = tile(0, cx);
        let gap = gpui_kit::point(first.right() + gpui_kit::px(2.), first.center().y);
        click_at(window, gap, none, 1, cx);
        assert_eq!(selection(cx), (vec![], None, false));

        // Right-clicking acts on the selection, if the tile's in it.
        click(2, none, 1, cx);
        click(3, command, 1, cx);
        cx.update_window(window, |_, window, cx| {
            window.right_click(("tile", 3usize), cx)
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(selection(cx).0, [2, 3]);
        assert_eq!(
            gallery.read_with(cx, |gallery, _| gallery.selected_photos().len()),
            2
        );
        press(window, "escape", cx);
        cx.update_window(window, |_, window, cx| {
            window.right_click(("tile", 4usize), cx)
        })
        .unwrap();
        cx.run_until_parked();
        assert_eq!(selection(cx).0, [4]);
        let photos = gallery.read_with(cx, |gallery, _| gallery.selected_photos().len());
        assert_eq!(photos, 1);

        // A double-click opens the photo.
        press(window, "escape", cx);
        click(5, none, 2, cx);
        assert_eq!(selection(cx), (vec![5], Some(5), true));

        std::fs::remove_dir_all(folder).ok();
    }

    /// Drags from `from` to `to` with `modifiers` held, in steps.
    fn drag_between(
        window: AnyWindowHandle,
        from: gpui_kit::Point<gpui_kit::Pixels>,
        to: gpui_kit::Point<gpui_kit::Pixels>,
        modifiers: gpui_kit::Modifiers,
        cx: &mut TestAppContext,
    ) {
        use gpui_kit::{InputEvent as _, MouseUpEvent};
        cx.update_window(window, |_, window, cx| {
            let down = MouseDownEvent {
                button: MouseButton::Left,
                position: from,
                modifiers,
                click_count: 1,
                first_mouse: false,
            };
            window.dispatch_event(down.to_platform_input(), cx);
            for step in 1..=4 {
                let position = from + (to - from) * (step as f32 / 4.);
                let moved = MouseMoveEvent {
                    position,
                    pressed_button: Some(MouseButton::Left),
                    modifiers,
                };
                window.dispatch_event(moved.to_platform_input(), cx);
                window.render_frame(cx);
            }
            let up = MouseUpEvent {
                button: MouseButton::Left,
                position: to,
                modifiers,
                click_count: 1,
            };
            window.dispatch_event(up.to_platform_input(), cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    #[gpui_kit::test]
    fn selects_with_a_box(cx: &mut TestAppContext) {
        let folder = photo_folder(13);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        // Three columns, as in `selects_several_photos_and_opens_on_double_click`.
        let tile = |ix: usize, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.find(("tile", ix)).bounds()
            })
            .unwrap()
        };
        let selection = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| {
                gallery.selection.iter().copied().collect::<Vec<_>>()
            })
        };
        let none = gpui_kit::Modifiers::default();

        // From inside one tile to inside another: everything it touches,
        // and the click it ends with neither selects nor opens anything.
        let from = tile(0, cx).center();
        let to = tile(4, cx).center();
        drag_between(window, from, to, none, cx);
        assert_eq!(selection(cx), [0, 1, 3, 4]);
        gallery.read_with(cx, |gallery, _| {
            assert!(gallery.viewer.is_none());
            assert!(gallery.marquee.is_none());
        });

        // Backwards, from the gap beside a tile, it replaces the selection.
        let start = tile(1, cx);
        let from = gpui_kit::point(
            start.right() + gpui_kit::px(3.),
            start.top() + gpui_kit::px(10.),
        );
        let to = tile(0, cx).center();
        drag_between(window, from, to, none, cx);
        assert_eq!(selection(cx), [0, 1]);

        // With ⌘, it adds to it.
        let from = tile(3, cx).center();
        let to = from + gpui_kit::point(gpui_kit::px(20.), gpui_kit::px(20.));
        drag_between(window, from, to, gpui_kit::Modifiers::command(), cx);
        assert_eq!(selection(cx), [0, 1, 3]);

        // From a selected photo it would drag the selection, but with
        // Shift it's a box too.
        let from = tile(3, cx).center();
        let to = tile(4, cx).center();
        drag_between(window, from, to, gpui_kit::Modifiers::shift(), cx);
        assert_eq!(selection(cx), [0, 1, 3, 4]);
        assert!(!cx.update(|cx| cx.has_active_drag()));

        // A press that hardly moves is still a click.
        let center = tile(5, cx).center();
        drag_between(
            window,
            center,
            center + gpui_kit::point(gpui_kit::px(1.), gpui_kit::px(1.)),
            none,
            cx,
        );
        assert_eq!(selection(cx), [5]);

        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn fetches_evicted_photos_when_opened_or_pinned(cx: &mut TestAppContext) {
        use crate::library::sync_test_support::FakeS3;
        use crate::library::{DownloadSource, TargetSettings};
        use crate::sync_targets;

        let folder = photo_folder(19);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let frame = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let library = cx.update(|cx| library::current(cx)).unwrap();

        // Everything's uploaded, then the originals are evicted.
        let s3 = FakeS3::start();
        let root = std::env::temp_dir().join(format!("1k-words-kw27-{}", std::process::id()));
        cx.update(|cx| sync_targets::test_support::init(root.clone(), cx));
        let plugins = cx.update(|cx| sync_targets::plugins(cx)).unwrap().unwrap();
        let config = sync_targets::validate(&plugins, "s3", s3.config()).unwrap();
        let target = cx
            .update(|cx| {
                sync_targets::save(
                    None,
                    sync_targets::NewTarget {
                        name: "Bucket".into(),
                        plugin: "s3".into(),
                        summary: String::new(),
                        config: config.clone(),
                    },
                    cx,
                )
            })
            .unwrap();
        let source = DownloadSource {
            id: library.add_sync_target(&target).unwrap(),
            settings: TargetSettings {
                plugin: "s3".into(),
                config,
            },
        };
        let summary = library
            .upload(
                &plugins,
                source.id,
                &source.settings,
                &AtomicBool::new(false),
                &Default::default(),
                |_| {},
            )
            .unwrap();
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        let photos = library.photos().unwrap();
        for photo in &photos {
            let path = photo.path.strip_prefix(&library.info.path).unwrap();
            library.evict_original(path.to_str().unwrap()).unwrap();
        }

        // Opening one fetches it, and only it, then shows it.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| {
                gallery.open_viewer(0, StartFrame::Cover, window, cx)
            })
        })
        .unwrap();
        frame(cx);
        let shown = gallery.read_with(cx, |gallery, cx| {
            let viewer = gallery.viewer.as_ref().unwrap().read(cx);
            viewer.load_result()
        });
        assert_eq!(shown, Ok(true));
        assert_eq!(photos.iter().filter(|p| p.path.is_file()).count(), 1);
        assert_eq!(library.fetched_originals().unwrap().len(), 1);
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.close_viewer(window, cx))
        })
        .unwrap();
        frame(cx);

        // Pinning from the menu downloads what the selection needs.
        cx.update_window(window, |_, window, cx| {
            window.right_click(("tile", 1usize), cx)
        })
        .unwrap();
        frame(cx);
        cx.update_window(window, |_, window, cx| {
            window
                .within(("tile", 1usize))
                .within("popup-menu")
                .click(1usize, cx);
        })
        .unwrap();
        frame(cx);
        assert_eq!(photos.iter().filter(|p| p.path.is_file()).count(), 2);
        assert_eq!(library.pinned_originals().unwrap().len(), 1);
        assert_eq!(
            gallery.read_with(cx, |gallery, _| gallery.notice.clone()),
            Some("Kept on this device.".into())
        );

        cx.update(|cx| sync_targets::delete(&target, cx)).unwrap();
        library::test_support::remove(&root);
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn shows_whats_not_on_this_device(cx: &mut TestAppContext) {
        use crate::library::sync_test_support::FakeS3;
        use crate::sync_settings::OriginalsChanged;
        use crate::sync_targets;

        let folder = photo_folder(20);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let frame = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        frame(cx);
        let library = cx.update(|cx| library::current(cx)).unwrap();
        library.make_missing_previews().unwrap();

        // Everything's uploaded, then the first tile's original is removed.
        let s3 = FakeS3::start();
        let root = std::env::temp_dir().join(format!("1k-words-kw46-{}", std::process::id()));
        cx.update(|cx| sync_targets::test_support::init(root.clone(), cx));
        let plugins = cx.update(|cx| sync_targets::plugins(cx)).unwrap().unwrap();
        let config = sync_targets::validate(&plugins, "s3", s3.config()).unwrap();
        let target = cx
            .update(|cx| {
                sync_targets::save(
                    None,
                    sync_targets::NewTarget {
                        name: "Bucket".into(),
                        plugin: "s3".into(),
                        summary: String::new(),
                        config: config.clone(),
                    },
                    cx,
                )
            })
            .unwrap();
        let id = library.add_sync_target(&target).unwrap();
        let settings = crate::library::TargetSettings {
            plugin: "s3".into(),
            config,
        };
        let summary = library
            .upload(
                &plugins,
                id,
                &settings,
                &AtomicBool::new(false),
                &Default::default(),
                |_| {},
            )
            .unwrap();
        assert!(summary.failed.is_empty(), "{:?}", summary.failed);
        let cover = gallery.read_with(cx, |gallery, _| gallery.stacks[0].cover().clone());
        let path = cover.path.strip_prefix(&library.info.path).unwrap();
        library.evict_original(path.to_str().unwrap()).unwrap();

        // Told originals have gone, the grid marks it, and only it.
        let library_id = library.info.id.clone();
        cx.update(|cx| cx.set_global(OriginalsChanged { library_id }));
        frame(cx);
        gallery.read_with(cx, |gallery, _| {
            assert_eq!(gallery.not_here.iter().collect::<Vec<_>>(), [&cover.path]);
            assert!(!gallery.all_pinned(&gallery.stacks[0].photos));
        });

        // It can't be downloaded (the credentials are refused), so its
        // preview is shown, and the sidebar says where it is.
        s3.fail_next(&[403; 8]);
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| {
                gallery.open_viewer(0, StartFrame::Cover, window, cx)
            })
        })
        .unwrap();
        frame(cx);
        let viewer = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, cx| {
                let viewer = gallery.viewer.as_ref().unwrap().read(cx);
                (
                    viewer.load_result(),
                    viewer.shown_image(),
                    viewer.on_device(cx),
                )
            })
        };
        let (result, shown, on_device) = viewer(cx);
        assert!(result.is_err(), "{result:?}");
        let (_, _, full) = shown.expect("its preview is shown");
        assert!(!full);
        assert_eq!(
            on_device,
            Some((
                vec!["Not on this device · backed up to 1 target".to_owned()],
                false
            ))
        );

        // Keep Here pins it, and the sidebar and the tile say so.
        cx.update_window(window, |_, window, cx| window.click("keep-here", cx))
            .unwrap();
        frame(cx);
        assert!(
            library
                .pinned_originals()
                .unwrap()
                .contains(path.to_str().unwrap())
        );
        let (_, _, on_device) = viewer(cx);
        assert_eq!(on_device.map(|(_, pinned)| pinned), Some(true));
        press(window, "escape", cx);
        frame(cx);
        gallery.read_with(cx, |gallery, _| {
            assert!(gallery.all_pinned(&gallery.stacks[0].photos));
        });

        cx.update(|cx| sync_targets::delete(&target, cx)).ok();
        library::test_support::remove(&root);
        std::fs::remove_dir_all(folder).ok();
    }

    #[gpui_kit::test]
    fn changes_albums_by_menu_drag_and_viewer(cx: &mut TestAppContext) {
        let folder = photo_folder(14);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let frame = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let library = cx.update(|cx| library::current(cx)).unwrap();
        let trips = library.create_album("Trips", None).unwrap();
        let family = library.create_album("Family", None).unwrap();
        let photos = library.photos().unwrap();
        library.add_to_album(trips, &photos[..3]).unwrap();
        gallery.update(cx, |gallery, cx| gallery.load_albums(cx));
        frame(cx);
        let in_album = |album, cx: &mut TestAppContext| {
            cx.run_until_parked();
            library.album_photos(album).unwrap().len()
        };
        let notice = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, _| gallery.notice.clone().unwrap_or_default())
        };
        let click = |id: (&'static str, usize), cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| window.click(id, cx))
                .unwrap();
            frame(cx);
        };

        // In an album, ⌘⌫ takes the selection out of it.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.show_album(trips, window, cx))
        })
        .unwrap();
        frame(cx);
        click(("tile", 0), cx);
        press(window, "cmd-backspace", cx);
        assert_eq!(in_album(trips, cx), 2);
        assert_eq!(notice(cx), "Took the photo out of “Trips”.");
        gallery.read_with(cx, |gallery, _| assert_eq!(gallery.stacks.len(), 2));

        // So does its right-click menu, after Add to Album.
        cx.update_window(window, |_, window, cx| {
            window.right_click(("tile", 1usize), cx)
        })
        .unwrap();
        frame(cx);
        cx.update_window(window, |_, window, cx| {
            window
                .within(("tile", 1usize))
                .within("popup-menu")
                .click(1usize, cx);
        })
        .unwrap();
        frame(cx);
        assert_eq!(in_album(trips, cx), 1);

        // Back in the library, the selection is dragged onto an album in
        // the sidebar.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.load(Source::Library, cx));
            window.render_frame(cx);
        })
        .unwrap();
        frame(cx);
        click(("tile", 0), cx);
        cx.update_window(window, |_, window, cx| {
            // As ⌘-clicks would.
            gallery.update(cx, |gallery, cx| {
                gallery.toggle_selected(1);
                gallery.toggle_selected(2);
                cx.notify();
            });
            window.render_frame(cx);
        })
        .unwrap();
        cx.update_window(window, |_, window, cx| {
            window.drag_to(("tile", 0usize), ("album", family.key()), cx)
        })
        .unwrap();
        frame(cx);
        assert_eq!(in_album(family, cx), 3);
        assert_eq!(notice(cx), "Added 3 photos to “Family”.");
        // A drag from the selection doesn't draw a box.
        gallery.read_with(cx, |gallery, _| {
            assert!(gallery.marquee.is_none());
            assert_eq!(gallery.selection.len(), 3);
        });

        // The viewer shows the albums a photo is in, and its menu changes
        // them.
        let photo_albums = |cx: &mut TestAppContext| {
            gallery.read_with(cx, |gallery, cx| {
                let viewer = gallery.viewer.as_ref().unwrap().read(cx);
                viewer.photo_albums().map(<[AlbumId]>::to_vec)
            })
        };
        cx.update_window(window, |_, window, cx| {
            window.double_click(("tile", 0usize), cx)
        })
        .unwrap();
        frame(cx);
        assert_eq!(photo_albums(cx), Some(vec![family]));
        cx.update_window(window, |_, window, cx| window.click("photo-albums", cx))
            .unwrap();
        frame(cx);
        // Its albums, by name, checked if the photo's in them: Family, Trips,
        // then New Album….
        cx.update_window(window, |_, window, cx| {
            window
                .within("popover:dropdown-menu:Name(\"photo-albums\")")
                .within("popup-menu")
                .click(1usize, cx);
        })
        .unwrap();
        frame(cx);
        assert_eq!(photo_albums(cx), Some(vec![trips, family]));
        assert_eq!(in_album(trips, cx), 2);
        // The sidebar's counts follow.
        gallery.read_with(cx, |gallery, _| {
            let trips = gallery.albums.iter().find(|a| a.id == trips).unwrap();
            assert_eq!(trips.photos, 2);
        });

        // Right-clicking the photo shows the same menu; choosing an album
        // it's in takes it out.
        cx.update_window(window, |_, window, cx| window.right_click("image-pane", cx))
            .unwrap();
        frame(cx);
        cx.update_window(window, |_, window, cx| {
            window
                .within("image-pane")
                .within("popup-menu")
                .click(1usize, cx);
        })
        .unwrap();
        frame(cx);
        assert_eq!(photo_albums(cx), Some(vec![trips]));
        assert_eq!(in_album(family, cx), 2);

        std::fs::remove_dir_all(folder).ok();
    }

    #[test]
    fn evicts_thumbnails_shown_longest_ago_to_fit_the_budget() {
        // (index, last shown, bytes)
        let loaded = || [(0, 5, 40), (1, 1, 30), (2, 3, 30), (3, 9, 30)].into_iter();
        // Within budget: nothing.
        assert!(evictions(loaded(), &[], 130).is_empty());
        // 130 bytes into 70: the two shown longest ago go.
        assert_eq!(evictions(loaded(), &[], 70), [1, 2]);
        // Those on screen stay, whatever the budget.
        assert_eq!(evictions(loaded(), &[1], 70), [2, 0]);
        assert_eq!(evictions(loaded(), &[0, 1, 2, 3], 0), Vec::<usize>::new());
    }

    #[gpui_kit::test]
    fn rearranges_albums_in_the_sidebar(cx: &mut TestAppContext) {
        let folder = photo_folder(15);
        let (window, gallery) = open_gallery(folder.clone(), cx);
        let frame = |cx: &mut TestAppContext| {
            cx.run_until_parked();
            cx.update_window(window, |_, window, cx| window.render_frame(cx))
                .unwrap();
            cx.run_until_parked();
        };
        let library = cx.update(|cx| library::current(cx)).unwrap();
        let trips = library.create_album("Trips", None).unwrap();
        let japan = library.create_album("Japan", Some(trips)).unwrap();
        let family = library.create_album("Family", None).unwrap();
        let photos = library.photos().unwrap();
        library.add_to_album(japan, &photos[..2]).unwrap();
        library.add_to_album(trips, &photos[2..3]).unwrap();
        // Showing Japan, so Trips is open in the sidebar.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| {
                gallery.show_album(japan, window, cx);
                gallery.load_albums(cx);
            })
        })
        .unwrap();
        frame(cx);
        let parent = |album: AlbumId, cx: &mut TestAppContext| {
            frame(cx);
            gallery.read_with(cx, |gallery, _| {
                gallery
                    .albums
                    .iter()
                    .find(|a| a.id == album)
                    .unwrap()
                    .parent
            })
        };
        let drop_target = |album: AlbumId| ("album", album.key());
        let drag = |from: AlbumId, to: AlbumId, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.drag_to(drop_target(from), drop_target(to), cx)
            })
            .unwrap();
            frame(cx);
        };
        let menu_item = |album: AlbumId, item: usize, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.right_click(drop_target(album), cx)
            })
            .unwrap();
            frame(cx);
            cx.update_window(window, |_, window, cx| {
                window.within("popup-menu").click(item, cx);
            })
            .unwrap();
            frame(cx);
        };

        // Not into an album inside it.
        drag(trips, japan, cx);
        assert_eq!(parent(trips, cx), None);
        // Into another album.
        drag(family, japan, cx);
        assert_eq!(parent(family, cx), Some(japan));

        // Photos are dropped anywhere on a row, such as its icon.
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.load(Source::Library, cx));
            window.render_frame(cx);
        })
        .unwrap();
        frame(cx);
        cx.update_window(window, |_, window, cx| window.click(("tile", 3usize), cx))
            .unwrap();
        frame(cx);
        let (from, to) = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                let tile = window.find(("tile", 3usize)).bounds();
                let row = window.find(drop_target(trips)).bounds();
                let icon = gpui_kit::point(row.left() + px(10.), row.center().y);
                (tile.center(), icon)
            })
            .unwrap();
        cx.update_window(window, |_, window, cx| window.drag(from, to, cx))
            .unwrap();
        frame(cx);
        assert_eq!(library.album_photos(trips).unwrap().len(), 2);

        // Trips shows the photos of the albums in it, from its menu: New
        // Album Inside…, Rename…, then this.
        menu_item(trips, 2, cx);
        assert!(
            library
                .albums()
                .unwrap()
                .iter()
                .any(|a| a.id == trips && a.show_nested)
        );
        cx.update_window(window, |_, window, cx| {
            gallery.update(cx, |gallery, cx| gallery.show_album(trips, window, cx))
        })
        .unwrap();
        frame(cx);
        gallery.read_with(cx, |gallery, _| assert_eq!(gallery.stacks.len(), 4));

        // Japan goes back to the top level from its menu, after Show
        // Photos from Albums Inside (it has Family in it). Trips no longer
        // shows its photos.
        menu_item(japan, 3, cx);
        assert_eq!(parent(japan, cx), None);
        gallery.read_with(cx, |gallery, _| assert_eq!(gallery.stacks.len(), 2));

        std::fs::remove_dir_all(folder).ok();
    }

    #[test]
    fn describes_looking_for_objects() {
        let looking = DetectionStatus::Looking {
            done: 1200,
            total: 4800,
        };
        assert_eq!(
            detection_progress_text("YOLO11", looking),
            ("Finding objects with YOLO11 · 1,200 of 4,800".into(), 25.)
        );
        let downloading = DetectionStatus::Downloading {
            done: 50,
            total: 200,
        };
        assert_eq!(
            detection_progress_text("YOLO11", downloading),
            ("Downloading YOLO11 · 25%".into(), 25.)
        );
    }

    #[gpui_kit::test]
    fn says_when_finding_objects_stopped(cx: &mut TestAppContext) {
        // Each test has a folder of its own, by its size.
        let folder = photo_folder(16);
        let (window, _) = open_gallery(folder.clone(), cx);
        let shown = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find("detection-error").is_some()
            })
            .unwrap()
        };
        cx.update(detection_settings::init);
        assert!(!shown(cx));
        cx.update(|cx| {
            detection_settings::set_error(Some("The API key wasn't accepted.".into()), cx)
        });
        assert!(shown(cx));
        cx.update(|cx| detection_settings::set_error(None, cx));
        assert!(!shown(cx));
        std::fs::remove_dir_all(folder).ok();
    }
}

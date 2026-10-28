//! The photo viewer: a zoomable view of one photo, a sidebar with its
//! details, and, for brackets and bursts, a filmstrip of every frame.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::{
    AnyElement, App, Context, Div, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, ObjectFit, RenderImage, ScrollDelta,
    ScrollWheelEvent, SharedString, Subscription, Task, TestSupportExt as _, Window, div, img,
    prelude::*, px, rgba, size, svg, transparent_black,
};

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::{ActiveTheme, Icon, IconName, Selectable as _, Sizable};

use crate::album_dialogs::{NameCallback, album_title, prompt_for_album_name};
use crate::assets::{BRACKET_ICON, BURST_ICON, LIVE_ICON};
use crate::decode;
use crate::details::{DetailSection, format_ev_value, objects_section, photo_details};
use crate::detect::DetectedObject;
use crate::gallery::show_error;
use crate::library::{self, Album, AlbumId, Library, tree_order};
use crate::navigation;
use crate::object_boxes::ObjectBoxes;
use crate::photos::{Stack, StackKind, StartFrame};
use crate::settings;
use crate::tiles::{FullImage, Tiles};
use crate::video::{PlaybackState, VideoPlayer};
use crate::zoom::{ZoomState, zoomed_image};

gpui_kit::actions!(
    viewer,
    [
        CloseViewer,
        ZoomIn,
        ZoomOut,
        ZoomToFit,
        ActualSize,
        PreviousFrame,
        NextFrame
    ]
);

const KEY_CONTEXT: &str = "PhotoViewer";
const SIDEBAR_WIDTH: f32 = 280.;
/// Height of the thumbnails in the floating filmstrip.
const FILMSTRIP_THUMBNAIL_HEIGHT: f32 = 56.;
const ZOOM_STEP: f32 = 1.25;

pub fn bind_keys(cx: &mut App) {
    let context = Some(KEY_CONTEXT);
    cx.bind_keys([
        KeyBinding::new("escape", CloseViewer, context),
        KeyBinding::new("cmd-=", ZoomIn, context),
        KeyBinding::new("cmd-+", ZoomIn, context),
        KeyBinding::new("cmd--", ZoomOut, context),
        KeyBinding::new("cmd-0", ZoomToFit, context),
        KeyBinding::new("cmd-1", ActualSize, context),
        KeyBinding::new("left", PreviousFrame, context),
        KeyBinding::new("right", NextFrame, context),
    ]);
}

pub enum ViewerEvent {
    Close,
    /// Stepped back past the first frame: show the previous photo.
    Previous,
    /// Stepped on past the last frame: show the next photo.
    Next,
    /// A photo was added to or taken out of an album, or an album was made.
    AlbumsChanged,
}

struct LoadedImage {
    image: Arc<RenderImage>,
    /// Whether it's the photo at full resolution, rather than a preview or
    /// the photo scaled to fit the window.
    full: bool,
}

enum LoadState {
    Loading,
    Done,
    Failed(SharedString),
}

pub struct PhotoViewer {
    focus_handle: FocusHandle,
    stack: Stack,
    selected: usize,
    /// The gallery thumbnail of the stack's cover, shown while the cover's
    /// full image loads.
    cover_thumbnail: Option<Arc<RenderImage>>,
    image: Option<LoadedImage>,
    load_state: LoadState,
    filmstrip: Vec<Option<Arc<RenderImage>>>,
    details: Vec<DetailSection>,
    /// The objects found in the selected photo, if any, and their section
    /// of the sidebar.
    objects: Option<(Arc<[DetectedObject]>, DetailSection)>,
    zoom: Rc<RefCell<ZoomState>>,
    /// The Live Photo video, while it plays over the photo.
    live: Option<(Entity<VideoPlayer>, Subscription)>,
    /// Whether the image pane has received a mouse down, so that only two
    /// clicks on the image count as a double-click on it: not, say, a third
    /// quick click after double-clicking a gallery tile to open it.
    pane_clicked: bool,
    _load_image: Task<()>,
    /// Loading the photo at full resolution, once it's zoomed in further
    /// than the image shown can show sharply.
    load_full: Option<Task<()>>,
    /// The photo at full resolution, once loaded, and the tiles of it in
    /// view (see `tiles`).
    tiles: Option<Tiles>,
    /// Cutting tiles that have come into view.
    cutting: Option<Task<()>>,
    /// Loading it failed, so it isn't tried again for this photo.
    full_failed: bool,
    _load_details: Task<()>,
    _load_filmstrip: Vec<Task<()>>,
    /// The library the photos are in, if they're a library's, for albums.
    library: Option<Arc<Library>>,
    /// The library's albums, for the albums menu.
    albums: Arc<Vec<Album>>,
    /// The albums the selected photo is in, once they're known.
    photo_albums: Option<Vec<AlbumId>>,
    _load_albums: Task<()>,
}

impl EventEmitter<ViewerEvent> for PhotoViewer {}

impl PhotoViewer {
    pub fn new(
        stack: Stack,
        cover_thumbnail: Option<Arc<RenderImage>>,
        start: StartFrame,
        library: Option<Arc<Library>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let first = stack.start_frame(start);
        let mut this = Self {
            focus_handle: cx.focus_handle(),
            zoom: Rc::new(RefCell::new(ZoomState::new(image_size(&stack, first)))),
            filmstrip: vec![None; stack.photos.len()],
            stack,
            selected: first,
            cover_thumbnail,
            image: None,
            load_state: LoadState::Loading,
            details: Vec::new(),
            objects: None,
            pane_clicked: false,
            live: None,
            _load_image: Task::ready(()),
            load_full: None,
            tiles: None,
            cutting: None,
            full_failed: false,
            _load_details: Task::ready(()),
            _load_filmstrip: Vec::new(),
            library,
            albums: Arc::default(),
            photo_albums: None,
            _load_albums: Task::ready(()),
        };
        this.load_filmstrip(cx);
        this.select(first, window, cx);
        this
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_live(cx);
        self.release_tiles(cx);
        let Some(photo) = self.stack.photos.get(ix) else {
            return;
        };
        if let Some(loaded) = self.image.take() {
            cx.drop_image(loaded.image, Some(window));
        }
        self.load_state = LoadState::Loading;
        self.load_full = None;
        self.full_failed = false;
        self.photo_albums = None;
        self.objects = None;
        self.selected = ix;
        self.zoom
            .borrow_mut()
            .set_image_size(image_size(&self.stack, ix));

        let (viewport, scale) = (window.viewport_size(), window.scale_factor());
        let fit = size(
            (f32::from(viewport.width) * scale).ceil() as u32,
            (f32::from(viewport.height) * scale).ceil() as u32,
        );
        let photo = photo.clone();
        self._load_image = cx.spawn({
            let photo = photo.clone();
            async move |this, cx| {
                // The embedded preview of a raw file takes milliseconds.
                let preview = cx.background_spawn({
                    let photo = photo.clone();
                    async move { decode::viewer_embedded_preview(&photo) }
                });
                if let Some(preview) = preview.await {
                    this.update(cx, |this, cx| this.show(preview, false, cx))
                        .ok();
                }

                // Then the photo, scaled to fit the window: all that's
                // needed until it's zoomed in (see `load_full_if_needed`).
                // Its full resolution would take several times the memory.
                let loaded = cx
                    .background_spawn(async move {
                        let decoded = decode::decode_full(&photo)?;
                        anyhow::Ok(decode::viewer_image(decoded, fit.width, fit.height))
                    })
                    .await;
                this.update(cx, |this, cx| match loaded {
                    Ok((image, full)) => {
                        this.show(image, full, cx);
                        this.load_state = LoadState::Done;
                    }
                    Err(error) => {
                        this.load_state = LoadState::Failed(error.to_string().into());
                        cx.notify();
                    }
                })
                .ok();
            }
        });

        let library = library::current(cx);
        self._load_details = cx.spawn(async move |this, cx| {
            let details = cx
                .background_spawn({
                    let photo = photo.clone();
                    async move { photo_details(&photo) }
                })
                .await;
            this.update(cx, |this, cx| {
                this.details = details;
                cx.notify();
            })
            .ok();

            // Looking for them may take a moment, so they're added after.
            let Some(library) = library else {
                return;
            };
            let objects = cx
                .background_spawn(async move {
                    let objects = library.objects(&photo).unwrap_or_else(|error| {
                        eprintln!(
                            "couldn't find objects in {}: {error:#}",
                            photo.path.display()
                        );
                        None
                    })?;
                    let section = objects_section(&objects)?;
                    Some((Arc::from(objects), section))
                })
                .await;
            if objects.is_some() {
                this.update(cx, |this, cx| {
                    this.objects = objects;
                    cx.notify();
                })
                .ok();
            }
        });
        self.load_albums(cx);
        cx.notify();
    }

    /// The albums the photo shown is in, once they're known; never for
    /// photos that aren't a library's.
    #[cfg(test)]
    pub fn photo_albums(&self) -> Option<&[AlbumId]> {
        self.photo_albums.as_deref()
    }

    /// Reads the library's albums, and which the selected photo is in.
    fn load_albums(&mut self, cx: &mut Context<Self>) {
        let Some(library) = self.library.clone() else {
            return;
        };
        let photo = self.stack.photos[self.selected].clone();
        self._load_albums = cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    anyhow::Ok((library.albums()?, library.albums_with(&photo)?))
                })
                .await;
            this.update(cx, |this, cx| {
                match loaded {
                    Ok((albums, photo_albums)) => {
                        this.albums = Arc::new(albums);
                        this.photo_albums = Some(photo_albums);
                    }
                    Err(error) => eprintln!("couldn't read the photo's albums: {error:#}"),
                }
                cx.notify();
            })
            .ok();
        });
    }

    /// Adds the photo shown to an album, or takes it out if it's in it.
    fn toggle_album(&mut self, album: AlbumId, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(library), Some(photo_albums)) = (self.library.clone(), &self.photo_albums) else {
            return;
        };
        let inside = photo_albums.contains(&album);
        let photo = self.stack.photos[self.selected].clone();
        let change = cx.background_spawn(async move {
            let photos = [photo];
            match inside {
                true => library.remove_from_album(album, &photos),
                false => library.add_to_album(album, &photos),
            }
        });
        cx.spawn_in(window, async move |this, cx| {
            let changed = change.await;
            this.update_in(cx, |this, window, cx| match changed {
                Ok(_) => this.albums_changed(cx),
                Err(error) => show_error("The album couldn't be changed.", &error, window, cx),
            })
            .ok();
        })
        .detach();
    }

    /// Asks for a name for a new album, makes it, and adds the photo shown.
    fn add_to_new_album(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(library) = self.library.clone() else {
            return;
        };
        let photo = self.stack.photos[self.selected].clone();
        let viewer = cx.weak_entity();
        let create: NameCallback = Rc::new(move |name, window, cx| {
            let (library, photo, viewer) = (library.clone(), photo.clone(), viewer.clone());
            let create = cx.background_spawn(async move {
                let album = library.create_album(&name, None)?;
                library.add_to_album(album, &[photo])
            });
            window.spawn(cx, async move |cx| {
                create.await?;
                viewer.update(cx, |viewer, cx| viewer.albums_changed(cx))?;
                Ok(())
            })
        });
        prompt_for_album_name("New Album", "Create", "", create, window, cx);
    }

    fn albums_changed(&mut self, cx: &mut Context<Self>) {
        self.load_albums(cx);
        cx.emit(ViewerEvent::AlbumsChanged);
    }

    /// Every album, checked if the photo shown is in it: choosing one adds
    /// the photo to it or takes it out. Then a new album, for the photo.
    fn albums_menu(&self, cx: &Context<Self>) -> impl Fn(PopupMenu) -> PopupMenu + 'static {
        let viewer = cx.weak_entity();
        let albums = self.albums.clone();
        let photo_albums = self.photo_albums.clone().unwrap_or_default();
        move |mut menu| {
            for (_, album) in tree_order(&albums) {
                let album = album.id;
                let viewer = viewer.clone();
                menu = menu.item(
                    PopupMenuItem::new(album_title(&albums, album))
                        .checked(photo_albums.contains(&album))
                        .on_click(move |_, window, cx| {
                            viewer
                                .update(cx, |viewer, cx| viewer.toggle_album(album, window, cx))
                                .ok();
                        }),
                );
            }
            if !albums.is_empty() {
                menu = menu.separator();
            }
            let viewer = viewer.clone();
            menu.item(
                PopupMenuItem::new("New Album…").on_click(move |_, window, cx| {
                    viewer
                        .update(cx, |viewer, cx| viewer.add_to_new_album(window, cx))
                        .ok();
                }),
            )
        }
    }

    /// The albums the photo shown is in, with a menu to change them.
    fn render_albums(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let photo_albums = self.photo_albums.as_ref()?;
        let menu = Rc::new(self.albums_menu(cx));
        let button = Button::new("photo-albums")
            .icon(Icon::new(LucideIcon::Album))
            .label("Albums")
            .xsmall()
            .ghost()
            .dropdown_menu(move |popup, _, _| menu(popup));
        let muted_foreground = cx.theme().muted_foreground;
        let mut titles: Vec<String> = photo_albums
            .iter()
            .map(|&album| album_title(&self.albums, album))
            .filter(|title| !title.is_empty())
            .collect();
        titles.sort();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_1p5()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .gap_2()
                        .child(div().text_xs().text_color(muted_foreground).child("ALBUMS"))
                        .child(button),
                )
                .when(titles.is_empty(), |section| {
                    section.child(
                        div()
                            .text_xs()
                            .text_color(muted_foreground)
                            .child("Not in any album."),
                    )
                })
                .children(titles.into_iter().map(|title| div().text_xs().child(title))),
        )
    }

    /// Plays the selected photo's Live Photo video over it, once.
    fn play_live(&mut self, cx: &mut Context<Self>) {
        let Some(video) = self.stack.photos[self.selected].live_video.clone() else {
            return;
        };
        if self.live.is_some() {
            return;
        }
        let player = cx.new(|cx| VideoPlayer::new(video, true, cx));
        let observe = cx.observe(&player, |this, player, cx| {
            if *player.read(cx).state() != PlaybackState::Playing {
                this.stop_live(cx);
            }
            cx.notify();
        });
        self.live = Some((player, observe));
        cx.notify();
    }

    fn stop_live(&mut self, cx: &mut App) {
        if let Some((player, _)) = self.live.take() {
            player.update(cx, |player, cx| player.stop(cx));
        }
    }

    fn show(&mut self, image: Arc<RenderImage>, full: bool, cx: &mut Context<Self>) {
        if let Some(old) = self.image.replace(LoadedImage { image, full }) {
            decode::release(old.image, cx);
        }
        cx.notify();
    }

    /// Whether it's zoomed in further than the image shown has pixels for.
    fn needs_full(&self) -> bool {
        let Some(loaded) = &self.image else {
            return false;
        };
        if loaded.full {
            return false;
        }
        let shown = loaded.image.size(0);
        let needed = self.zoom.borrow().displayed_pixels();
        needed.width > shown.width.0 as f32 || needed.height > shown.height.0 as f32
    }

    /// Loads the photo at full resolution if it's zoomed in further than
    /// the image shown has pixels for. It's kept on the CPU, and shown in
    /// tiles (see `update_tiles`).
    fn load_full_if_needed(&mut self, cx: &mut Context<Self>) {
        if self.tiles.is_some()
            || self.load_full.is_some()
            || self.full_failed
            || !matches!(self.load_state, LoadState::Done)
            || !self.needs_full()
        {
            return;
        }
        let photo = self.stack.photos[self.selected].clone();
        self.load_full = Some(cx.spawn(async move |this, cx| {
            let loaded = cx
                .background_spawn(async move {
                    anyhow::Ok(FullImage::new(decode::decode_full(&photo)?))
                })
                .await;
            this.update(cx, |this, cx| {
                this.load_full = None;
                match loaded {
                    Ok(full) => this.tiles = Some(Tiles::new(full)),
                    // The image fitted stays.
                    Err(error) => {
                        eprintln!("couldn't load the full photo: {error:#}");
                        this.full_failed = true;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    /// Cuts the tiles of the full-resolution photo that have come into
    /// view, and releases those that have gone out of it, or all of them
    /// once it's zoomed out.
    fn update_tiles(&mut self, cx: &mut Context<Self>) {
        let visible = if self.needs_full() {
            self.zoom.borrow().visible_fraction()
        } else {
            None
        };
        let Some(tiles) = &mut self.tiles else {
            return;
        };
        let (released, missing) = tiles.update(visible);
        for tile in released {
            decode::release(tile, cx);
        }
        if missing.is_empty() || self.cutting.is_some() {
            return;
        }
        let source = tiles.source.clone();
        self.cutting = Some(cx.spawn(async move |this, cx| {
            let cut = cx
                .background_spawn({
                    let source = source.clone();
                    async move {
                        missing
                            .into_iter()
                            .map(|key| (key, source.cut(key)))
                            .collect::<Vec<_>>()
                    }
                })
                .await;
            this.update(cx, |this, cx| {
                this.cutting = None;
                match &mut this.tiles {
                    // Those that have gone out of view meanwhile are
                    // released at the next update.
                    Some(tiles) if Arc::ptr_eq(&tiles.source, &source) => tiles.insert(cut),
                    // Another photo's been selected.
                    _ => {
                        for (_, tile) in cut {
                            decode::release(tile, cx);
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// Frees the full-resolution photo and its tiles.
    fn release_tiles(&mut self, cx: &mut App) {
        self.cutting = None;
        if let Some(mut tiles) = self.tiles.take() {
            for tile in tiles.take_all() {
                decode::release(tile, cx);
            }
        }
    }

    fn load_filmstrip(&mut self, cx: &mut Context<Self>) {
        if self.stack.photos.len() < 2 {
            return;
        }
        self._load_filmstrip = self
            .stack
            .photos
            .iter()
            .enumerate()
            .map(|(ix, photo)| {
                let photo = photo.clone();
                cx.spawn(async move |this, cx| {
                    let result = cx
                        .background_spawn(async move { decode::filmstrip_thumbnail(&photo) })
                        .await;
                    this.update(cx, |this, cx| {
                        this.filmstrip[ix] = result.ok();
                        cx.notify();
                    })
                    .ok();
                })
            })
            .collect();
    }

    /// Removes this viewer's images from the GPU atlas. `window` must be
    /// given when called during an update of that window.
    pub fn release_images(&mut self, mut window: Option<&mut Window>, cx: &mut App) {
        self.stop_live(cx);
        self.cutting = None;
        let tiles = self
            .tiles
            .take()
            .map(|mut tiles| tiles.take_all())
            .unwrap_or_default();
        for image in self
            .image
            .take()
            .map(|loaded| loaded.image)
            .into_iter()
            .chain(self.filmstrip.iter_mut().filter_map(Option::take))
            .chain(tiles)
        {
            cx.drop_image(image, window.as_deref_mut());
        }
    }

    fn close(&mut self, _: &CloseViewer, window: &mut Window, cx: &mut Context<Self>) {
        self.release_images(Some(window), cx);
        cx.emit(ViewerEvent::Close);
    }

    /// Steps through a bracket's or burst's frames, then on to the
    /// neighboring photos.
    fn step_frame(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected as isize + delta;
        if ix < 0 {
            cx.emit(ViewerEvent::Previous);
        } else if ix as usize >= self.stack.photos.len() {
            cx.emit(ViewerEvent::Next);
        } else {
            self.select(ix as usize, window, cx);
        }
    }

    /// The size of the image shown, and whether it's at full resolution.
    #[cfg(test)]
    pub fn shown_image(&self) -> Option<(u32, u32, bool)> {
        let loaded = self.image.as_ref()?;
        let size = loaded.image.size(0);
        Some((size.width.0 as u32, size.height.0 as u32, loaded.full))
    }

    /// The full-resolution photo's size and how many tiles of it are
    /// shown, once it's loaded.
    #[cfg(test)]
    pub fn full_tiles(&self) -> Option<(u32, u32, usize)> {
        let tiles = self.tiles.as_ref()?;
        Some((
            tiles.source.width(),
            tiles.source.height(),
            tiles.shown().len(),
        ))
    }

    #[cfg(test)]
    pub fn shown(&self) -> (std::path::PathBuf, usize) {
        (self.stack.cover().path.clone(), self.selected)
    }

    fn update_zoom(&mut self, cx: &mut Context<Self>, f: impl FnOnce(&mut ZoomState)) {
        f(&mut self.zoom.borrow_mut());
        cx.notify();
    }

    fn on_scroll(&mut self, event: &ScrollWheelEvent, _: &mut Window, cx: &mut Context<Self>) {
        let anchor = Some(event.position);
        match event.delta {
            // Mouse wheels zoom.
            ScrollDelta::Lines(delta) => self.update_zoom(cx, |zoom| {
                zoom.zoom_by(ZOOM_STEP.powf(delta.y), anchor);
            }),
            // Trackpads pan, or zoom while ⌘ or ctrl is held.
            ScrollDelta::Pixels(delta) => {
                if event.modifiers.platform || event.modifiers.control {
                    let factor = 2f32.powf(f32::from(delta.y) / 200.);
                    self.update_zoom(cx, |zoom| zoom.zoom_by(factor, anchor));
                } else {
                    self.update_zoom(cx, |zoom| zoom.pan_by(delta));
                }
            }
        }
    }

    fn on_mouse_down(&mut self, event: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let is_double_click = event.click_count == 2 && self.pane_clicked;
        self.pane_clicked = true;
        if is_double_click {
            self.update_zoom(cx, |zoom| zoom.toggle_actual_size(Some(event.position)));
        } else {
            self.update_zoom(cx, |zoom| zoom.start_drag(event.position));
        }
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let mut zoom = self.zoom.borrow_mut();
        if event.pressed_button != Some(MouseButton::Left) {
            zoom.end_drag();
        } else if zoom.drag_to(event.position) {
            cx.notify();
        }
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> Div {
        let top_bar = div().flex().flex_none().items_center().gap_3().h(px(44.));
        navigation::title_bar_start(top_bar, cx)
            .pr_3()
            .border_b_1()
            .border_color(cx.theme().title_bar_border)
            .bg(cx.theme().title_bar)
            .child(
                Button::new("back")
                    .icon(IconName::ChevronLeft)
                    .label("Gallery")
                    .small()
                    .on_click(
                        cx.listener(|this, _, window, cx| this.close(&CloseViewer, window, cx)),
                    ),
            )
    }

    /// The file name and position in the stack on the left; loading state
    /// and zoom on the right.
    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let photo = &self.stack.photos[self.selected];
        let name: SharedString = photo
            .path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
            .into();
        let position = match self.stack.kind {
            StackKind::Single => None,
            StackKind::Bracket => Some("Bracket"),
            StackKind::Burst => Some("Burst"),
        }
        .map(|kind| {
            format!(
                "{kind} · {} of {}",
                self.selected + 1,
                self.stack.photos.len()
            )
        });
        let loading_full = self.load_full.is_some();
        let status = match (&self.load_state, &self.image) {
            (LoadState::Failed(_), Some(_)) => Some("Preview only"),
            (LoadState::Loading, _) => Some("Loading…"),
            _ if loading_full => Some("Loading full resolution…"),
            _ => None,
        };
        let zoom = self.zoom.borrow();
        let zoom_label: SharedString = if zoom.is_fit() {
            "Fit".into()
        } else {
            format!("{:.0}%", zoom.actual_percent()).into()
        };

        StatusBar::new()
            .flex_none()
            .px_3()
            .left(div().text_color(cx.theme().foreground).child(name))
            .when_some(position, |bar, position| bar.left(position))
            .when_some(status, |bar, status| bar.right(status))
            .right(zoom_label)
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let objects = self.objects.as_ref().map(|(_, section)| {
            let show = settings::get(cx).show_object_boxes;
            let toggle = Button::new("object-boxes")
                .icon(Icon::new(LucideIcon::ScanEye))
                .label(if show { "Hide Boxes" } else { "Show Boxes" })
                .xsmall()
                .ghost()
                .selected(show)
                .tooltip("Draw boxes around the objects in the photo")
                .on_click(cx.listener(move |_, _, _, cx| {
                    settings::set_show_object_boxes(!show, cx);
                    cx.notify();
                }));
            render_section(section, Some(toggle.into_any_element()), cx)
        });
        div()
            .id("details")
            .flex_none()
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .overflow_y_scroll()
            .border_l_1()
            .border_color(cx.theme().sidebar_border)
            .bg(cx.theme().sidebar)
            .p_4()
            .flex()
            .flex_col()
            .gap_5()
            .children(
                self.details
                    .iter()
                    .map(|section| render_section(section, None, cx)),
            )
            .children(objects)
            .children(self.render_albums(cx))
    }

    /// The objects to draw boxes around, if they're shown.
    pub fn object_boxes(&self, cx: &App) -> Option<Arc<[DetectedObject]>> {
        self.objects
            .as_ref()
            .filter(|_| settings::get(cx).show_object_boxes)
            .map(|(objects, _)| objects.clone())
    }

    fn render_image_pane(&self, cx: &mut Context<Self>) -> AnyElement {
        // Until the preview loads, stretch a thumbnail we already have.
        let image = self
            .image
            .as_ref()
            .map(|loaded| loaded.image.clone())
            .or_else(|| self.filmstrip[self.selected].clone())
            .or_else(|| {
                (self.selected == self.stack.cover)
                    .then(|| self.cover_thumbnail.clone())
                    .flatten()
            });
        let error: Option<SharedString> = match (&self.load_state, &self.image) {
            (LoadState::Failed(error), None) => {
                Some(format!("Can't display image: {error}").into())
            }
            _ => None,
        };
        let zoom = self.zoom.borrow();
        // A playing Live Photo covers the photo, zoomed and panned with it;
        // the object boxes mark the photo, so they're hidden meanwhile.
        let live_frame = self
            .live
            .as_ref()
            .and_then(|(player, _)| player.read(cx).render_frame(ObjectFit::Cover))
            .map(|frame| div().relative().size_full().overflow_hidden().child(frame));
        let boxes = if live_frame.is_some() {
            None
        } else {
            self.object_boxes(cx)
        };

        let pane = div()
            .id("image-pane")
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .bg(cx.theme().background)
            .when(!zoom.is_fit(), |pane| pane.cursor_grab())
            .when(zoom.is_dragging(), |pane| pane.cursor_grabbing())
            .on_scroll_wheel(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.update_zoom(cx, ZoomState::end_drag)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.update_zoom(cx, ZoomState::end_drag)),
            )
            .children(image.map(|image| {
                let tiles = self.tiles.as_ref().map(Tiles::shown).unwrap_or_default();
                let image = zoomed_image(image, self.zoom.clone()).tiles(tiles);
                match (live_frame, boxes) {
                    (Some(frame), _) => image.overlay(frame),
                    (None, Some(objects)) => image.overlay(ObjectBoxes::new(objects)),
                    (None, None) => image,
                }
            }))
            .children(error.map(|error| {
                div()
                    .size_full()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(error)
            }))
            .children(self.render_live(cx))
            .when(self.stack.photos.len() > 1, |pane| {
                pane.child(self.render_filmstrip(cx))
            });
        // Found by UI tests.
        let pane = pane.test_support();
        // Right-click for the albums menu, for a library's photos.
        if self.photo_albums.is_none() {
            return pane.into_any_element();
        }
        let menu = Rc::new(self.albums_menu(cx));
        pane.context_menu(move |popup, _, _| menu(popup.label("Albums")))
            .into_any_element()
    }

    /// For Live Photos: the LIVE button that plays the video (on hover, like
    /// Photos, or on click). The video is drawn over the photo by
    /// `render_image_pane`.
    fn render_live(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.stack.photos[self.selected].live_video.as_ref()?;
        let button = div()
            .id("live")
            .absolute()
            .top_3()
            .left_3()
            .occlude()
            .cursor_pointer()
            .child(
                pill()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        svg()
                            .path(LIVE_ICON)
                            .size_3p5()
                            .text_color(rgba(0xffffffff)),
                    )
                    .child("LIVE"),
            )
            .on_hover(cx.listener(|this, hovered: &bool, _, cx| {
                if *hovered {
                    this.play_live(cx);
                }
            }))
            .on_click(cx.listener(|this, _, _, cx| this.play_live(cx)))
            .into_any_element();
        Some(button)
    }

    /// Every frame of a bracket or burst, floating over the bottom of the
    /// image pane.
    fn render_filmstrip(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let icon = match self.stack.kind {
            StackKind::Burst => BURST_ICON,
            _ => BRACKET_ICON,
        };
        let theme = cx.theme();
        let (primary, muted, popover, popover_foreground, border) = (
            theme.primary,
            theme.muted,
            theme.popover,
            theme.popover_foreground,
            theme.border,
        );

        // The full-width row only centers the strip; without a hitbox of its
        // own, it doesn't block panning the image on either side.
        div()
            .absolute()
            .bottom_4()
            .left_0()
            .right_0()
            .px_4()
            .flex()
            .justify_center()
            .child(
                div()
                    .id("filmstrip")
                    // Keep clicks and scrolling from reaching the image pane.
                    .occlude()
                    .max_w_full()
                    .flex()
                    .items_center()
                    .gap_2()
                    .p_2()
                    .rounded_lg()
                    .bg(popover.opacity(0.72))
                    .border_1()
                    .border_color(border.opacity(0.6))
                    .shadow_lg()
                    .overflow_x_scroll()
                    // The key: the stack's icon above the unit of the labels
                    // under each frame ("EV" for brackets).
                    .child(
                        filmstrip_column()
                            .px_1()
                            .child(
                                div()
                                    .h(px(FILMSTRIP_THUMBNAIL_HEIGHT))
                                    .flex()
                                    .items_center()
                                    .child(
                                        svg()
                                            .path(icon)
                                            .size_4()
                                            .text_color(popover_foreground.opacity(0.7)),
                                    ),
                            )
                            .child(
                                filmstrip_label()
                                    .text_color(popover_foreground.opacity(0.6))
                                    .child(match self.stack.kind {
                                        StackKind::Bracket => "EV",
                                        _ => "",
                                    }),
                            ),
                    )
                    .children(self.stack.photos.iter().enumerate().map(|(ix, photo)| {
                        let selected = ix == self.selected;
                        let label = match self.stack.kind {
                            StackKind::Bracket => {
                                format_ev_value(photo.meta.exposure_bias.unwrap_or(0.0))
                            }
                            _ => format!("{}", ix + 1),
                        };
                        filmstrip_column()
                            .id(("frame", ix))
                            .cursor_pointer()
                            .when(!selected, |frame| {
                                frame.opacity(0.75).hover(|s| s.opacity(1.))
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
                                filmstrip_label()
                                    .text_color(if selected {
                                        popover_foreground
                                    } else {
                                        popover_foreground.opacity(0.6)
                                    })
                                    .child(label),
                            )
                            .on_click(
                                cx.listener(move |this, _, window, cx| this.select(ix, window, cx)),
                            )
                    })),
            )
    }
}

fn filmstrip_column() -> Div {
    div().flex_none().flex().flex_col().items_center().gap_1()
}

fn filmstrip_label() -> Div {
    div().h(px(14.)).text_xs().line_height(px(14.))
}

// Pills and labels sit on top of photos, so they use fixed colors that read
// on any image rather than theme colors.
fn pill() -> Div {
    div()
        .px_2()
        .py_0p5()
        .rounded_md()
        .bg(rgba(0x000000a6))
        .text_color(rgba(0xffffffff))
        .text_xs()
}

/// Size in image pixels of a stack's frame, with EXIF orientation applied.
fn image_size(stack: &Stack, ix: usize) -> gpui_kit::Size<f32> {
    let photo = &stack.photos[ix];
    size(photo.width as f32, photo.height as f32)
}

impl Focusable for PhotoViewer {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PhotoViewer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.load_full_if_needed(cx);
        self.update_tiles(cx);
        div()
            .track_focus(&self.focus_handle)
            .key_context(KEY_CONTEXT)
            .on_action(cx.listener(Self::close))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| {
                this.update_zoom(cx, |zoom| zoom.zoom_by(ZOOM_STEP, None))
            }))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| {
                this.update_zoom(cx, |zoom| zoom.zoom_by(1. / ZOOM_STEP, None))
            }))
            .on_action(cx.listener(|this, _: &ZoomToFit, _, cx| {
                this.update_zoom(cx, ZoomState::zoom_to_fit)
            }))
            .on_action(cx.listener(|this, _: &ActualSize, _, cx| {
                this.update_zoom(cx, ZoomState::zoom_to_actual_size)
            }))
            .on_action(
                cx.listener(|this, _: &PreviousFrame, window, cx| this.step_frame(-1, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NextFrame, window, cx| this.step_frame(1, window, cx)),
            )
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(self.render_top_bar(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_image_pane(cx))
                    .child(self.render_sidebar(cx)),
            )
            .child(self.render_status_bar(cx))
    }
}

/// A section of the sidebar: its title, with `action` beside it if given,
/// over its rows of labels and values.
fn render_section(section: &DetailSection, action: Option<AnyElement>, cx: &App) -> Div {
    let muted_foreground = cx.theme().muted_foreground;
    div()
        .flex()
        .flex_col()
        .gap_1p5()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .child(
                    div()
                        .text_xs()
                        .text_color(muted_foreground)
                        .child(section.title.to_uppercase()),
                )
                .children(action),
        )
        .children(section.rows.iter().map(|(label, value)| {
            div()
                .flex()
                .gap_2()
                .text_xs()
                .child(
                    div()
                        .flex_none()
                        .w(px(96.))
                        .text_color(muted_foreground)
                        .child(label.clone()),
                )
                .child(div().flex_1().min_w_0().child(value.clone()))
        }))
}

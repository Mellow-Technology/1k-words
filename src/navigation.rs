//! The navigation sidebar on the left of the window.
//!
//! Whether it's open is app-wide state, because the title bars of both the
//! gallery and the viewer have to make room for the macOS traffic lights
//! when it's closed.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarItem, SidebarMenu, SidebarMenuItem,
    SidebarToggleButton,
};
use gpui_kit::component::{ActiveTheme, Icon, IconName, Sizable};
use gpui_kit::{
    App, Context, Div, Global, InteractiveElement as _, IntoElement, KeyBinding, ParentElement,
    Render, RenderOnce, Styled, TestSupportExt as _, ViewElement, Window, div, px,
};

use crate::devices::{self, Device};
use crate::library::{self, Album, AlbumId, child_albums};

gpui_kit::actions!(navigation, [ToggleSidebar, OpenSettings]);

/// What the main area of the window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Page {
    /// The open folder: the gallery grid or the photo viewer.
    #[default]
    Library,
    Settings,
}

const SIDEBAR_WIDTH: f32 = 220.;
/// Room for the traffic lights in a title bar when the sidebar is closed.
const TRAFFIC_LIGHTS_WIDTH: f32 = 76.;

#[derive(Default)]
struct SidebarState {
    collapsed: bool,
}

impl Global for SidebarState {}

pub fn init(cx: &mut App) {
    cx.set_global(SidebarState::default());
    cx.on_action(|_: &ToggleSidebar, cx| {
        let state = cx.global_mut::<SidebarState>();
        state.collapsed = !state.collapsed;
        cx.refresh_windows();
    });
    cx.bind_keys([
        KeyBinding::new("cmd-ctrl-s", ToggleSidebar, None),
        KeyBinding::new("cmd-,", OpenSettings, None),
    ]);
}

pub fn is_collapsed(cx: &App) -> bool {
    cx.global::<SidebarState>().collapsed
}

/// Adds the start of a title bar: room for the traffic lights and a button
/// to reopen the sidebar when it's closed, or just padding when it's open
/// (the sidebar holds the traffic lights then).
pub fn title_bar_start(title_bar: Div, cx: &App) -> Div {
    if is_collapsed(cx) {
        title_bar
            .pl(px(TRAFFIC_LIGHTS_WIDTH))
            .child(toggle_button(true))
    } else {
        title_bar.pl_3()
    }
}

pub type LibraryCallback = Rc<dyn Fn(&str, &mut Window, &mut App)>;
pub type DeviceCallback = Rc<dyn Fn(&Device, &mut Window, &mut App)>;
pub type AlbumCallback = Rc<dyn Fn(AlbumId, &mut Window, &mut App)>;
/// Makes an album, in the one given if any.
pub type NewAlbumCallback = Rc<dyn Fn(Option<AlbumId>, &mut Window, &mut App)>;

/// What the sidebar shows about the app's current state.
pub struct NavigationContext {
    pub page: Page,
    /// Whether the whole of the open library is shown (rather than an
    /// album or a device).
    pub showing_library: bool,
    /// Shows the library with this id, opening it if it isn't open.
    pub show_library: LibraryCallback,
    /// Mount point of the device whose photos are shown, if any.
    pub open_device: Option<PathBuf>,
    /// Opens a device's photos, or shows them if it's already open.
    pub show_device: DeviceCallback,
    pub eject_device: DeviceCallback,
    /// The open library's albums.
    pub albums: Arc<Vec<Album>>,
    /// The album shown, if any.
    pub open_album: Option<AlbumId>,
    pub show_album: AlbumCallback,
    pub new_album: NewAlbumCallback,
    pub rename_album: AlbumCallback,
    pub delete_album: AlbumCallback,
    /// Adds the photos being dragged (see `DraggedPhotos`) to an album.
    pub drop_on_album: AlbumCallback,
}

/// The gallery's selected photos, being dragged onto an album.
#[derive(Clone)]
pub struct DraggedPhotos {
    /// How many there are, for the badge under the pointer.
    pub count: usize,
}

impl Render for DraggedPhotos {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = match self.count {
            1 => "1 photo".to_owned(),
            count => format!("{count} photos"),
        };
        div()
            .flex()
            .items_center()
            .gap_1p5()
            .px_2()
            .py_1()
            .rounded_md()
            .shadow_md()
            .text_sm()
            .bg(cx.theme().primary)
            .text_color(cx.theme().primary_foreground)
            .child(Icon::new(LucideIcon::Images).small())
            .child(label)
    }
}

pub fn navigation_sidebar(context: NavigationContext, cx: &App) -> impl IntoElement {
    let devices = devices_group(&context, cx);
    let current = library::current(cx).map(|library| library.info.id.clone());
    let items: Vec<SidebarMenuItem> = library::list(cx)
        .iter()
        .map(|info| {
            let active = context.page == Page::Library
                && context.showing_library
                && current.as_deref() == Some(info.id.as_str());
            let show_library = context.show_library.clone();
            let id = info.id.clone();
            SidebarMenuItem::new(info.name.clone())
                .icon(Icon::new(LucideIcon::Images))
                .active(active)
                .on_click(move |_, window, cx| show_library(&id, window, cx))
        })
        .collect();

    Sidebar::new("navigation")
        .collapsible(SidebarCollapsible::Offcanvas)
        .collapsed(is_collapsed(cx))
        .w(px(SIDEBAR_WIDTH))
        .header(
            // The traffic lights sit to the left of this row.
            div()
                .w_full()
                .flex()
                .justify_end()
                .child(toggle_button(false)),
        )
        .child(SidebarGroup::new("Libraries").child(SidebarMenu::new().children(items)))
        .children(albums_group(&context, cx))
        .children(devices)
        .footer(MenuElement(
            SidebarMenu::new().child(
                SidebarMenuItem::new("Settings")
                    .icon(IconName::Settings)
                    .active(context.page == Page::Settings)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenSettings), cx)),
            ),
        ))
}

/// The open library's albums, nested as they are, and a button to make
/// one.
fn albums_group(context: &NavigationContext, cx: &App) -> Option<SidebarGroup<SidebarMenu>> {
    library::current(cx)?;
    // The albums the open one is in start out open, to show it.
    let open_path: Vec<AlbumId> = context
        .open_album
        .map(|id| {
            library::album_path(&context.albums, id)
                .iter()
                .map(|album| album.id)
                .collect()
        })
        .unwrap_or_default();
    let albums =
        child_albums(&context.albums, None).map(|album| album_item(context, album, &open_path));
    let new_album = context.new_album.clone();
    let add = SidebarMenuItem::new("New Album…")
        .icon(Icon::new(LucideIcon::FolderPlus))
        .on_click(move |_, window, cx| new_album(None, window, cx));
    Some(SidebarGroup::new("Albums").child(SidebarMenu::new().children(albums.chain([add]))))
}

/// An album, with the albums in it beneath, and a menu to make an album in
/// it, rename it, or delete it.
fn album_item(
    context: &NavigationContext,
    album: &Album,
    open_path: &[AlbumId],
) -> SidebarMenuItem {
    let children: Vec<SidebarMenuItem> = child_albums(&context.albums, Some(album.id))
        .map(|child| album_item(context, child, open_path))
        .collect();
    let id = album.id;
    let active = context.page == Page::Library && context.open_album == Some(id);
    let contains_open = open_path.contains(&id) && context.open_album != Some(id);
    let show_album = context.show_album.clone();
    let new_album = context.new_album.clone();
    let rename_album = context.rename_album.clone();
    let delete_album = context.delete_album.clone();
    let drop_on_album = context.drop_on_album.clone();
    SidebarMenuItem::new(album.name.clone())
        .icon(Icon::new(LucideIcon::Album))
        .active(active)
        .default_open(contains_open)
        .on_click(move |_, window, cx| show_album(id, window, cx))
        // Photos are dropped on the row. Its suffix is the only place for
        // that, so it's stretched over the row's label, without taking its
        // clicks.
        .suffix(move |_, cx| {
            let drop_on_album = drop_on_album.clone();
            let highlight = cx.theme().primary;
            div()
                .id(("album-drop", id as u64))
                .absolute()
                .inset_0()
                .rounded_sm()
                .drag_over::<DraggedPhotos>(move |style, _, _, _| {
                    style
                        .bg(highlight.opacity(0.2))
                        .border_1()
                        .border_color(highlight)
                })
                .on_drop(move |_: &DraggedPhotos, window, cx| drop_on_album(id, window, cx))
                // Found by UI tests.
                .test_support()
        })
        .children(children)
        .context_menu(move |menu, _, _| {
            let new_album = new_album.clone();
            let rename_album = rename_album.clone();
            let delete_album = delete_album.clone();
            menu.item(
                PopupMenuItem::new("New Album Inside…")
                    .on_click(move |_, window, cx| new_album(Some(id), window, cx)),
            )
            .item(
                PopupMenuItem::new("Rename…")
                    .on_click(move |_, window, cx| rename_album(id, window, cx)),
            )
            .separator()
            .item(
                PopupMenuItem::new("Delete Album…")
                    .on_click(move |_, window, cx| delete_album(id, window, cx)),
            )
        })
}

/// The removable devices, if any are mounted, each with an eject button.
fn devices_group(context: &NavigationContext, cx: &App) -> Option<SidebarGroup<SidebarMenu>> {
    let devices = devices::list(cx);
    if devices.is_empty() {
        return None;
    }
    let items = devices.iter().enumerate().map(|(ix, device)| {
        let active =
            context.page == Page::Library && context.open_device.as_ref() == Some(&device.path);
        let show_device = context.show_device.clone();
        let eject_device = context.eject_device.clone();
        let clicked = device.clone();
        let ejected = device.clone();
        SidebarMenuItem::new(device.name.clone())
            .icon(Icon::new(LucideIcon::HardDrive))
            .active(active)
            .on_click(move |_, window, cx| show_device(&clicked, window, cx))
            .suffix(move |_, _| {
                let eject_device = eject_device.clone();
                let ejected = ejected.clone();
                Button::new(("eject", ix))
                    .icon(Icon::new(LucideIcon::Eject))
                    .ghost()
                    .xsmall()
                    .tooltip(format!("Eject {}", ejected.name))
                    .on_click(move |_, window, cx| eject_device(&ejected, window, cx))
            })
    });
    Some(SidebarGroup::new("Devices").child(SidebarMenu::new().children(items)))
}

/// Renders a sidebar menu outside the sidebar's groups, e.g. in its footer.
#[derive(Clone)]
struct MenuElement(SidebarMenu);

impl RenderOnce for MenuElement {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        SidebarItem::render(self.0, "footer-menu", window, cx)
    }
}

impl IntoElement for MenuElement {
    type Element = ViewElement<Self>;

    fn into_element(self) -> Self::Element {
        ViewElement::new(self)
    }
}

fn toggle_button(collapsed: bool) -> SidebarToggleButton {
    SidebarToggleButton::new()
        .collapsed(collapsed)
        .accessibility_label(if collapsed {
            "Show Sidebar"
        } else {
            "Hide Sidebar"
        })
        .on_click(|_, window, cx| window.dispatch_action(Box::new(ToggleSidebar), cx))
}

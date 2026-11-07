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
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::menu::PopupMenuItem;
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarItem, SidebarMenu, SidebarMenuItem,
    SidebarToggleButton,
};
use gpui_kit::component::{
    ActiveTheme, Collapsible, Icon, IconName, Sizable, StyledExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Div, ElementId, Global, Hsla,
    InteractiveElement as _, IntoElement, KeyBinding, ParentElement, Render, RenderOnce, Role,
    SharedString, Stateful, StatefulInteractiveElement as _, StyleRefinement, Styled,
    TestSupportExt as _, ViewElement, Window, div, percentage, prelude::FluentBuilder as _, px,
};

use crate::devices::{self, Device};
use crate::library::{self, Album, AlbumId, child_albums, nested_albums};

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
/// Puts an album in another, or at the top level.
pub type MoveAlbumCallback = Rc<dyn Fn(AlbumId, Option<AlbumId>, &mut Window, &mut App)>;

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
    pub move_album: MoveAlbumCallback,
    /// Turns showing the photos of the albums in an album on or off.
    pub toggle_show_nested: AlbumCallback,
    /// The albums pinned to stay on this device.
    pub pinned_albums: Arc<Vec<AlbumId>>,
    pub toggle_album_pin: AlbumCallback,
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
        drag_badge(LucideIcon::Images, label.into(), cx)
    }
}

/// An album being dragged in the sidebar, to put it in another.
#[derive(Clone)]
pub struct DraggedAlbum {
    pub id: AlbumId,
    pub name: SharedString,
}

impl Render for DraggedAlbum {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        drag_badge(LucideIcon::Album, self.name.clone(), cx)
    }
}

fn drag_badge(icon: LucideIcon, label: SharedString, cx: &App) -> Div {
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
        .child(Icon::new(icon).small())
        .child(label)
}

pub fn navigation_sidebar(context: NavigationContext, cx: &App) -> impl IntoElement {
    let context = Rc::new(context);
    let devices = devices_group(&context, cx).map(Section::Menu);
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
        .child(Section::Menu(
            SidebarGroup::new("Libraries").child(SidebarMenu::new().children(items)),
        ))
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

/// A group of the sidebar: a menu, or the albums, which draw their own
/// rows (see `AlbumTree`).
#[derive(Clone)]
enum Section {
    Menu(SidebarGroup<SidebarMenu>),
    Albums(SidebarGroup<AlbumTree>),
}

impl Collapsible for Section {
    fn is_collapsed(&self) -> bool {
        match self {
            Section::Menu(group) => group.is_collapsed(),
            Section::Albums(group) => group.is_collapsed(),
        }
    }

    fn collapsed(self, collapsed: bool) -> Self {
        match self {
            Section::Menu(group) => Section::Menu(group.collapsed(collapsed)),
            Section::Albums(group) => Section::Albums(group.collapsed(collapsed)),
        }
    }
}

impl SidebarItem for Section {
    fn render(
        self,
        id: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        match self {
            Section::Menu(group) => group.render(id, window, cx).into_any_element(),
            Section::Albums(group) => group.render(id, window, cx).into_any_element(),
        }
    }
}

/// The open library's albums, nested as they are, and a button to make
/// one.
fn albums_group(context: &Rc<NavigationContext>, cx: &App) -> Option<Section> {
    library::current(cx)?;
    Some(Section::Albums(SidebarGroup::new("Albums").child(
        AlbumTree {
            context: context.clone(),
            collapsed: false,
        },
    )))
}

/// The albums in the sidebar. They're drawn like GPUI Kit's
/// `SidebarMenuItem`s, but each whole row can be dragged, to move the album
/// into another, and photos and albums dropped on it, which its rows don't
/// allow.
#[derive(Clone)]
struct AlbumTree {
    context: Rc<NavigationContext>,
    collapsed: bool,
}

impl Collapsible for AlbumTree {
    fn is_collapsed(&self) -> bool {
        self.collapsed
    }

    fn collapsed(mut self, collapsed: bool) -> Self {
        self.collapsed = collapsed;
        self
    }
}

impl SidebarItem for AlbumTree {
    fn render(
        self,
        _: impl Into<ElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> impl IntoElement {
        let context = &self.context;
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
        let new_album = context.new_album.clone();
        let add = sidebar_row(
            "new-album".into(),
            LucideIcon::FolderPlus,
            "New Album…".into(),
            false,
            cx,
        )
        .on_click(move |_, window, cx| new_album(None, window, cx));
        v_flex()
            .gap_2()
            .children(
                child_albums(&context.albums, None)
                    .map(|album| album_item(context, album, &open_path, window, cx)),
            )
            .child(add)
    }
}

/// A row as GPUI Kit's sidebar draws its menu items.
fn sidebar_row(
    id: ElementId,
    icon: LucideIcon,
    label: SharedString,
    active: bool,
    cx: &App,
) -> Stateful<Div> {
    let theme = cx.theme();
    h_flex()
        .id(id)
        .role(Role::TreeItem)
        .aria_label(label.clone())
        .aria_selected(active)
        .w_full()
        .h_7()
        .flex_shrink_0()
        .overflow_x_hidden()
        .p_2()
        .gap_x_2()
        .rounded(theme.radius)
        .text_sm()
        .when(!active, |row| {
            let (accent, foreground) = (theme.sidebar_accent, theme.sidebar_accent_foreground);
            row.hover(move |row| row.bg(accent.opacity(0.8)).text_color(foreground))
        })
        .when(active, |row| {
            row.font_medium()
                .bg(theme.tokens.sidebar_accent)
                .text_color(theme.sidebar_accent_foreground)
        })
        .child(Icon::new(icon))
        .child(h_flex().flex_1().overflow_x_hidden().child(label))
}

/// An album, with the albums in it beneath, and a menu to make an album in
/// it, rename it, move it or delete it.
fn album_item(
    context: &Rc<NavigationContext>,
    album: &Album,
    open_path: &[AlbumId],
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let id = album.id;
    let children: Vec<&Album> = child_albums(&context.albums, Some(id)).collect();
    let has_children = !children.is_empty();
    let active = context.page == Page::Library && context.open_album == Some(id);
    let open = has_children.then(|| {
        let contains_open = open_path.contains(&id) && context.open_album != Some(id);
        window.use_keyed_state(("album-open", id.key()), cx, move |_, _| contains_open)
    });
    let is_open = open.as_ref().is_some_and(|open| *open.read(cx));
    let highlight = cx.theme().primary;

    let mut row = sidebar_row(
        ("album", id.key()).into(),
        LucideIcon::Album,
        album.name.clone().into(),
        active,
        cx,
    );
    if let Some(open) = open {
        row = row.child(
            Button::new("caret")
                .xsmall()
                .ghost()
                .icon(
                    Icon::new(IconName::ChevronRight)
                        .size_4()
                        .when(is_open, |icon| icon.rotate(percentage(0.25))),
                )
                .on_click(move |_, _, cx| {
                    // Not showing the album too.
                    cx.stop_propagation();
                    open.update(cx, |open, cx| {
                        *open = !*open;
                        cx.notify();
                    });
                }),
        );
    }
    let show_album = context.show_album.clone();
    let drop_on_album = context.drop_on_album.clone();
    let move_album = context.move_album.clone();
    let albums = context.albums.clone();
    let row = row
        .on_click(move |_, window, cx| show_album(id, window, cx))
        .on_drag(
            DraggedAlbum {
                id,
                name: album.name.clone().into(),
            },
            |dragged, _, _, cx| cx.new(|_| dragged.clone()),
        )
        // Not into itself, or an album in it.
        .can_drop(move |dragged, _, _| {
            dragged
                .downcast_ref::<DraggedAlbum>()
                .is_none_or(|album| !nested_albums(&albums, album.id).contains(&id))
        })
        .drag_over::<DraggedPhotos>(move |style, _, _, _| drop_highlight(style, highlight))
        .drag_over::<DraggedAlbum>(move |style, _, _, _| drop_highlight(style, highlight))
        .on_drop(move |_: &DraggedPhotos, window, cx| drop_on_album(id, window, cx))
        .on_drop({
            let move_album = move_album.clone();
            move |album: &DraggedAlbum, window, cx| move_album(album.id, Some(id), window, cx)
        })
        // Found by UI tests.
        .test_support();

    let new_album = context.new_album.clone();
    let rename_album = context.rename_album.clone();
    let delete_album = context.delete_album.clone();
    let toggle_show_nested = context.toggle_show_nested.clone();
    let toggle_album_pin = context.toggle_album_pin.clone();
    let pinned = context.pinned_albums.contains(&id);
    let (show_nested, nested) = (album.show_nested, album.parent.is_some());
    let row = row.context_menu(move |menu, _, _| {
        let new_album = new_album.clone();
        let rename_album = rename_album.clone();
        let delete_album = delete_album.clone();
        let move_album = move_album.clone();
        let toggle_show_nested = toggle_show_nested.clone();
        let toggle_album_pin = toggle_album_pin.clone();
        menu.item(
            PopupMenuItem::new("New Album Inside…")
                .on_click(move |_, window, cx| new_album(Some(id), window, cx)),
        )
        .item(
            PopupMenuItem::new("Rename…")
                .on_click(move |_, window, cx| rename_album(id, window, cx)),
        )
        .when(has_children, |menu| {
            menu.item(
                PopupMenuItem::new("Show Photos from Albums Inside")
                    .checked(show_nested)
                    .on_click(move |_, window, cx| toggle_show_nested(id, window, cx)),
            )
        })
        .when(nested, |menu| {
            menu.item(
                PopupMenuItem::new("Move to Top Level")
                    .on_click(move |_, window, cx| move_album(id, None, window, cx)),
            )
        })
        .item(
            PopupMenuItem::new("Keep on This Device")
                .checked(pinned)
                .on_click(move |_, window, cx| toggle_album_pin(id, window, cx)),
        )
        .separator()
        .item(
            PopupMenuItem::new("Delete Album…")
                .on_click(move |_, window, cx| delete_album(id, window, cx)),
        )
    });

    let border = cx.theme().sidebar_border;
    v_flex()
        .w_full()
        .child(row)
        .when(is_open, |item| {
            item.child(
                v_flex()
                    .border_l_1()
                    .border_color(border)
                    .gap_1()
                    .ml_3p5()
                    .pl_2p5()
                    .py_0p5()
                    .children(
                        children
                            .into_iter()
                            .map(|child| album_item(context, child, open_path, window, cx)),
                    ),
            )
        })
        .into_any_element()
}

fn drop_highlight(style: StyleRefinement, highlight: Hsla) -> StyleRefinement {
    style
        .bg(highlight.opacity(0.2))
        .border_1()
        .border_color(highlight)
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

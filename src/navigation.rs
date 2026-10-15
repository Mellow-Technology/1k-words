//! The navigation sidebar on the left of the window.
//!
//! Whether it's open is app-wide state, because the title bars of both the
//! gallery and the viewer have to make room for the macOS traffic lights
//! when it's closed.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::sidebar::{
    Sidebar, SidebarCollapsible, SidebarGroup, SidebarItem, SidebarMenu, SidebarMenuItem,
    SidebarToggleButton,
};
use gpui_kit::component::{Icon, IconName, Sizable};
use gpui_kit::{
    App, Div, Global, IntoElement, KeyBinding, ParentElement, RenderOnce, Styled, ViewElement,
    Window, div, px,
};

use crate::devices::{self, Device};
use crate::library;

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

/// What the sidebar shows about the app's current state.
pub struct NavigationContext {
    pub page: Page,
    /// Whether the open library is shown (rather than a device).
    pub showing_library: bool,
    /// Shows the library with this id, opening it if it isn't open.
    pub show_library: LibraryCallback,
    /// Mount point of the device whose photos are shown, if any.
    pub open_device: Option<PathBuf>,
    /// Opens a device's photos, or shows them if it's already open.
    pub show_device: DeviceCallback,
    pub eject_device: DeviceCallback,
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

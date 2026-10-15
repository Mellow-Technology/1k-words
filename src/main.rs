mod aspect_fit;
mod assets;
mod decode;
mod details;
mod devices;
mod gallery;
mod heif;
mod library;
mod location;
mod masonry;
mod navigation;
mod paths;
mod photos;
mod quick_preview;
mod raw;
mod settings;
mod video;
mod viewer;
mod zoom;

use gpui_kit::{
    App, Bounds, Focusable, KeyBinding, Menu, MenuItem, TitlebarOptions, WindowBounds,
    WindowOptions, point, prelude::*, px, size,
};

use gallery::{Gallery, ImportPhotos, NewLibrary};

gpui_kit::actions!(app, [Quit]);

fn main() {
    gpui_kit::application()
        .with_assets(assets::Assets)
        .run(move |cx: &mut App| {
            gpui_kit::init(cx);
            assets::load_fonts(cx);
            settings::init(cx);
            navigation::init(cx);
            devices::init(cx);
            library::init(cx);
            cx.on_action(|_: &Quit, cx| cx.quit());
            gallery::bind_keys(cx);
            viewer::bind_keys(cx);
            cx.bind_keys([
                KeyBinding::new("cmd-shift-i", ImportPhotos, None),
                KeyBinding::new("cmd-shift-n", NewLibrary, None),
                KeyBinding::new("cmd-q", Quit, None),
            ]);
            cx.set_menus(vec![
                Menu {
                    name: "1k Words".into(),
                    items: vec![
                        MenuItem::action("Settings…", navigation::OpenSettings),
                        MenuItem::separator(),
                        MenuItem::action("Quit", Quit),
                    ],
                    disabled: false,
                },
                Menu {
                    name: "File".into(),
                    items: vec![
                        MenuItem::action("New Library…", NewLibrary),
                        MenuItem::separator(),
                        MenuItem::action("Import…", ImportPhotos),
                    ],
                    disabled: false,
                },
                Menu {
                    name: "View".into(),
                    items: vec![MenuItem::action(
                        "Toggle Sidebar",
                        navigation::ToggleSidebar,
                    )],
                    disabled: false,
                },
            ]);

            let bounds = Bounds::centered(None, size(px(1200.), px(800.)), cx);
            let (window, _) = gpui_kit::open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    titlebar: Some(TitlebarOptions {
                        title: Some("1k Words".into()),
                        appears_transparent: true,
                        traffic_light_position: Some(point(px(14.), px(15.))),
                    }),
                    window_min_size: Some(size(px(360.), px(300.))),
                    ..Default::default()
                },
                cx,
                |window, cx| {
                    let gallery = cx.new(|cx| Gallery::new(window, cx));
                    let focus_handle = gallery.focus_handle(cx);
                    window.focus(&focus_handle, cx);
                    gallery
                },
            )
            .expect("failed to open window");

            window
                .update(cx, |_, window, _| window.activate_window())
                .ok();
            cx.activate(true);
        });
}

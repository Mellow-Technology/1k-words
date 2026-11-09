//! Syncing libraries' catalogs between devices, through a catalog server
//! (see `library::CatalogServer`).
//!
//! The server is set app-wide, in the Sync page's Catalog Server group: its
//! URL in the settings, its token in the keychain. While it's set, each
//! open library's catalog syncs shortly after startup, every minute, after
//! imports and with Sync Now; all but Sync Now wait while automatic syncing
//! is paused. A sync that fetches changes tells the gallery (see
//! `CatalogChanged`), which shows them, and asks about albums deleted on
//! one device but changed on another.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem};
use gpui_kit::component::{ActiveTheme, Sizable, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Entity, Focusable as _, Global, ParentElement, SharedString, Styled,
    Window, div, px,
};

use crate::config_form::form_row;
use crate::gallery::show_error;
use crate::library::{self, CatalogServer, Library};
use crate::settings;

const KEYCHAIN_SERVICE: &str = "1k Words Catalog Server";
const KEYCHAIN_ACCOUNT: &str = "token";
/// How often catalogs sync without being asked. Often, as each change
/// syncs column by column and the last to arrive wins: the sooner one
/// device's changes reach another, the less they can cross.
const INTERVAL: Duration = Duration::from_secs(60);
/// How long after starting up the first sync waits.
const FIRST_DELAY: Duration = Duration::from_secs(3);

/// Syncs under way, and how the last went, by library.
#[derive(Default)]
struct CatalogSync {
    running: HashSet<String>,
    /// Libraries to sync again when their sync finishes, for changes made
    /// meanwhile.
    again: HashSet<String>,
    last: HashMap<String, Outcome>,
    /// The server's token, once read from the keychain.
    token: Option<Option<String>>,
}

impl Global for CatalogSync {}

struct Outcome {
    at: SystemTime,
    error: Option<String>,
}

/// Set when a sync has fetched changes to a library's catalog, so what's
/// shown of it can be read again.
pub struct CatalogChanged {
    pub library_id: String,
}

impl Global for CatalogChanged {}

fn state(cx: &mut App) -> &mut CatalogSync {
    cx.default_global::<CatalogSync>()
}

/// The server catalogs sync with, if one's set.
fn server(cx: &mut App) -> Option<CatalogServer> {
    let url = settings::get(cx).catalog_server.clone()?;
    let token = state(cx)
        .token
        .get_or_insert_with(
            || match crate::keychain::get(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT) {
                Ok(token) => token.and_then(|token| String::from_utf8(token).ok()),
                Err(error) => {
                    eprintln!("couldn't read the catalog server's token: {error:#}");
                    None
                }
            },
        )
        .clone();
    Some(CatalogServer { url, token })
}

/// Syncs open libraries' catalogs from time to time. Call once, at startup.
pub fn init(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FIRST_DELAY).await;
        loop {
            cx.update(|cx| {
                if !settings::get(cx).pause_automatic_sync {
                    sync_open_libraries(cx);
                }
            });
            cx.background_executor().timer(INTERVAL).await;
        }
    })
    .detach();
}

fn sync_open_libraries(cx: &mut App) {
    for library in library::loaded(cx) {
        start(library, cx);
    }
}

/// Syncs a library's catalog, if a server's set and the setting's on, such
/// as after photos are imported into it.
pub fn sync_in_background(library_id: &str, cx: &mut App) {
    if settings::get(cx).pause_automatic_sync {
        return;
    }
    if let Some(library) = library::loaded(cx)
        .into_iter()
        .find(|library| library.info.id == library_id)
    {
        start(library, cx);
    }
}

/// Starts syncing a library's catalog, if a server's set. If it's syncing
/// already, it syncs again afterwards.
fn start(library: Arc<Library>, cx: &mut App) {
    let Some(server) = server(cx) else {
        return;
    };
    let id = library.info.id.clone();
    let syncing = state(cx);
    if !syncing.running.insert(id.clone()) {
        syncing.again.insert(id);
        return;
    }
    let task = cx.background_spawn(async move { library.sync_catalog(&server) });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            let syncing = state(cx);
            syncing.running.remove(&id);
            let again = syncing.again.remove(&id) && result.is_ok();
            let error = result.as_ref().err().map(|error| format!("{error:#}"));
            if let Some(error) = &error {
                eprintln!("couldn't sync the catalog: {error}");
            }
            syncing.last.insert(
                id.clone(),
                Outcome {
                    at: SystemTime::now(),
                    error,
                },
            );
            // Other devices' syncs arrive with it, and this one's is recorded.
            crate::sync_settings::refresh_library(&id, cx);
            if matches!(result, Ok(true)) {
                cx.set_global(CatalogChanged {
                    library_id: id.clone(),
                });
            }
            if again {
                sync_in_background(&id, cx);
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// The Sync page's group for the catalog server.
pub fn group(cx: &App) -> SettingGroup {
    let url = settings::get(cx).catalog_server.clone();
    let server = SettingItem::new(
        "Server",
        SettingField::render(move |_, _, cx| {
            let set = url.is_some();
            let mut row = div().flex().items_center().gap_2().child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(url.clone().unwrap_or_else(|| "None".into())),
            );
            if set {
                row = row.child(
                    Button::new("sync-catalogs")
                        .label("Sync Now")
                        .small()
                        .on_click(|_, _, cx| sync_open_libraries(cx)),
                );
            }
            row.child(
                Button::new("catalog-server")
                    .label(if set { "Edit…" } else { "Set Up…" })
                    .small()
                    .on_click(|_, window, cx| open_server_dialog(window, cx)),
            )
        }),
    )
    .description(status_text(cx));
    SettingGroup::new()
        .title("Catalog Server")
        .description(
            "Sync your libraries' photos, albums and what's been found in them between your \
             devices, through a turso-sync server.",
        )
        .item(server)
}

/// How the last syncs went.
fn status_text(cx: &App) -> String {
    let Some(syncing) = cx.try_global::<CatalogSync>() else {
        return String::new();
    };
    if settings::get(cx).catalog_server.is_none() {
        return String::new();
    }
    let newer: Vec<String> = library::loaded(cx)
        .iter()
        .filter(|library| library.catalog_is_newer())
        .map(|library| format!("“{}”", library.info.name))
        .collect();
    if !newer.is_empty() {
        return format!(
            "{} was synced from a newer version of 1k Words, so it can't be changed here \
             until this one is updated.",
            newer.join(", ")
        );
    }
    if !syncing.running.is_empty() {
        return "Syncing…".into();
    }
    if let Some(error) = syncing.last.values().find_map(|o| o.error.as_ref()) {
        return format!("The last sync failed: {error}");
    }
    match syncing.last.values().map(|o| o.at).max() {
        Some(at) => format!("Synced {}.", ago(at)),
        None => String::new(),
    }
}

fn ago(at: SystemTime) -> String {
    let minutes = at.elapsed().map_or(0, |d| d.as_secs() / 60);
    match minutes {
        0 => "just now".into(),
        1 => "a minute ago".into(),
        n => format!("{n} minutes ago"),
    }
}

struct ServerForm {
    url: Entity<InputState>,
    token: Entity<InputState>,
}

fn open_server_dialog(window: &mut Window, cx: &mut App) {
    let url = settings::get(cx).catalog_server.clone().unwrap_or_default();
    let token = server(cx)
        .and_then(|server| server.token)
        .unwrap_or_default();
    let form = Arc::new(ServerForm {
        url: cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("https://sync.example.com")
                .default_value(url)
        }),
        token: cx.new(|cx| {
            InputState::new(window, cx)
                .default_value(token)
                .masked(true)
        }),
    });
    let focus = std::cell::Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        if focus.replace(false) {
            form.url.focus_handle(cx).focus(window, cx);
        }
        let body = div()
            .flex()
            .flex_col()
            .gap_3()
            .child(form_row(
                "URL",
                Some("Where the server listens for sync clients."),
                Input::new(&form.url),
                cx,
            ))
            .child(form_row(
                "Token",
                Some("An access token from the server's Tokens page, if it needs one."),
                Input::new(&form.token),
                cx,
            ));
        let save_form = form.clone();
        let footer =
            DialogFooter::new()
                .child(
                    Button::new("cancel")
                        .label("Cancel")
                        .on_click(|_, window, cx| window.close_dialog(cx)),
                )
                .child(Button::new("save").label("Save").primary().on_click(
                    move |_, window, cx| {
                        let url = save_form.url.read(cx).value().trim().to_owned();
                        let token = save_form.token.read(cx).value().trim().to_owned();
                        match save_server(url, token, cx) {
                            Ok(()) => window.close_dialog(cx),
                            Err(error) => {
                                show_error("The server couldn't be saved.", &error, window, cx)
                            }
                        }
                    },
                ));
        dialog
            .title(SharedString::from("Catalog Server"))
            .w(px(480.))
            .child(body)
            .footer(footer)
    });
}

/// Sets the server (none if `url` is empty) and its token, then syncs the
/// open libraries with it.
fn save_server(url: String, token: String, cx: &mut App) -> anyhow::Result<()> {
    if !url.is_empty() && !(url.starts_with("http://") || url.starts_with("https://")) {
        anyhow::bail!("The URL should start with https:// (or http://).");
    }
    if token.is_empty() || url.is_empty() {
        crate::keychain::delete(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT)?;
    } else {
        crate::keychain::set(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT, token.as_bytes())?;
    }
    let syncing = state(cx);
    syncing.token = Some(Some(token).filter(|token| !token.is_empty()));
    syncing.last.clear();
    settings::set_catalog_server(Some(url).filter(|url| !url.is_empty()), cx);
    sync_open_libraries(cx);
    Ok(())
}

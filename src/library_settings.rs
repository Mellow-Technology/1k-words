//! The Libraries page in Settings: each library's location, photo count and
//! size, with buttons to move or delete it, and the targets it syncs to (see
//! `sync_settings`).
//!
//! Sizes are worked out in the background (see `refresh_stats`), and kept
//! app-wide with any move or delete in progress, so the page can show them.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, Sizable};
use gpui_kit::{
    App, AppContext as _, Div, Global, IntoElement, ParentElement, SharedString, Styled, div,
};

use crate::library::{self, LibraryInfo, LibraryStats, MoveProgress};
use crate::navigation::LibraryCallback;

/// How often the page redraws while a library's being moved, to show
/// progress.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

enum Stats {
    Loading,
    Ready(LibraryStats),
    /// Such as when the library's folder is on a disk that isn't connected.
    Unavailable(SharedString),
}

/// A move or delete in progress.
struct Busy {
    /// "Moving", say.
    verb: &'static str,
    progress: Arc<MoveProgress>,
}

#[derive(Default)]
struct LibraryActivity {
    stats: HashMap<String, Stats>,
    busy: HashMap<String, Busy>,
}

impl Global for LibraryActivity {}

fn activity(cx: &mut App) -> &mut LibraryActivity {
    cx.default_global::<LibraryActivity>()
}

/// What the page's buttons do, for the library with the given id.
pub struct LibraryActions {
    pub move_library: LibraryCallback,
    pub delete_library: LibraryCallback,
}

/// Works out every library's photo count and size again, in the
/// background.
pub fn refresh_stats(cx: &mut App) {
    let handles: Vec<_> = library::list(cx)
        .iter()
        .filter_map(|info| library::handle(&info.id, cx))
        .collect();
    activity(cx)
        .stats
        .retain(|id, _| handles.iter().any(|h| h.info.id == *id));
    for handle in handles {
        let id = handle.info.id.clone();
        crate::sync_settings::refresh_library(&id, cx);
        // Keep showing the last figures until the new ones are in.
        activity(cx)
            .stats
            .entry(id.clone())
            .or_insert(Stats::Loading);
        cx.spawn(async move |cx| {
            let result = cx.background_spawn(async move { handle.stats() }).await;
            cx.update(|cx| {
                let stats = match result {
                    Ok(stats) => Stats::Ready(stats),
                    Err(error) => Stats::Unavailable(format!("{error:#}").into()),
                };
                activity(cx).stats.insert(id, stats);
                cx.refresh_windows();
            });
        })
        .detach();
    }
}

/// Notes that a library's being moved or deleted (`verb` says which), until
/// `finish`, returning the progress to update.
pub fn start(id: &str, verb: &'static str, cx: &mut App) -> Arc<MoveProgress> {
    let progress = Arc::new(MoveProgress::default());
    activity(cx).busy.insert(
        id.to_owned(),
        Busy {
            verb,
            progress: progress.clone(),
        },
    );
    let id = id.to_owned();
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PROGRESS_INTERVAL).await;
            let busy = cx.update(|cx| {
                cx.refresh_windows();
                activity(cx).busy.contains_key(&id)
            });
            if !busy {
                break;
            }
        }
    })
    .detach();
    cx.refresh_windows();
    progress
}

pub fn finish(id: &str, cx: &mut App) {
    activity(cx).busy.remove(id);
    cx.refresh_windows();
}

/// Whether any library is being moved or deleted.
pub fn is_busy(cx: &App) -> bool {
    cx.try_global::<LibraryActivity>()
        .is_some_and(|activity| !activity.busy.is_empty())
}

/// The library's photo count, if it's known.
pub fn photo_count(id: &str, cx: &App) -> Option<usize> {
    match cx.try_global::<LibraryActivity>()?.stats.get(id)? {
        Stats::Ready(stats) => Some(stats.photos),
        _ => None,
    }
}

pub fn page(actions: LibraryActions, cx: &App) -> SettingPage {
    let current = library::current(cx).map(|library| library.info.id.clone());
    let actions = Rc::new(actions);
    SettingPage::new("Libraries")
        .icon(Icon::new(LucideIcon::Images))
        .resettable(false)
        .groups(
            library::list(cx).iter().map(|info| {
                library_group(info, current.as_deref() == Some(&info.id), &actions, cx)
            }),
        )
}

fn library_group(
    info: &LibraryInfo,
    is_open: bool,
    actions: &Rc<LibraryActions>,
    cx: &App,
) -> SettingGroup {
    let title = if is_open {
        format!("{} (Open)", info.name)
    } else {
        info.name.clone()
    };
    let path = info.path.clone();
    let location = SettingItem::new(
        "Location",
        SettingField::render(move |_, _, _| {
            let path = path.clone();
            Button::new("reveal")
                .label(reveal_label())
                .small()
                .on_click(move |_, _, cx| cx.reveal_path(&path))
        }),
    )
    .description(info.path.display().to_string());

    let id = info.id.clone();
    let photos = SettingItem::new(
        "Photos",
        SettingField::render(move |_, _, cx| {
            let text = match stats(&id, cx) {
                Some(Stats::Ready(stats)) => format_count(stats.photos),
                Some(Stats::Unavailable(_)) => "—".into(),
                Some(Stats::Loading) | None => "Counting…".into(),
            };
            value_text(text, cx)
        }),
    );

    let id = info.id.clone();
    let size = SettingItem::new(
        "Size on Disk",
        SettingField::render(move |_, _, cx| {
            let text = match stats(&id, cx) {
                Some(Stats::Ready(stats)) => format_size(stats.bytes),
                Some(Stats::Unavailable(reason)) => format!("Not available: {reason}"),
                Some(Stats::Loading) | None => "Calculating…".into(),
            };
            value_text(text, cx)
        }),
    );

    let (id, move_actions) = (info.id.clone(), actions.clone());
    let move_item = SettingItem::new(
        "Move",
        SettingField::render(move |_, _, cx| {
            if let Some(busy) = busy(&id, cx) {
                return value_text(busy_text(busy), cx).into_any_element();
            }
            let (id, actions) = (id.clone(), move_actions.clone());
            Button::new("move")
                .label("Move…")
                .small()
                .disabled(is_busy(cx))
                .on_click(move |_, window, cx| (actions.move_library)(&id, window, cx))
                .into_any_element()
        }),
    )
    .description("Move the library's folder, photos and all, to another folder or disk.");

    let (id, delete_actions) = (info.id.clone(), actions.clone());
    let delete_item = SettingItem::new(
        "Delete",
        SettingField::render(move |_, _, cx| {
            let (id, actions) = (id.clone(), delete_actions.clone());
            Button::new("delete")
                .label("Delete…")
                .small()
                .danger()
                .disabled(is_busy(cx))
                .on_click(move |_, window, cx| (actions.delete_library)(&id, window, cx))
        }),
    )
    .description("Move the library's folder to the Trash, and remove it from 1k Words.");

    SettingGroup::new()
        .title(title)
        .item(location)
        .item(photos)
        .item(size)
        .items(crate::sync_settings::library_items(info, cx))
        .item(move_item)
        .item(delete_item)
}

fn stats<'a>(id: &str, cx: &'a App) -> Option<&'a Stats> {
    cx.try_global::<LibraryActivity>()?.stats.get(id)
}

fn busy<'a>(id: &str, cx: &'a App) -> Option<&'a Busy> {
    cx.try_global::<LibraryActivity>()?.busy.get(id)
}

fn busy_text(busy: &Busy) -> String {
    let copied = busy.progress.copied.load(Ordering::Relaxed);
    let total = busy.progress.total.load(Ordering::Relaxed);
    if total == 0 {
        format!("{}…", busy.verb)
    } else {
        format!(
            "{}… {} of {}",
            busy.verb,
            format_size(copied),
            format_size(total)
        )
    }
}

fn value_text(text: impl Into<SharedString>, cx: &App) -> Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn reveal_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "Show in Finder"
    } else {
        "Show Folder"
    }
}

/// `12,345`.
pub fn format_count(count: usize) -> String {
    let digits = count.to_string();
    let mut text = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

/// In decimal units, as Finder shows them: `1.2 GB`.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["bytes", "KB", "MB", "GB", "TB"];
    if bytes < 1000 {
        return format!("{bytes} bytes");
    }
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1000. && unit < UNITS.len() - 1 {
        size /= 1000.;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_counts_and_sizes() {
        assert_eq!(format_count(0), "0");
        assert_eq!(format_count(999), "999");
        assert_eq!(format_count(12_345), "12,345");
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_size(999), "999 bytes");
        assert_eq!(format_size(1_500), "1.5 KB");
        assert_eq!(format_size(2_340_000_000), "2.3 GB");
    }
}

//! Sync in Settings.
//!
//! The Sync page lists the targets libraries can sync to (see
//! `sync_targets`), with a dialog to add or edit one from its plugin's
//! settings. Each library's group on the Libraries page lists the targets it
//! syncs to, how far along each is, and buttons to sync now or stop syncing
//! there (see `library_items`).
//!
//! What each library syncs to, and uploads in progress, are kept app-wide so
//! the pages can show them.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, Sizable, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, Entity, Focusable as _, Global, ParentElement, PromptLevel, SharedString,
    Styled, Task, Window, div, px,
};

use crate::config_form::{ConfigForm, form_row};
use crate::gallery::{show_error, show_warning};
use crate::library::{
    self, DownloadSource, DownloadSummary, Library, LibraryInfo, LibraryTarget, SyncStatus,
    TargetSettings, UploadProgress, UploadSummary,
};
use crate::photos::Photo;
use crate::plugins::{Config, FieldKind, Plugin, Plugins};
use crate::settings;
use crate::sync_targets::{self, NewTarget, TargetInfo};

/// How often the pages redraw while uploading, to show progress.
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);
/// The upload speed shown is the average over this long.
const SPEED_WINDOW: Duration = Duration::from_secs(3);

/// An upload in progress, updated from the background as it goes (see
/// `UploadProgress`).
struct Upload {
    /// The target's id in `sync_targets`.
    target: String,
    photos_done: AtomicUsize,
    photos_total: AtomicUsize,
    /// The size of the files done, and of all of them.
    bytes_done: AtomicU64,
    bytes_total: AtomicU64,
    /// The size of the files and parts under way, and `sent` when that
    /// was last reported.
    file_size: AtomicU64,
    file_start: AtomicU64,
    /// Bytes sent over the network.
    sent: Arc<AtomicU64>,
    cancel: AtomicBool,
}

impl Upload {
    fn new(target: String) -> Self {
        Self {
            target,
            photos_done: AtomicUsize::new(0),
            photos_total: AtomicUsize::new(0),
            bytes_done: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            file_size: AtomicU64::new(0),
            file_start: AtomicU64::new(0),
            sent: Arc::new(AtomicU64::new(0)),
            cancel: AtomicBool::new(false),
        }
    }

    fn update(&self, progress: &UploadProgress) {
        self.photos_done
            .store(progress.photos_done, Ordering::Relaxed);
        self.photos_total
            .store(progress.photos_total, Ordering::Relaxed);
        self.bytes_done
            .store(progress.bytes_done, Ordering::Relaxed);
        self.bytes_total
            .store(progress.bytes_total, Ordering::Relaxed);
        self.file_size.store(progress.under_way, Ordering::Relaxed);
        self.file_start
            .store(self.sent.load(Ordering::Relaxed), Ordering::Relaxed);
    }

    /// Bytes uploaded so far, counting the file under way, and to upload in
    /// all.
    fn bytes(&self) -> (u64, u64) {
        let under_way = self
            .sent
            .load(Ordering::Relaxed)
            .saturating_sub(self.file_start.load(Ordering::Relaxed))
            .min(self.file_size.load(Ordering::Relaxed));
        let total = self.bytes_total.load(Ordering::Relaxed);
        let done = (self.bytes_done.load(Ordering::Relaxed) + under_way).min(total);
        (done, total)
    }
}

/// A library's sync to a target: by the library's id and its id for the
/// target.
type LinkKey = (String, library::Id);

#[derive(Default)]
struct SyncActivity {
    /// What each library syncs to, and how far along each is.
    links: HashMap<String, Vec<(LibraryTarget, SyncStatus)>>,
    uploads: HashMap<LinkKey, Arc<Upload>>,
    /// Uploads asked for again while under way, to go again once they're
    /// done.
    again: HashSet<LinkKey>,
    /// How each target's last upload went.
    results: HashMap<LinkKey, String>,
    /// Bytes sent by uploads that have finished, so the total sent only
    /// goes up.
    finished_sent: u64,
    /// The total sent over the last `SPEED_WINDOW`, for the speed.
    samples: VecDeque<(Instant, u64)>,
}

impl SyncActivity {
    fn total_sent(&self) -> u64 {
        self.finished_sent
            + self
                .uploads
                .values()
                .map(|upload| upload.sent.load(Ordering::Relaxed))
                .sum::<u64>()
    }

    /// Records how much has been sent, now.
    fn sample(&mut self, now: Instant) {
        let sent = self.total_sent();
        self.samples.push_back((now, sent));
        while self
            .samples
            .front()
            .is_some_and(|(time, _)| now.duration_since(*time) > SPEED_WINDOW)
        {
            self.samples.pop_front();
        }
    }

    /// Bytes a second, once there's been time to measure it.
    fn speed(&self) -> Option<u64> {
        let ((first_time, first), (last_time, last)) =
            (self.samples.front()?, self.samples.back()?);
        let elapsed = last_time.duration_since(*first_time).as_secs_f64();
        (elapsed >= 1.0).then(|| ((last - first) as f64 / elapsed) as u64)
    }
}

/// How all the uploads under way are going, together.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SyncProgress {
    /// Photos being synced, counting each library's once however many
    /// targets it's syncing to.
    pub photos: usize,
    pub targets: usize,
    /// 0 to 100, by bytes.
    pub percent: f32,
    /// Bytes a second, once it's known.
    pub speed: Option<u64>,
}

impl SyncProgress {
    /// "Syncing 120 photos to 2 targets · 45% · 3.2 MB/s".
    pub fn text(&self) -> String {
        let plural = |count: usize, noun: &str| {
            let s = if count == 1 { "" } else { "s" };
            format!("{} {noun}{s}", format_count(count))
        };
        let mut text = format!(
            "Syncing {} to {} · {:.0}%",
            plural(self.photos, "photo"),
            plural(self.targets, "target"),
            self.percent.floor()
        );
        if let Some(speed) = self.speed {
            text.push_str(&format!(" · {}/s", format_size(speed)));
        }
        text
    }
}

/// How the uploads under way are going, if any are.
pub fn progress(cx: &App) -> Option<SyncProgress> {
    let activity = read_activity(cx)?;
    if activity.uploads.is_empty() {
        return None;
    }
    let mut photos_by_library: HashMap<&str, usize> = HashMap::new();
    let mut targets: Vec<&str> = Vec::new();
    let (mut done, mut total) = (0, 0);
    for ((library_id, _), upload) in &activity.uploads {
        let photos = photos_by_library.entry(library_id).or_default();
        *photos = (*photos).max(upload.photos_total.load(Ordering::Relaxed));
        if !targets.contains(&upload.target.as_str()) {
            targets.push(&upload.target);
        }
        let (upload_done, upload_total) = upload.bytes();
        done += upload_done;
        total += upload_total;
    }
    Some(SyncProgress {
        photos: photos_by_library.values().sum(),
        targets: targets.len(),
        percent: if total == 0 {
            0.0
        } else {
            (done as f64 / total as f64 * 100.0) as f32
        },
        speed: activity.speed(),
    })
}

impl Global for SyncActivity {}

fn activity(cx: &mut App) -> &mut SyncActivity {
    cx.default_global::<SyncActivity>()
}

fn read_activity(cx: &App) -> Option<&SyncActivity> {
    cx.try_global::<SyncActivity>()
}

/// Reads again what a library syncs to, in the background.
pub fn refresh_library(library_id: &str, cx: &mut App) {
    let Some(handle) = library::handle(library_id, cx) else {
        activity(cx).links.remove(library_id);
        return;
    };
    let id = library_id.to_owned();
    cx.spawn(async move |cx| {
        let overview = cx
            .background_spawn(async move { handle.sync_overview() })
            .await;
        cx.update(|cx| {
            match overview {
                Ok(links) => {
                    activity(cx).links.insert(id, links);
                }
                Err(error) => eprintln!("couldn't read what a library syncs to: {error:#}"),
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Whether anything's uploading.
pub fn is_syncing(cx: &App) -> bool {
    read_activity(cx).is_some_and(|activity| !activity.uploads.is_empty())
}

/// The Sync page: the targets, and adding more.
pub fn page(cx: &App) -> SettingPage {
    SettingPage::new("Sync")
        .icon(Icon::new(LucideIcon::CloudUpload))
        .resettable(false)
        .description(
            "Places your libraries can sync to. Choose which libraries sync where on the \
             Libraries page.",
        )
        .group(
            SettingGroup::new().item(
                SettingItem::new(
                    "Sync Automatically",
                    SettingField::switch(
                        |cx| !settings::get(cx).pause_automatic_sync,
                        |on, cx| {
                            settings::set_sync_automatically(on, cx);
                            if on {
                                sync_all_in_background(cx);
                            }
                        },
                    ),
                )
                .description(
                    "Upload new photos after they're imported, and check for anything not yet \
                     uploaded every few minutes. Otherwise, libraries only sync with Sync Now.",
                ),
            ),
        )
        .groups(sync_targets::list(cx).iter().map(target_group))
        .group(add_group(cx))
}

fn target_group(target: &TargetInfo) -> SettingGroup {
    let plugin = target.plugin.clone();
    let service = SettingItem::new(
        "Service",
        SettingField::render(move |_, _, cx| {
            let name = match sync_targets::plugins(cx) {
                Ok(Some(plugins)) => plugins
                    .get(&plugin)
                    .map(|p| p.info.name.clone())
                    .unwrap_or_else(|| format!("“{plugin}” (not installed)")),
                _ => plugin.clone(),
            };
            value_text(name, cx)
        }),
    )
    .description(target.summary.clone());

    let id = target.id.clone();
    let manage = SettingItem::new(
        "Settings",
        SettingField::render(move |_, _, _| {
            let (edit_id, delete_id) = (id.clone(), id.clone());
            div()
                .flex()
                .gap_2()
                .child(
                    Button::new("edit")
                        .label("Edit…")
                        .small()
                        .on_click(move |_, window, cx| edit_target(&edit_id, window, cx)),
                )
                .child(
                    Button::new("delete")
                        .label("Delete…")
                        .small()
                        .danger()
                        .on_click(move |_, window, cx| delete_target(&delete_id, window, cx)),
                )
        }),
    );
    SettingGroup::new()
        .title(target.name.clone())
        .item(service)
        .item(manage)
}

fn add_group(cx: &App) -> SettingGroup {
    let group = SettingGroup::new().title("Add a Target");
    let plugins = match sync_targets::plugins(cx) {
        Ok(Some(plugins)) => plugins,
        Ok(None) => {
            return group.item(SettingItem::new(
                "Loading…",
                SettingField::render(|_, _, _| div()),
            ));
        }
        Err(error) => {
            return group.item(
                SettingItem::new(
                    "Sync isn't available",
                    SettingField::render(|_, _, _| div()),
                )
                .description(format!("The sync plugins couldn't be loaded: {error}")),
            );
        }
    };
    group.items(plugins.all().iter().map(|plugin| {
        let plugin_id = plugin.info.id.clone();
        SettingItem::new(
            plugin.info.name.clone(),
            SettingField::render(move |_, _, _| {
                let plugin_id = plugin_id.clone();
                Button::new("add")
                    .label("Add…")
                    .small()
                    .on_click(move |_, window, cx| add_target(&plugin_id, window, cx))
            }),
        )
    }))
}

fn loaded_plugins(window: &mut Window, cx: &mut App) -> Option<Arc<Plugins>> {
    match sync_targets::plugins(cx) {
        Ok(Some(plugins)) => Some(plugins),
        Ok(None) => {
            show_warning(
                "Sync is still starting.",
                "Try again in a moment.",
                window,
                cx,
            );
            None
        }
        Err(error) => {
            show_warning(
                "Sync isn't available.",
                &format!("The sync plugins couldn't be loaded: {error}"),
                window,
                cx,
            );
            None
        }
    }
}

fn add_target(plugin_id: &str, window: &mut Window, cx: &mut App) {
    let Some(plugins) = loaded_plugins(window, cx) else {
        return;
    };
    if let Some(plugin) = plugins.get(plugin_id).cloned() {
        open_target_dialog(plugins, plugin, None, Vec::new(), window, cx);
    }
}

fn edit_target(id: &str, window: &mut Window, cx: &mut App) {
    let Some(plugins) = loaded_plugins(window, cx) else {
        return;
    };
    let Some(target) = sync_targets::get(id, cx) else {
        return;
    };
    let Some(plugin) = plugins.get(&target.plugin).cloned() else {
        show_warning(
            "This target can't be edited.",
            &format!("Its plugin, “{}”, isn't installed.", target.plugin),
            window,
            cx,
        );
        return;
    };
    match sync_targets::config(&target) {
        Ok(config) => {
            open_target_dialog(plugins, plugin, Some(target), config, window, cx);
        }
        Err(error) => show_error(
            "The target's settings couldn't be read.",
            &error,
            window,
            cx,
        ),
    }
}

/// Asks to confirm, then deletes a target, and every library's record of
/// what it synced there.
fn delete_target(id: &str, window: &mut Window, cx: &mut App) {
    if is_syncing(cx) || crate::library_settings::is_busy(cx) {
        show_warning(
            "Please wait until syncing has finished.",
            "Targets can't be deleted while libraries are syncing, moving or being deleted.",
            window,
            cx,
        );
        return;
    }
    let Some(target) = sync_targets::get(id, cx) else {
        return;
    };
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Delete “{}”?", target.name),
        Some(
            "Libraries will stop syncing to it, and forget what they've synced there. \
             What's been uploaded stays on the service.",
        ),
        &["Delete", "Cancel"],
        cx,
    );
    let id = id.to_owned();
    window
        .spawn(cx, async move |cx| {
            if answer.await != Ok(0) {
                return;
            }
            let Ok(handles) = cx.update(|_, cx| {
                library::list(cx)
                    .iter()
                    .filter_map(|info| library::handle(&info.id, cx))
                    .collect::<Vec<_>>()
            }) else {
                return;
            };
            let target = id.clone();
            let forgotten = cx
                .background_spawn(async move {
                    handles
                        .iter()
                        .try_for_each(|handle| handle.forget_sync_target(&target))
                })
                .await;
            cx.update(
                |window, cx| match forgotten.and_then(|()| sync_targets::delete(&id, cx)) {
                    Ok(()) => {
                        let ids: Vec<String> = library::list(cx)
                            .iter()
                            .map(|info| info.id.clone())
                            .collect();
                        for library_id in ids {
                            refresh_library(&library_id, cx);
                        }
                    }
                    Err(error) => show_error("The target couldn't be deleted.", &error, window, cx),
                },
            )
            .ok();
        })
        .detach();
}

/// The target dialog's state.
struct TargetForm {
    plugins: Arc<Plugins>,
    plugin: Arc<Plugin>,
    /// The target being edited, if it's not a new one.
    id: Option<String>,
    name: Entity<InputState>,
    fields: ConfigForm,
    /// While the plugin checks the settings.
    saving: bool,
    error: Option<String>,
}

impl TargetForm {
    /// What the target's shown with besides its name: the plugin's name, and
    /// the URLs it's set to use.
    fn summary(&self, config: &Config) -> String {
        let urls = self
            .plugin
            .info
            .config
            .iter()
            .filter(|field| field.kind == FieldKind::Url)
            .filter_map(|field| config.iter().find(|(key, _)| *key == field.key))
            .map(|(_, url)| url.as_str());
        std::iter::once(self.plugin.info.name.as_str())
            .chain(urls)
            .collect::<Vec<_>>()
            .join(" · ")
    }
}

/// Opens the dialog to add a target for `plugin`, or edit `existing` (whose
/// settings are `config`). Returns its state.
fn open_target_dialog(
    plugins: Arc<Plugins>,
    plugin: Arc<Plugin>,
    existing: Option<TargetInfo>,
    config: Config,
    window: &mut Window,
    cx: &mut App,
) -> Rc<RefCell<TargetForm>> {
    let name = existing
        .as_ref()
        .map_or_else(|| plugin.info.name.clone(), |t| t.name.clone());
    let name = cx.new(|cx| InputState::new(window, cx).default_value(name));
    let fields = ConfigForm::new(&plugin.info.config, &config, window, cx);
    let title = match &existing {
        Some(target) => format!("Edit “{}”", target.name),
        None => format!("Add {}", plugin.info.name),
    };
    let form = Rc::new(RefCell::new(TargetForm {
        plugins,
        plugin,
        id: existing.map(|target| target.id),
        name,
        fields,
        saving: false,
        error: None,
    }));
    let focus_name = Cell::new(true);
    let dialog_form = form.clone();
    window.open_dialog(cx, move |dialog, window, cx| {
        let form = &dialog_form;
        if focus_name.replace(false) {
            let name = form.borrow().name.clone();
            name.focus_handle(cx).focus(window, cx);
        }
        let state = form.borrow();
        let mut body = div().flex().flex_col().gap_3().child(form_row(
            "Name",
            None,
            Input::new(&state.name),
            cx,
        ));
        body = body.children(state.fields.rows(cx));
        if let Some(error) = &state.error {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        let save_form = form.clone();
        let footer = DialogFooter::new()
            .child(
                Button::new("cancel")
                    .label("Cancel")
                    .on_click(|_, window, cx| window.close_dialog(cx)),
            )
            .child(
                Button::new("save")
                    .label(if state.id.is_some() { "Save" } else { "Add" })
                    .primary()
                    .loading(state.saving)
                    .disabled(state.saving)
                    .on_click(move |_, window, cx| save_target(save_form.clone(), window, cx)),
            );
        dialog
            .title(SharedString::from(title.clone()))
            .w(px(480.))
            .child(body)
            .footer(footer)
    });
    form
}

/// Has the plugin check the dialog's settings in the background, then saves
/// the target and closes the dialog, or shows why not.
fn save_target(form: Rc<RefCell<TargetForm>>, window: &mut Window, cx: &mut App) {
    let (plugins, plugin_id, config) = {
        let mut state = form.borrow_mut();
        state.saving = true;
        state.error = None;
        (
            state.plugins.clone(),
            state.plugin.info.id.clone(),
            state.fields.config(cx),
        )
    };
    window.refresh();
    let check =
        cx.background_spawn(async move { sync_targets::validate(&plugins, &plugin_id, config) });
    window
        .spawn(cx, async move |cx| {
            let result = check.await;
            cx.update(|window, cx| {
                let saved = result.and_then(|config| {
                    let state = form.borrow();
                    let target = NewTarget {
                        name: state.name.read(cx).value().to_string(),
                        plugin: state.plugin.info.id.clone(),
                        summary: state.summary(&config),
                        config,
                    };
                    sync_targets::save(state.id.as_deref(), target, cx)
                });
                let mut state = form.borrow_mut();
                state.saving = false;
                match saved {
                    Ok(_) => {
                        drop(state);
                        window.close_dialog(cx);
                    }
                    Err(error) => state.error = Some(format!("{error:#}")),
                }
                window.refresh();
            })
            .ok();
        })
        .detach();
}

/// The Libraries page's items for a library's syncing: one per target it
/// syncs to, then a way to add another.
pub fn library_items(info: &LibraryInfo, cx: &App) -> Vec<SettingItem> {
    let links = read_activity(cx)
        .and_then(|activity| activity.links.get(&info.id))
        .cloned()
        .unwrap_or_default();
    let mut items: Vec<SettingItem> = links
        .iter()
        .map(|(link, status)| link_item(&info.id, link, *status, cx))
        .collect();

    let linked: Vec<String> = links.iter().map(|(link, _)| link.target.clone()).collect();
    let available: Vec<(SharedString, SharedString)> = sync_targets::list(cx)
        .iter()
        .filter(|target| !linked.contains(&target.id))
        .map(|target| (target.id.clone().into(), target.name.clone().into()))
        .collect();
    let description = if sync_targets::list(cx).is_empty() {
        "Add places to sync to on the Sync page."
    } else if available.is_empty() {
        "The library syncs to every target."
    } else {
        "Upload the library's photos to another target."
    };
    let library_id = info.id.clone();
    let mut options = vec![(SharedString::default(), "Choose…".into())];
    options.extend(available);
    items.push(
        SettingItem::new(
            "Sync To",
            SettingField::dropdown(
                options,
                |_| SharedString::default(),
                move |target, cx| {
                    if !target.is_empty() {
                        link_target(&library_id, &target, cx);
                    }
                },
            ),
        )
        .description(description),
    );
    items
}

fn link_item(library_id: &str, link: &LibraryTarget, status: SyncStatus, cx: &App) -> SettingItem {
    let name = sync_targets::get(&link.target, cx)
        .map_or_else(|| "Deleted target".to_owned(), |target| target.name);
    let key: LinkKey = (library_id.to_owned(), link.id);
    let activity = read_activity(cx);
    let upload = activity.and_then(|a| a.uploads.get(&key)).cloned();
    let result = activity.and_then(|a| a.results.get(&key)).cloned();
    let description = match (&upload, &link.error) {
        (Some(upload), _) => {
            let (done, total) = upload.bytes();
            format!(
                "Uploading… {} of {} photos ({}%)",
                format_count(upload.photos_done.load(Ordering::Relaxed)),
                format_count(upload.photos_total.load(Ordering::Relaxed)),
                (done * 100).checked_div(total).unwrap_or(0)
            )
        }
        (None, Some(error)) => format!("Stopped: {error}"),
        (None, None) => {
            let mut text = status_text(status);
            if let Some(result) = result {
                text.push_str(&format!(" {result}"));
            }
            text
        }
    };
    let link = link.clone();
    let library_id = library_id.to_owned();
    SettingItem::new(
        name,
        SettingField::render(move |_, _, _| {
            let buttons = div().flex().gap_2();
            let (library_id, link) = (library_id.clone(), link.clone());
            if let Some(upload) = &upload {
                let upload = upload.clone();
                return buttons.child(
                    Button::new("stop")
                        .label("Stop")
                        .small()
                        .on_click(move |_, _, _| upload.cancel.store(true, Ordering::Relaxed)),
                );
            }
            let (remove_library, remove_link) = (library_id.clone(), link.clone());
            buttons
                .child(
                    Button::new("sync")
                        .label("Sync Now")
                        .small()
                        .on_click(move |_, window, cx| sync_now(&library_id, &link, window, cx)),
                )
                .child(Button::new("remove").label("Remove…").small().on_click(
                    move |_, window, cx| unlink_target(&remove_library, &remove_link, window, cx),
                ))
        }),
    )
    .description(description)
}

fn status_text(status: SyncStatus) -> String {
    let total = status.synced + status.pending + status.rejected;
    let mut text = format!(
        "{} of {} files uploaded.",
        format_count(status.synced),
        format_count(total)
    );
    if status.rejected > 0 {
        text.push_str(&format!(
            " {} refused by the service.",
            format_count(status.rejected)
        ));
    }
    text
}

fn link_target(library_id: &str, target: &str, cx: &mut App) {
    let added = library::get(library_id, cx).and_then(|library| library.add_sync_target(target));
    if let Err(error) = added {
        eprintln!("couldn't add the sync target: {error:#}");
    }
    refresh_library(library_id, cx);
}

/// Asks to confirm, then stops a library syncing to a target.
fn unlink_target(library_id: &str, link: &LibraryTarget, window: &mut Window, cx: &mut App) {
    let name = sync_targets::get(&link.target, cx).map_or_else(String::new, |t| t.name);
    let answer = window.prompt(
        PromptLevel::Warning,
        &format!("Stop syncing to “{name}”?"),
        Some(
            "The library will forget what it's uploaded there; adding the target again uploads \
             everything again. What's been uploaded stays on the service.",
        ),
        &["Stop Syncing", "Cancel"],
        cx,
    );
    let (library_id, link_id) = (library_id.to_owned(), link.id);
    window
        .spawn(cx, async move |cx| {
            if answer.await != Ok(0) {
                return;
            }
            cx.update(|window, cx| {
                let removed = library::get(&library_id, cx)
                    .and_then(|library| library.remove_sync_target(link_id));
                if let Err(error) = removed {
                    show_error("The target couldn't be removed.", &error, window, cx);
                }
                let key = (library_id.clone(), link_id);
                activity(cx).results.remove(&key);
                refresh_library(&library_id, cx);
            })
            .ok();
        })
        .detach();
}

/// Uploads what a library hasn't yet to a target, in the background.
fn sync_now(library_id: &str, link: &LibraryTarget, window: &mut Window, cx: &mut App) {
    if crate::library_settings::is_busy(cx) {
        show_warning(
            "Please wait until the library is free.",
            "A library is being moved or deleted.",
            window,
            cx,
        );
        return;
    }
    let Some(plugins) = loaded_plugins(window, cx) else {
        return;
    };
    if sync_targets::get(&link.target, cx).is_none() {
        show_warning(
            "This target has been deleted.",
            "Remove it from the library.",
            window,
            cx,
        );
        return;
    }
    let library = match library::get(library_id, cx) {
        Ok(library) => library,
        Err(error) => {
            show_error("The library couldn't be opened.", &error, window, cx);
            return;
        }
    };
    start_upload(library, link, plugins, cx);
}

/// How often libraries are synced without being asked (see
/// `start_automatic_sync`).
const AUTOMATIC_SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// How long after starting up the first automatic sync waits, so it doesn't
/// slow the app down as it opens.
const AUTOMATIC_SYNC_DELAY: Duration = Duration::from_secs(60);

/// Syncs every library to its targets from time to time, while the
/// setting's on. Call once, at startup.
pub fn start_automatic_sync(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(AUTOMATIC_SYNC_DELAY).await;
        loop {
            cx.update(sync_all_in_background);
            cx.background_executor()
                .timer(AUTOMATIC_SYNC_INTERVAL)
                .await;
        }
    })
    .detach();
}

/// Syncs every library that has targets, if the setting's on. Which do is
/// read in the background, so libraries without any aren't opened.
fn sync_all_in_background(cx: &mut App) {
    if settings::get(cx).pause_automatic_sync {
        return;
    }
    let handles: Vec<_> = library::list(cx)
        .iter()
        .filter_map(|info| library::handle(&info.id, cx))
        .collect();
    cx.spawn(async move |cx| {
        let with_targets = cx
            .background_spawn(async move {
                handles
                    .into_iter()
                    .filter(|handle| handle.sync_overview().is_ok_and(|links| !links.is_empty()))
                    .map(|handle| handle.info.id)
                    .collect::<Vec<_>>()
            })
            .await;
        cx.update(|cx| {
            for id in with_targets {
                sync_in_background(&id, cx);
            }
        });
    })
    .detach();
}

/// Uploads what a library hasn't yet to each of its targets, if the
/// setting's on, such as after photos are imported into it. Unlike Sync
/// Now, nothing's shown if it can't: it's tried again later. Targets that
/// stopped for a reason only the user can fix, such as bad credentials,
/// wait for Sync Now.
pub fn sync_in_background(library_id: &str, cx: &mut App) {
    if settings::get(cx).pause_automatic_sync || crate::library_settings::is_busy(cx) {
        return;
    }
    let Ok(Some(plugins)) = sync_targets::plugins(cx) else {
        return;
    };
    // The library's folder may be on a drive that isn't connected.
    let Ok(library) = library::get(library_id, cx) else {
        return;
    };
    let links = match library.sync_targets() {
        Ok(links) => links,
        Err(error) => {
            eprintln!("couldn't read what a library syncs to: {error:#}");
            return;
        }
    };
    for link in links {
        if link.error.is_none() && sync_targets::get(&link.target, cx).is_some() {
            start_upload(library.clone(), &link, plugins.clone(), cx);
        }
    }
}

/// Starts uploading a library to a target. If it's uploading there already,
/// it goes through the library again afterwards, for photos added since.
fn start_upload(
    library: Arc<library::Library>,
    link: &LibraryTarget,
    plugins: Arc<Plugins>,
    cx: &mut App,
) {
    let Some(target) = sync_targets::get(&link.target, cx) else {
        return;
    };
    let key: LinkKey = (library.info.id.clone(), link.id);
    let upload = Arc::new(Upload::new(link.target.clone()));
    let syncing = activity(cx);
    if syncing.uploads.contains_key(&key) {
        syncing.again.insert(key);
        return;
    }
    syncing.uploads.insert(key.clone(), upload.clone());
    syncing.results.remove(&key);
    start_ticking(cx);

    let link_id = link.id;
    let progress = upload.clone();
    let task = cx.background_spawn(async move {
        let settings = TargetSettings {
            config: sync_targets::config(&target)?,
            plugin: target.plugin,
        };
        let uploaded = library.upload(
            &plugins,
            link_id,
            &settings,
            &progress.cancel,
            &progress.sent,
            |p| progress.update(&p),
        )?;
        let mut downloaded = DownloadSummary::default();
        if uploaded.stopped.is_none() {
            // Thumbnails this device lacks, such as those of originals it's
            // let go of, and what pinned albums have gained since. Other
            // originals only come when they're asked for.
            let cancel = &progress.cancel;
            downloaded = library.download_derivatives(&plugins, link_id, &settings, cancel)?;
            let source = DownloadSource {
                id: link_id,
                settings,
            };
            downloaded.add(library.download_pinned(&plugins, &[source], cancel)?);
        }
        anyhow::Ok((uploaded, downloaded))
    });
    cx.spawn(async move |cx| {
        let result = task.await;
        cx.update(|cx| {
            let activity = activity(cx);
            let cancelled = match activity.uploads.remove(&key) {
                Some(upload) => {
                    activity.finished_sent += upload.sent.load(Ordering::Relaxed);
                    upload.cancel.load(Ordering::Relaxed)
                }
                None => false,
            };
            let again = activity.again.remove(&key) && !cancelled && result.is_ok();
            activity.results.insert(key.clone(), result_text(result));
            refresh_library(&key.0, cx);
            if again {
                sync_in_background(&key.0, cx);
            }
        });
    })
    .detach();
}

/// What's needed to download from a library's targets, in the background.
/// Fails if it can't be done now.
fn download_setup(cx: &App) -> Result<(Arc<Plugins>, Vec<TargetInfo>)> {
    if crate::library_settings::is_busy(cx) {
        bail!("A library is being moved or deleted.");
    }
    let plugins = sync_targets::plugins(cx)
        .map_err(|error| anyhow!(error))?
        .context("The sync plugins are still loading.")?;
    Ok((plugins, sync_targets::list(cx).to_vec()))
}

/// The targets of `library` that are set up on this device. Blocks on the
/// keychain.
fn download_sources(library: &Library, targets: &[TargetInfo]) -> Result<Vec<DownloadSource>> {
    let mut sources = Vec::new();
    for link in library.sync_targets()? {
        let Some(target) = targets.iter().find(|target| target.id == link.target) else {
            continue;
        };
        match sync_targets::config(target) {
            Ok(config) => sources.push(DownloadSource {
                id: link.id,
                settings: TargetSettings {
                    plugin: target.plugin.clone(),
                    config,
                },
            }),
            Err(error) => eprintln!("couldn't read a target's settings: {error:#}"),
        }
    }
    Ok(sources)
}

/// Fetches what a photo's missing of its originals, from whichever of its
/// library's targets has them. Unlike syncing, this is done even while
/// automatic sync is paused: the user's asked for it.
pub fn fetch_original(library: Arc<Library>, photo: Photo, cx: &App) -> Task<Result<()>> {
    let setup = download_setup(cx);
    cx.background_spawn(async move {
        let (plugins, targets) = setup?;
        let sources = download_sources(&library, &targets)?;
        library.fetch_original(&plugins, &sources, &photo, &AtomicBool::new(false))
    })
}

/// Downloads what a library's pinned photos and albums need.
pub fn download_pinned(library: Arc<Library>, cx: &App) -> Task<Result<DownloadSummary>> {
    let setup = download_setup(cx);
    cx.background_spawn(async move {
        let (plugins, targets) = setup?;
        let sources = download_sources(&library, &targets)?;
        library.download_pinned(&plugins, &sources, &AtomicBool::new(false))
    })
}

/// How a sync went, for after the target's status.
fn result_text(result: anyhow::Result<(UploadSummary, DownloadSummary)>) -> String {
    match result {
        Err(error) => format!("The last sync failed: {error:#}"),
        Ok((summary, downloaded)) => {
            let mut text = match summary.uploaded {
                0 => "Nothing new to upload.".to_owned(),
                1 => "Uploaded 1 file.".to_owned(),
                n => format!("Uploaded {} files.", format_count(n)),
            };
            match summary.failed.len() {
                0 => {}
                1 => text.push_str(&format!(" 1 failed: {}", summary.failed[0].1)),
                n => text.push_str(&format!(
                    " {} failed, the last with: {}",
                    format_count(n),
                    summary.failed[n - 1].1
                )),
            }
            if let Some(stopped) = summary.stopped {
                text.push_str(&format!(" {stopped}"));
            }
            match downloaded.downloaded {
                0 => {}
                1 => text.push_str(" Downloaded 1 file."),
                n => text.push_str(&format!(" Downloaded {} files.", format_count(n))),
            }
            match downloaded.failed.len() {
                0 => {}
                1 => text.push_str(&format!(" 1 download failed: {}", downloaded.failed[0].1)),
                n => text.push_str(&format!(
                    " {} downloads failed, the last with: {}",
                    format_count(n),
                    downloaded.failed[n - 1].1
                )),
            }
            if let Some(stopped) = downloaded.stopped {
                text.push_str(&format!(" {stopped}"));
            }
            text
        }
    }
}

/// Redraws while anything's uploading, measuring the speed as it goes.
fn start_ticking(cx: &mut App) {
    cx.spawn(async move |cx| {
        loop {
            cx.background_executor().timer(PROGRESS_INTERVAL).await;
            let syncing = cx.update(|cx| {
                cx.refresh_windows();
                let activity = activity(cx);
                if activity.uploads.is_empty() {
                    activity.samples.clear();
                    false
                } else {
                    activity.sample(Instant::now());
                    true
                }
            });
            if !syncing {
                break;
            }
        }
    })
    .detach();
}

fn value_text(text: impl Into<SharedString>, cx: &App) -> gpui_kit::Div {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child(text.into())
}

fn format_count(count: usize) -> String {
    crate::library_settings::format_count(count)
}

fn format_size(bytes: u64) -> String {
    crate::library_settings::format_size(bytes)
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        Bounds, IntoElement, Render, TestAppContext, WindowBounds, WindowOptions, point, size,
    };

    use super::*;
    use crate::library::sync_test_support::FakeS3;

    struct Blank;

    impl Render for Blank {
        fn render(&mut self, _: &mut Window, _: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            div()
        }
    }

    /// Shows one settings page.
    struct Page(fn(&App) -> SettingPage);

    impl Render for Page {
        fn render(&mut self, _: &mut Window, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            gpui_kit::component::setting::Settings::new("page").page((self.0)(cx))
        }
    }

    fn libraries_page(cx: &App) -> SettingPage {
        let actions = crate::library_settings::LibraryActions {
            move_library: Rc::new(|_, _, _| {}),
            delete_library: Rc::new(|_, _, _| {}),
        };
        crate::library_settings::page(actions, cx)
    }

    /// Draws `page` in a window of its own.
    fn draw(page: fn(&App) -> SettingPage, cx: &mut TestAppContext) {
        let window = cx
            .update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Page(page)))
            })
            .unwrap()
            .0;
        cx.run_until_parked();
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
    }

    /// Fills in the dialog's text fields from `values`, then saves.
    fn fill_and_save(
        form: &Rc<RefCell<TargetForm>>,
        values: &[(&str, &str)],
        window: gpui_kit::AnyWindowHandle,
        cx: &mut TestAppContext,
    ) {
        cx.update_window(window, |_, window, cx| {
            form.borrow().fields.fill(values, window, cx);
            save_target(form.clone(), window, cx);
        })
        .unwrap();
        cx.run_until_parked();
    }

    /// An upload of `photos` photos, `total` bytes, as if `done` bytes of
    /// files are done and `under_way` bytes of the next have been sent.
    fn upload(target: &str, photos: usize, total: u64, done: u64, under_way: u64) -> Arc<Upload> {
        let upload = Upload::new(target.into());
        upload.update(&UploadProgress {
            photos_done: 0,
            photos_total: photos,
            bytes_done: done,
            bytes_total: total,
            path: "",
            under_way: 100,
        });
        upload.sent.store(under_way, Ordering::Relaxed);
        Arc::new(upload)
    }

    #[test]
    fn counts_bytes_under_way_up_to_the_files_size() {
        assert_eq!(upload("a", 1, 1000, 200, 50).bytes(), (250, 1000));
        // A retried file's bytes aren't counted twice.
        assert_eq!(upload("a", 1, 1000, 200, 150).bytes(), (300, 1000));
        assert_eq!(upload("a", 1, 250, 200, 100).bytes(), (250, 250));
    }

    #[test]
    fn measures_the_speed_over_a_few_seconds() {
        let mut activity = SyncActivity::default();
        let upload = upload("a", 1, 1000, 0, 0);
        activity.uploads.insert(
            ("library".into(), library::Id::from_u128(1)),
            upload.clone(),
        );
        let start = Instant::now();
        activity.sample(start);
        assert_eq!(activity.speed(), None);
        upload.sent.store(1_000_000, Ordering::Relaxed);
        activity.sample(start + Duration::from_millis(500));
        assert_eq!(activity.speed(), None);
        upload.sent.store(2_000_000, Ordering::Relaxed);
        activity.sample(start + Duration::from_secs(2));
        assert_eq!(activity.speed(), Some(1_000_000));
        // Finished uploads' bytes still count; old samples drop out.
        activity.uploads.clear();
        activity.finished_sent = 2_000_000;
        activity.sample(start + Duration::from_secs(4));
        assert_eq!(activity.samples.len(), 2);
        assert_eq!(activity.speed(), Some(0));
    }

    #[gpui_kit::test]
    fn sums_up_every_upload_under_way(cx: &mut TestAppContext) {
        assert_eq!(cx.update(|cx| progress(cx)), None);
        cx.update(|cx| {
            let uploads = &mut activity(cx).uploads;
            // One library to two targets, and another to one of them.
            uploads.insert(
                ("one".into(), library::Id::from_u128(1)),
                upload("a", 120, 1000, 400, 0),
            );
            uploads.insert(
                ("one".into(), library::Id::from_u128(2)),
                upload("b", 100, 1000, 100, 0),
            );
            uploads.insert(
                ("two".into(), library::Id::from_u128(1)),
                upload("a", 1, 2000, 0, 0),
            );
        });
        let sync = cx.update(|cx| progress(cx)).unwrap();
        assert_eq!(sync.photos, 121);
        assert_eq!(sync.targets, 2);
        assert_eq!(sync.percent, 12.5);
        assert_eq!(sync.text(), "Syncing 121 photos to 2 targets · 12%");
        let one = SyncProgress {
            photos: 1,
            targets: 1,
            percent: 99.9,
            speed: Some(3_200_000),
        };
        assert_eq!(one.text(), "Syncing 1 photo to 1 target · 99% · 3.2 MB/s");
    }

    #[gpui_kit::test]
    fn adds_a_target_and_syncs_a_library_to_it(cx: &mut TestAppContext) {
        let (config_dir, library_dir) = library::test_support::temp_dirs("sync-settings");
        let root = config_dir.parent().unwrap().to_path_buf();
        let source = root.join("card");
        for (name, shade) in [("IMG_0001.JPG", 10), ("IMG_0002.JPG", 20)] {
            std::fs::create_dir_all(&source).unwrap();
            image::RgbImage::from_pixel(16, 16, image::Rgb([shade, 90, 160]))
                .save(source.join(name))
                .unwrap();
        }
        let s3 = FakeS3::start();
        cx.update(|cx| {
            gpui_kit::init(cx);
            settings::init(cx);
            library::init_at(config_dir.clone(), library_dir, cx);
            sync_targets::test_support::init(config_dir.clone(), cx);
        });
        let library = cx.update(|cx| library::current(cx)).unwrap();
        library.import(std::slice::from_ref(&source)).unwrap();
        let library_id = library.info.id.clone();
        let window = cx
            .update(|cx| {
                gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(0.), px(0.)),
                            size: size(px(800.), px(600.)),
                        })),
                        ..Default::default()
                    },
                    cx,
                    |_, cx| cx.new(|_| Blank),
                )
            })
            .unwrap()
            .0;

        // The dialog has the plugin's settings, with its defaults.
        let open = |cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                let plugins = sync_targets::plugins(cx).unwrap().unwrap();
                let plugin = plugins.get("s3").unwrap().clone();
                open_target_dialog(plugins, plugin, None, Vec::new(), window, cx)
            })
            .unwrap()
        };
        let form = open(cx);
        cx.update(|cx| {
            let state = form.borrow();
            assert_eq!(state.name.read(cx).value(), "S3-Compatible Storage");
            let config = state.fields.config(cx);
            assert!(config.contains(&("region".into(), "us-east-1".into())));
            assert!(config.contains(&("virtual_hosted".into(), "false".into())));
        });

        // Missing settings keep the dialog open, and say what's wrong.
        fill_and_save(&form, &[("endpoint", &s3.endpoint)], window, cx);
        assert_eq!(form.borrow().error.as_deref(), Some("Bucket is required."));
        assert!(
            cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        );
        assert!(cx.update(|cx| sync_targets::list(cx).is_empty()));

        // Complete ones are checked with the service, and saved.
        let config = s3.config();
        let settings: Vec<(&str, &str)> = config
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_str()))
            .collect();
        fill_and_save(&form, &settings, window, cx);
        assert_eq!(form.borrow().error, None);
        assert!(
            !cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        );
        let targets = cx.update(|cx| sync_targets::list(cx).to_vec());
        assert_eq!(targets.len(), 1);
        assert_eq!(
            targets[0].summary,
            format!("S3-Compatible Storage · {}", s3.endpoint)
        );
        assert_eq!(s3.take()[0].method, "HEAD");

        // The library syncs to it.
        cx.update(|cx| link_target(&library_id, &targets[0].id, cx));
        cx.run_until_parked();
        let links = cx.update(|cx| read_activity(cx).unwrap().links[&library_id].clone());
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].1.pending, 2);
        draw(libraries_page, cx);
        draw(page, cx);
        cx.update_window(window, |_, window, cx| {
            sync_now(&library_id, &links[0].0, window, cx);
            assert!(is_syncing(cx));
        })
        .unwrap();
        cx.run_until_parked();
        assert!(!cx.update(|cx| is_syncing(cx)));
        assert_eq!(s3.take().len(), 2);
        let key = (library_id.clone(), links[0].0.id);
        let result = cx.update(|cx| read_activity(cx).unwrap().results[&key].clone());
        assert_eq!(result, "Uploaded 2 files.");
        let links = cx.update(|cx| read_activity(cx).unwrap().links[&library_id].clone());
        assert_eq!(links[0].1.synced, 2);
        draw(libraries_page, cx);

        // Syncing in the background, asked for again while under way, goes
        // again once it's done.
        let more = root.join("more");
        std::fs::create_dir_all(&more).unwrap();
        image::RgbImage::from_pixel(16, 16, image::Rgb([30, 90, 160]))
            .save(more.join("IMG_0003.JPG"))
            .unwrap();
        cx.update(|cx| sync_in_background(&library_id, cx));
        assert!(cx.update(|cx| is_syncing(cx)));
        library.import(std::slice::from_ref(&more)).unwrap();
        cx.update(|cx| sync_in_background(&library_id, cx));
        assert!(cx.update(|cx| read_activity(cx).unwrap().again.contains(&key)));
        cx.run_until_parked();
        assert!(!cx.update(|cx| is_syncing(cx)));
        assert!(cx.update(|cx| read_activity(cx).unwrap().again.is_empty()));
        assert_eq!(s3.take().len(), 1);
        let result = cx.update(|cx| read_activity(cx).unwrap().results[&key].clone());
        assert_eq!(result, "Nothing new to upload.");

        // Unless it's paused.
        cx.update(|cx| {
            settings::set_sync_automatically(false, cx);
            sync_in_background(&library_id, cx);
            assert!(!is_syncing(cx));
            settings::set_sync_automatically(true, cx);
        });

        // Deleting the target takes it off the library too.
        cx.update_window(window, |_, window, cx| {
            delete_target(&targets[0].id, window, cx)
        })
        .unwrap();
        cx.simulate_prompt_answer("Delete");
        cx.run_until_parked();
        assert!(cx.update(|cx| sync_targets::list(cx).is_empty()));
        assert!(library.sync_targets().unwrap().is_empty());
        let links = cx.update(|cx| read_activity(cx).unwrap().links[&library_id].clone());
        assert!(links.is_empty());

        drop(library);
        library::test_support::remove(&root);
    }
}

//! Object detection in Settings: choosing what finds objects in photos (see
//! `detect`), the built-in model or a detection plugin, and setting the
//! plugin up.
//!
//! Which is chosen is an app setting (`AppSettings::detection_plugin`). The
//! chosen plugin's settings, which may hold an API key, are kept in the
//! keychain on macOS, and in `detection-plugins.json` in the app's config
//! directory elsewhere; they're read in the background at startup.

use std::collections::BTreeMap;
#[cfg(not(target_os = "macos"))]
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use anyhow::{Context as _, Result, bail};
use gpui_kit::assets::IconName as LucideIcon;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::setting::{SettingField, SettingGroup, SettingItem, SettingPage};
use gpui_kit::component::{ActiveTheme, Disableable as _, Icon, Sizable, WindowExt as _};
use gpui_kit::{
    App, AppContext as _, BorrowAppContext as _, Global, IntoElement, ParentElement, SharedString,
    Styled, Window, div, px,
};

use crate::config_form::ConfigForm;
use crate::detect::{self, Detector};
use crate::gallery::show_warning;
#[cfg(not(target_os = "macos"))]
use crate::paths;
use crate::plugins::{self, Config};
use crate::{settings, sync_targets};

/// The keychain items holding detection plugins' settings, by plugin id.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "1k Words Object Detection";

/// What's known of the detector beyond the app settings. Observers are
/// told when it changes, as when the chosen plugin's ready to use.
#[derive(Default)]
pub struct DetectionSettings {
    /// The chosen plugin's id and settings, once they've been read.
    config: Option<(String, Config)>,
    /// Why finding objects last stopped, until it next works.
    error: Option<String>,
}

impl Global for DetectionSettings {}

/// Reads the chosen plugin's settings, in the background.
pub fn init(cx: &mut App) {
    cx.set_global(DetectionSettings::default());
    let Some(id) = settings::get(cx).detection_plugin.clone() else {
        return;
    };
    cx.spawn(async move |cx| {
        let config = cx
            .background_spawn({
                let id = id.clone();
                async move { stored_config(&id) }
            })
            .await;
        cx.update(|cx| {
            cx.update_global(|state: &mut DetectionSettings, _| match config {
                Ok(Some(config)) => state.config = Some((id, config)),
                Ok(None) => state.error = Some("Its settings are missing. Set it up again.".into()),
                Err(error) => state.error = Some(format!("{error:#}")),
            })
        });
    })
    .detach();
}

/// Tells observers the plugins have loaded, so the chosen one can be used.
pub fn plugins_loaded(cx: &mut App) {
    if cx.has_global::<DetectionSettings>() {
        cx.update_global(|_: &mut DetectionSettings, _| {});
    }
}

/// The key (see `Detector::key`) of the chosen detector, which is known
/// before it's ready.
pub fn current_key(cx: &App) -> String {
    detect::key_for(settings::get(cx).detection_plugin.as_deref())
}

/// The chosen detector, once it's ready: the built-in model, or the chosen
/// plugin once it's loaded and its settings have been read.
pub fn current(cx: &App) -> Option<Detector> {
    let Some(id) = &settings::get(cx).detection_plugin else {
        return Some(Detector::BuiltIn);
    };
    let plugin = sync_targets::plugins(cx).ok()??.detector(id)?.clone();
    let (configured, config) = cx.try_global::<DetectionSettings>()?.config.as_ref()?;
    (configured == id).then(|| Detector::Plugin {
        plugin,
        config: config.clone(),
    })
}

/// What the chosen detector's called, for the status bar.
pub fn current_name(cx: &App) -> String {
    let Some(id) = &settings::get(cx).detection_plugin else {
        return "YOLO11".into();
    };
    sync_targets::plugins(cx)
        .ok()
        .flatten()
        .and_then(|plugins| Some(plugins.detector(id)?.info.name.clone()))
        .unwrap_or_else(|| id.clone())
}

/// Why finding objects last stopped, if it did and hasn't worked since.
pub fn error(cx: &App) -> Option<String> {
    cx.try_global::<DetectionSettings>()?.error.clone()
}

/// Records why finding objects stopped, or that it's working again.
pub fn set_error(error: Option<String>, cx: &mut App) {
    if cx
        .try_global::<DetectionSettings>()
        .is_some_and(|state| state.error != error)
    {
        cx.update_global(|state: &mut DetectionSettings, _| state.error = error);
    }
}

/// The Object Detection page.
pub fn page(cx: &App) -> SettingPage {
    SettingPage::new("Object Detection")
        .icon(Icon::new(LucideIcon::ScanEye))
        .resettable(false)
        .description(
            "What finds the objects in your photos that you can search for. What each finds \
             is kept, so going back to one doesn't look at the photos again.",
        )
        .group(providers_group(cx))
}

fn providers_group(cx: &App) -> SettingGroup {
    let chosen = settings::get(cx).detection_plugin.clone();
    let error = cx
        .try_global::<DetectionSettings>()
        .and_then(|state| state.error.clone());
    let with_error = |description: String, chosen: bool| match (&error, chosen) {
        (Some(error), true) => format!("{description}\n\nFinding objects stopped: {error}"),
        _ => description,
    };

    let built_in = SettingItem::new(
        "YOLO11, on This Device",
        SettingField::render({
            let in_use = chosen.is_none();
            move |_, _, cx| {
                if in_use {
                    return in_use_label(cx).into_any_element();
                }
                Button::new("use-built-in")
                    .label("Use")
                    .small()
                    .on_click(|_, _, cx| use_built_in(cx))
                    .into_any_element()
            }
        }),
    )
    .description(with_error(
        "Finds 80 kinds of everyday object on this device, so photos never leave it. Its \
         model (228 MB) is downloaded the first time it's needed."
            .into(),
        chosen.is_none(),
    ));

    let group = SettingGroup::new().title("Provider").item(built_in);
    let plugins = match sync_targets::plugins(cx) {
        Ok(Some(plugins)) => plugins,
        _ => return group,
    };
    group.items(plugins.detectors().iter().enumerate().map(|(ix, plugin)| {
        let in_use = chosen.as_deref() == Some(plugin.info.id.as_str());
        let id = plugin.info.id.clone();
        SettingItem::new(
            plugin.info.name.clone(),
            SettingField::render(move |_, _, cx| {
                let id = id.clone();
                let open = move |_: &_, window: &mut Window, cx: &mut App| {
                    open_plugin_dialog(&id, window, cx);
                };
                if in_use {
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(in_use_label(cx))
                        .child(
                            Button::new(("edit-plugin", ix))
                                .label("Edit…")
                                .small()
                                .on_click(open),
                        )
                        .into_any_element()
                } else {
                    Button::new(("use-plugin", ix))
                        .label("Use…")
                        .small()
                        .on_click(open)
                        .into_any_element()
                }
            }),
        )
        .description(with_error(plugin.info.description.clone(), in_use))
    }))
}

fn in_use_label(cx: &App) -> impl IntoElement {
    div()
        .text_sm()
        .text_color(cx.theme().muted_foreground)
        .child("In Use")
}

fn use_built_in(cx: &mut App) {
    settings::set_detection_plugin(None, cx);
    cx.update_global(|state: &mut DetectionSettings, _| {
        state.config = None;
        state.error = None;
    });
}

/// The dialog to set up the detection plugin `id` and use it. Returns its
/// state.
fn open_plugin_dialog(id: &str, window: &mut Window, cx: &mut App) -> Option<Rc<PluginForm>> {
    let Some(plugin) = sync_targets::plugins(cx)
        .ok()
        .flatten()
        .and_then(|plugins| plugins.detector(id).cloned())
    else {
        show_warning(
            "Object detection plugins aren't available.",
            "They're still loading, or couldn't be loaded.",
            window,
            cx,
        );
        return None;
    };
    let config = cx
        .try_global::<DetectionSettings>()
        .and_then(|state| state.config.clone())
        .filter(|(configured, _)| configured == id)
        .map(|(_, config)| config)
        .unwrap_or_default();
    let form = Rc::new(PluginForm {
        fields: ConfigForm::new(&plugin.info.config, &config, window, cx),
        plugin,
        saving: Default::default(),
        error: Default::default(),
    });
    let dialog_form = form.clone();
    window.open_dialog(cx, move |dialog, _, cx| {
        let form = &dialog_form;
        let info = &form.plugin.info;
        let mut body = div().flex().flex_col().gap_3().child(
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(info.description.clone()),
        );
        body = body.children(form.fields.rows(cx));
        if let Some(error) = form.error.borrow().clone() {
            body = body.child(div().text_sm().text_color(cx.theme().danger).child(error));
        }
        let saving = form.saving.get();
        let save_form = form.clone();
        dialog
            .title(SharedString::from(info.name.clone()))
            .w(px(480.))
            .child(body)
            .footer(
                DialogFooter::new()
                    .child(
                        Button::new("cancel")
                            .label("Cancel")
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("use")
                            .label("Use")
                            .primary()
                            .loading(saving)
                            .disabled(saving)
                            .on_click(move |_, window, cx| save(save_form.clone(), window, cx)),
                    ),
            )
    });
    Some(form)
}

/// The plugin dialog's state.
struct PluginForm {
    plugin: Arc<plugins::Detector>,
    fields: ConfigForm,
    /// While the plugin checks the settings.
    saving: std::cell::Cell<bool>,
    error: std::cell::RefCell<Option<String>>,
}

/// Has the plugin check the settings in the background, then keeps them,
/// uses the plugin and closes the dialog; or shows why not.
fn save(form: Rc<PluginForm>, window: &mut Window, cx: &mut App) {
    form.saving.set(true);
    *form.error.borrow_mut() = None;
    window.refresh();
    let plugin = form.plugin.clone();
    let config = form.fields.config(cx);
    let check = cx.background_spawn(async move { validate_and_store(&plugin, config) });
    window
        .spawn(cx, async move |cx| {
            let result = check.await;
            cx.update(|window, cx| {
                form.saving.set(false);
                match result {
                    Ok(config) => {
                        let id = form.plugin.info.id.clone();
                        settings::set_detection_plugin(Some(id.clone()), cx);
                        cx.update_global(|state: &mut DetectionSettings, _| {
                            state.config = Some((id, config));
                            state.error = None;
                        });
                        window.close_dialog(cx);
                    }
                    Err(error) => *form.error.borrow_mut() = Some(format!("{error:#}")),
                }
                window.refresh();
            })
            .ok();
        })
        .detach();
}

/// Fills in the plugin's defaults, has it check the settings (which may
/// mean asking its service), and keeps them. Blocks.
fn validate_and_store(plugin: &plugins::Detector, config: Config) -> Result<Config> {
    let config = plugins::complete_config(&plugin.info.config, config)?;
    if let Err(error) = plugin.session(config.clone())?.validate()? {
        bail!("{}", error.message());
    }
    store_config(&plugin.info.id, &config)?;
    Ok(config)
}

/// A detection plugin's settings, as kept. Blocks on the keychain.
fn stored_config(id: &str) -> Result<Option<Config>> {
    let Some(json) = load_config(id)? else {
        return Ok(None);
    };
    let config: BTreeMap<String, String> =
        serde_json::from_slice(&json).context("reading the detection plugin's settings")?;
    Ok(Some(config.into_iter().collect()))
}

fn store_config(id: &str, config: &Config) -> Result<()> {
    let config: BTreeMap<&str, &str> = config
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    save_config(id, serde_json::to_vec(&config)?)
}

#[cfg(target_os = "macos")]
fn load_config(id: &str) -> Result<Option<Vec<u8>>> {
    crate::keychain::get(KEYCHAIN_SERVICE, id)
}

#[cfg(target_os = "macos")]
fn save_config(id: &str, json: Vec<u8>) -> Result<()> {
    crate::keychain::set(KEYCHAIN_SERVICE, id, &json)
}

/// Elsewhere, every plugin's settings in one file.
#[cfg(not(target_os = "macos"))]
fn configs_path() -> Option<PathBuf> {
    Some(paths::config_dir()?.join("detection-plugins.json"))
}

#[cfg(not(target_os = "macos"))]
fn load_config(id: &str) -> Result<Option<Vec<u8>>> {
    let Some(path) = configs_path() else {
        return Ok(None);
    };
    let all: BTreeMap<String, serde_json::Value> = match std::fs::read(&path) {
        Ok(json) => serde_json::from_slice(&json)?,
        Err(_) => return Ok(None),
    };
    Ok(all.get(id).map(|config| config.to_string().into_bytes()))
}

#[cfg(not(target_os = "macos"))]
fn save_config(id: &str, json: Vec<u8>) -> Result<()> {
    let path = configs_path().context("no config directory")?;
    let mut all: BTreeMap<String, serde_json::Value> = std::fs::read(&path)
        .ok()
        .and_then(|json| serde_json::from_slice(&json).ok())
        .unwrap_or_default();
    all.insert(id.to_owned(), serde_json::from_slice(&json)?);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&all)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{Render, TestAppContext, WindowOptions};

    use super::*;
    use crate::library::detect_test_support::fake_vision_server;

    /// The Object Detection page, in a window of its own.
    struct Page;

    impl Render for Page {
        fn render(&mut self, _: &mut Window, cx: &mut gpui_kit::Context<Self>) -> impl IntoElement {
            gpui_kit::component::setting::Settings::new("page").page(page(cx))
        }
    }

    #[gpui_kit::test]
    fn sets_up_a_detection_plugin_and_uses_it(cx: &mut TestAppContext) {
        let (config_dir, _) = crate::library::test_support::temp_dirs("detection-settings");
        cx.update(|cx| {
            gpui_kit::init(cx);
            settings::init(cx);
            init(cx);
            sync_targets::test_support::init(config_dir.clone(), cx);
        });
        let is_built_in = |cx: &mut TestAppContext| {
            cx.update(|cx| {
                (
                    current_key(cx),
                    matches!(current(cx), Some(Detector::BuiltIn)),
                )
            })
        };
        assert_eq!(is_built_in(cx), ("yolo11x".into(), true));
        let window = cx
            .update(|cx| {
                gpui_kit::open_window(WindowOptions::default(), cx, |_, cx| cx.new(|_| Page))
            })
            .unwrap()
            .0;
        cx.run_until_parked();

        let (endpoint, _) = fake_vision_server();
        let form = cx
            .update_window(window, |_, window, cx| {
                open_plugin_dialog("openai-vision", window, cx)
            })
            .unwrap()
            .unwrap();
        let save_with = |model: &str, cx: &mut TestAppContext| {
            cx.update_window(window, |_, window, cx| {
                form.fields
                    .fill(&[("endpoint", &endpoint), ("model", model)], window, cx);
                save(form.clone(), window, cx);
            })
            .unwrap();
            cx.run_until_parked();
        };

        // A model the server doesn't have keeps the dialog open.
        save_with("nope", cx);
        let error = form.error.borrow().clone().unwrap();
        assert!(error.contains("no model named"), "{error}");
        assert!(
            cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        );
        assert!(is_built_in(cx).1);

        // One it has is used, and its settings kept.
        save_with("vision-model", cx);
        assert!(
            !cx.update_window(window, |_, window, cx| window.has_active_dialog(cx))
                .unwrap()
        );
        assert_eq!(is_built_in(cx), ("plugin:openai-vision".into(), false));
        assert!(cx.update(|cx| matches!(current(cx), Some(Detector::Plugin { .. }))));
        let stored = stored_config("openai-vision").unwrap().unwrap();
        assert!(stored.contains(&("model".into(), "vision-model".into())));
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();

        // And back to the built-in model.
        cx.update(use_built_in);
        assert_eq!(is_built_in(cx), ("yolo11x".into(), true));
        crate::library::test_support::remove(config_dir.parent().unwrap());
    }
}

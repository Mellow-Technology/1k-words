//! App settings: stored in a JSON file and applied on startup.

use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::component::setting::{
    SettingField, SettingGroup, SettingItem, SettingPage, Settings,
};
use gpui_kit::component::{IconName, Theme, ThemeMode, ThemeSet};
use gpui_kit::{App, Global, IntoElement, SharedString, Subscription, Window};
use serde::{Deserialize, Serialize};

use crate::acknowledgements;
use crate::detection_settings;
use crate::library::SpaceBudget;
use crate::library_settings::{self, LibraryActions};
use crate::sync_settings;

const THEME: &str = include_str!("../themes/1k-words.json");

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    /// Follow the system's light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl Appearance {
    const ALL: [Appearance; 3] = [Appearance::System, Appearance::Light, Appearance::Dark];

    fn key(self) -> &'static str {
        match self {
            Appearance::System => "system",
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Appearance::System => "System",
            Appearance::Light => "Light",
            Appearance::Dark => "Dark",
        }
    }
}

/// How the gallery lays out its thumbnails.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GridLayout {
    /// Columns of thumbnails at their own aspect ratios.
    #[default]
    Masonry,
    /// Rows of square, cropped thumbnails.
    Square,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub appearance: Appearance,
    pub grid_layout: GridLayout,
    /// Whether the viewer draws boxes around the objects found in photos.
    pub show_object_boxes: bool,
    /// The detection plugin that finds objects in photos, by id; none for
    /// the built-in model (see `detection_settings`).
    pub detection_plugin: Option<String>,
    /// Whether libraries only sync with Sync Now, rather than after imports
    /// and from time to time as well (see `sync_settings`).
    pub pause_automatic_sync: bool,
    /// The server libraries' catalogs sync with, if any (see
    /// `catalog_sync`). Its token is in the keychain.
    pub catalog_server: Option<String>,
    /// How much originals may take on this device (see `library::free_space`).
    pub space_budget: SpaceBudget,
}

impl Global for AppSettings {}

/// Loads the saved settings and the app's themes, and applies them.
pub fn init(cx: &mut App) {
    load_themes(cx);
    cx.set_global(load());
    apply_appearance(None, cx);
}

pub fn get(cx: &App) -> &AppSettings {
    cx.global::<AppSettings>()
}

fn update(cx: &mut App, f: impl FnOnce(&mut AppSettings)) {
    f(cx.global_mut::<AppSettings>());
    if let Err(error) = save(get(cx)) {
        eprintln!("could not save settings: {error:#}");
    }
}

pub fn set_grid_layout(layout: GridLayout, cx: &mut App) {
    update(cx, |settings| settings.grid_layout = layout);
}

pub fn set_show_object_boxes(show: bool, cx: &mut App) {
    update(cx, |settings| settings.show_object_boxes = show);
}

pub fn set_detection_plugin(plugin: Option<String>, cx: &mut App) {
    update(cx, |settings| settings.detection_plugin = plugin);
}

pub fn set_space_budget(cx: &mut App, f: impl FnOnce(&mut SpaceBudget)) {
    update(cx, |settings| f(&mut settings.space_budget));
}

pub fn set_sync_automatically(on: bool, cx: &mut App) {
    update(cx, |settings| settings.pause_automatic_sync = !on);
}

pub fn set_catalog_server(url: Option<String>, cx: &mut App) {
    update(cx, |settings| settings.catalog_server = url);
}

/// Applies the appearance setting, reading the system appearance from
/// `window` when given.
pub fn apply_appearance(window: Option<&mut Window>, cx: &mut App) {
    match get(cx).appearance {
        Appearance::System => Theme::sync_system_appearance(window, cx),
        Appearance::Light => Theme::change(ThemeMode::Light, window, cx),
        Appearance::Dark => Theme::change(ThemeMode::Dark, window, cx),
    }
}

/// Keeps the theme in step with the system appearance while the setting is
/// "System". Call once per window.
pub fn observe_system_appearance(window: &mut Window) -> Subscription {
    window.observe_window_appearance(|window, cx| {
        if get(cx).appearance == Appearance::System {
            Theme::sync_system_appearance(Some(window), cx);
        }
    })
}

/// The settings page shown in the main area. `libraries` does what the
/// Libraries page's buttons ask.
pub fn settings_view(libraries: LibraryActions, cx: &App) -> impl IntoElement {
    let appearance = SettingField::dropdown(
        Appearance::ALL
            .iter()
            .map(|a| (SharedString::from(a.key()), SharedString::from(a.label())))
            .collect(),
        |cx| get(cx).appearance.key().into(),
        |key, cx| {
            let Some(appearance) = Appearance::ALL.into_iter().find(|a| key == a.key()) else {
                return;
            };
            update(cx, |settings| settings.appearance = appearance);
            apply_appearance(None, cx);
        },
    )
    .default_value(SharedString::from(Appearance::default().key()));

    Settings::new("settings")
        .page(
            SettingPage::new("Appearance")
                .icon(IconName::Palette)
                .group(
                    SettingGroup::new().item(
                        SettingItem::new("Theme", appearance)
                            .description("Use a light or dark appearance, or match the system."),
                    ),
                ),
        )
        .page(library_settings::page(libraries, cx))
        .page(sync_settings::page(cx))
        .page(detection_settings::page(cx))
        .page(acknowledgements::page())
}

/// Registers the app's light and dark themes with GPUI Kit.
fn load_themes(cx: &mut App) {
    let set = match serde_json::from_str::<ThemeSet>(THEME) {
        Ok(set) => set,
        Err(error) => {
            eprintln!("could not load theme: {error}");
            return;
        }
    };
    let theme = Theme::global_mut(cx);
    for config in set.themes {
        if config.mode.is_dark() {
            theme.dark_theme = Rc::new(config);
        } else {
            theme.light_theme = Rc::new(config);
        }
    }
}

fn settings_path() -> Option<PathBuf> {
    Some(crate::paths::config_dir()?.join("settings.json"))
}

fn load() -> AppSettings {
    // Tests start from the defaults, whatever the user has chosen.
    if cfg!(test) {
        return AppSettings::default();
    }
    settings_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

fn save(settings: &AppSettings) -> anyhow::Result<()> {
    // Nor do tests' changes overwrite the user's settings.
    if cfg!(test) {
        return Ok(());
    }
    let path = settings_path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(settings)?)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_has_light_and_dark_variants() {
        let set: ThemeSet = serde_json::from_str(THEME).expect("theme JSON");
        let modes: Vec<bool> = set.themes.iter().map(|t| t.mode.is_dark()).collect();
        assert_eq!(modes, vec![false, true]);
    }

    #[test]
    fn themes_use_bundled_font() {
        let set: ThemeSet = serde_json::from_str(THEME).expect("theme JSON");
        for theme in set.themes {
            assert_eq!(
                theme.font_family.as_deref(),
                Some(crate::assets::FONT_FAMILY)
            );
        }
    }

    #[test]
    fn appearance_defaults_to_system() {
        let settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.appearance, Appearance::System);
        let settings: AppSettings = serde_json::from_str(r#"{"appearance":"dark"}"#).unwrap();
        assert_eq!(settings.appearance, Appearance::Dark);
        assert_eq!(settings.grid_layout, GridLayout::Masonry);
        let settings: AppSettings = serde_json::from_str(r#"{"grid_layout":"square"}"#).unwrap();
        assert_eq!(settings.grid_layout, GridLayout::Square);
    }
}

//! Sync targets: the places libraries sync to, such as an S3 bucket. Each is
//! a sync plugin (see `plugins`) and its settings.
//!
//! Targets belong to the app, not a library: any number of libraries can
//! sync to the same target, each into its own folder there, and a library
//! can sync to any number of targets (see `library::sync`).
//!
//! `sync-targets.json` in the app's config directory lists them. Their
//! settings, credentials included, are kept in the keychain on macOS, and in
//! that file elsewhere.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result, anyhow, bail};
use gpui_kit::{App, AppContext as _, Global};
use serde::{Deserialize, Serialize};

use crate::paths;
use crate::plugins::{Config, Plugins};

/// The keychain items holding targets' settings, one per target.
#[cfg(target_os = "macos")]
const KEYCHAIN_SERVICE: &str = "1k Words Sync";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    pub name: String,
    /// The plugin's id.
    pub plugin: String,
    /// What's shown about it besides its name, such as the service's URL.
    /// Never secrets.
    #[serde(default)]
    pub summary: String,
    /// The settings, where they aren't kept in the keychain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    config: Option<BTreeMap<String, String>>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Registry {
    targets: Vec<TargetInfo>,
}

/// The targets, and the plugins they use.
pub struct SyncTargets {
    /// `sync-targets.json`, if there's a config directory.
    path: Option<PathBuf>,
    registry: Registry,
    /// Loaded in the background at startup.
    plugins: Option<Arc<Plugins>>,
    /// Why the plugins couldn't be loaded, if they couldn't.
    plugins_error: Option<String>,
}

impl Global for SyncTargets {}

/// Reads the list of targets, and starts loading the plugins.
pub fn init(cx: &mut App) {
    init_at(paths::config_dir(), cx);
}

/// `init`, with the config directory given.
pub fn init_at(config_dir: Option<PathBuf>, cx: &mut App) {
    let path = config_dir.map(|dir| dir.join("sync-targets.json"));
    let registry = path
        .as_ref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default();
    cx.set_global(SyncTargets {
        path,
        registry,
        plugins: None,
        plugins_error: None,
    });
    let plugins_dir = paths::config_dir().map(|dir| dir.join("plugins"));
    cx.spawn(async move |cx| {
        let plugins = cx
            .background_spawn(async move { Plugins::load(plugins_dir.as_deref()) })
            .await;
        cx.update(|cx| {
            let targets = cx.global_mut::<SyncTargets>();
            match plugins {
                Ok(plugins) => {
                    for (source, error) in &plugins.failed {
                        eprintln!("couldn't load the sync plugin {source}: {error}");
                    }
                    targets.plugins = Some(Arc::new(plugins));
                }
                Err(error) => targets.plugins_error = Some(format!("{error:#}")),
            }
            // The chosen detection plugin may be ready to use now.
            crate::detection_settings::plugins_loaded(cx);
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Every target, in the order they were added.
pub fn list(cx: &App) -> &[TargetInfo] {
    cx.try_global::<SyncTargets>()
        .map_or(&[], |targets| targets.registry.targets.as_slice())
}

pub fn get(id: &str, cx: &App) -> Option<TargetInfo> {
    list(cx).iter().find(|target| target.id == id).cloned()
}

/// The plugins, once they're loaded; or why they couldn't be.
pub fn plugins(cx: &App) -> Result<Option<Arc<Plugins>>, String> {
    let Some(targets) = cx.try_global::<SyncTargets>() else {
        return Ok(None);
    };
    match &targets.plugins_error {
        Some(error) => Err(error.clone()),
        None => Ok(targets.plugins.clone()),
    }
}

/// A target's settings. Blocks on the keychain.
pub fn config(target: &TargetInfo) -> Result<Config> {
    let json = match &target.config {
        Some(config) => return Ok(config.clone().into_iter().collect()),
        None => load_config(&target.id)?
            .with_context(|| format!("The settings for {} are missing.", target.name))?,
    };
    let config: BTreeMap<String, String> = serde_json::from_slice(&json)
        .with_context(|| format!("reading the settings for {}", target.name))?;
    Ok(config.into_iter().collect())
}

/// Fills in `config` with the plugin's defaults, checks the required
/// settings are there, and has the plugin check them (which may mean asking
/// the service). Blocks.
pub fn validate(plugins: &Plugins, plugin: &str, config: Config) -> Result<Config> {
    let plugin = plugins
        .get(plugin)
        .ok_or_else(|| anyhow!("there's no sync plugin \"{plugin}\""))?;
    let config = crate::plugins::complete_config(&plugin.info.config, config)?;
    if let Err(error) = plugin.session(config.clone())?.validate()? {
        bail!("{}", error.message());
    }
    Ok(config)
}

/// A target to save: its name, plugin, settings (already validated, see
/// `validate`) and `TargetInfo::summary`.
pub struct NewTarget {
    pub name: String,
    pub plugin: String,
    pub summary: String,
    pub config: Config,
}

/// Adds a target, or updates the target with `id`. Returns its id.
pub fn save(id: Option<&str>, target: NewTarget, cx: &mut App) -> Result<String> {
    let NewTarget {
        name,
        plugin,
        summary,
        config,
    } = target;
    let name = name.trim();
    if name.is_empty() {
        bail!("Name is required.");
    }
    let id = id.map_or_else(|| uuid::Uuid::new_v4().to_string(), str::to_owned);
    let config: BTreeMap<String, String> = config.into_iter().collect();
    let inline = store_config(&id, &serde_json::to_string(&config)?)?.then_some(config);
    let target = TargetInfo {
        id: id.clone(),
        name: name.to_owned(),
        plugin,
        summary,
        config: inline,
    };
    let targets = cx.global_mut::<SyncTargets>();
    match targets.registry.targets.iter_mut().find(|t| t.id == id) {
        Some(existing) => *existing = target,
        None => targets.registry.targets.push(target),
    }
    save_registry(targets)?;
    cx.refresh_windows();
    Ok(id)
}

/// Removes a target, and its settings. Libraries' records of it should be
/// removed too (see `LibraryHandle::forget_sync_target`).
pub fn delete(id: &str, cx: &mut App) -> Result<()> {
    let targets = cx.global_mut::<SyncTargets>();
    targets.registry.targets.retain(|target| target.id != id);
    save_registry(targets)?;
    delete_config(id)?;
    cx.refresh_windows();
    Ok(())
}

fn save_registry(targets: &SyncTargets) -> Result<()> {
    let Some(path) = &targets.path else {
        return Ok(());
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&targets.registry)?)?;
    Ok(())
}

/// Keeps a target's settings in the keychain, returning whether they should
/// be kept in the list of targets instead.
#[cfg(target_os = "macos")]
fn store_config(id: &str, json: &str) -> Result<bool> {
    crate::keychain::set(KEYCHAIN_SERVICE, id, json.as_bytes())?;
    Ok(false)
}

#[cfg(target_os = "macos")]
fn load_config(id: &str) -> Result<Option<Vec<u8>>> {
    crate::keychain::get(KEYCHAIN_SERVICE, id)
}

#[cfg(target_os = "macos")]
fn delete_config(id: &str) -> Result<()> {
    crate::keychain::delete(KEYCHAIN_SERVICE, id)
}

#[cfg(not(target_os = "macos"))]
fn store_config(_: &str, _: &str) -> Result<bool> {
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
fn load_config(_: &str) -> Result<Option<Vec<u8>>> {
    Ok(None)
}

#[cfg(not(target_os = "macos"))]
fn delete_config(_: &str) -> Result<()> {
    Ok(())
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// Sets up the targets with the plugins loaded, synchronously.
    pub fn init(config_dir: PathBuf, cx: &mut App) {
        init_at(Some(config_dir), cx);
        let plugins = Plugins::load(None).unwrap();
        cx.global_mut::<SyncTargets>().plugins = Some(Arc::new(plugins));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn saves_and_reads_targets(cx: &mut gpui_kit::TestAppContext) {
        let (config_dir, _) = crate::library::test_support::temp_dirs("sync-targets");
        cx.update(|cx| test_support::init(config_dir.clone(), cx));
        let config: Config = vec![("bucket".into(), "photos".into())];
        let id = cx
            .update(|cx| save(None, new_target(" Backup ", config.clone()), cx))
            .unwrap();
        let target = cx.update(|cx| get(&id, cx)).unwrap();
        assert_eq!(target.name, "Backup");
        assert_eq!(config_of(&target), config);

        // The settings are kept out of the file on macOS.
        let json = std::fs::read_to_string(config_dir.join("sync-targets.json")).unwrap();
        assert_eq!(json.contains("photos"), !cfg!(target_os = "macos"));

        // Reloading keeps them; updating replaces them.
        cx.update(|cx| init_at(Some(config_dir.clone()), cx));
        let new_config: Config = vec![("bucket".into(), "other".into())];
        cx.update(|cx| save(Some(&id), new_target("Backup", new_config.clone()), cx))
            .unwrap();
        assert_eq!(cx.update(|cx| list(cx).len()), 1);
        assert_eq!(
            config_of(&cx.update(|cx| get(&id, cx)).unwrap()),
            new_config
        );
        assert!(
            cx.update(|cx| save(None, new_target(" ", Vec::new()), cx))
                .is_err()
        );

        cx.update(|cx| delete(&id, cx)).unwrap();
        assert!(cx.update(|cx| list(cx).is_empty()));
        if cfg!(target_os = "macos") {
            assert!(super::config(&target).is_err());
        }
        crate::library::test_support::remove(config_dir.parent().unwrap());
    }

    fn new_target(name: &str, config: Config) -> NewTarget {
        NewTarget {
            name: name.into(),
            plugin: "s3".into(),
            summary: String::new(),
            config,
        }
    }

    fn config_of(target: &TargetInfo) -> Config {
        config(target).unwrap()
    }

    #[test]
    fn validates_settings_with_the_plugin() {
        let plugins = Plugins::load(None).unwrap();
        let error = validate(&plugins, "s3", Vec::new()).unwrap_err();
        assert_eq!(error.to_string(), "Endpoint is required.");
        let error = validate(&plugins, "nope", Vec::new()).unwrap_err();
        assert!(error.to_string().contains("no sync plugin"));
        // With everything there, the plugin checks the bucket: here, on a
        // port nothing's listening on.
        let config = [
            ("endpoint", "http://127.0.0.1:9"),
            ("bucket", "photos"),
            ("access_key_id", "id"),
            ("secret_access_key", "secret"),
        ]
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .to_vec();
        assert!(validate(&plugins, "s3", config).is_err());
    }
}

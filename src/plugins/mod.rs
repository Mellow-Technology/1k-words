//! Plugins: WebAssembly components, run with Wasmtime, that extend the app.
//! Their interface is `wit/plugins.wit`. There are two kinds:
//!
//! - Sync plugins copy a library's files to and from external services. A
//!   plugin only moves files: what's been synced where is recorded in the
//!   library's catalog (see `library::sync`).
//! - Detection plugins find objects in photos, such as with a vision model
//!   on a service (see `detect`).
//!
//! Plugins are sandboxed: they get no file system, environment or sockets,
//! only what `host` provides — reading the file being synced, and HTTP
//! requests to the hosts their settings name. The built-in plugins
//! (`plugins/*`, built by `build.rs`) are embedded in the app; more can be
//! added as `.wasm` files in the app's `plugins` folder.

mod host;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use anyhow::{Context as _, Result, anyhow};
use wasmtime::component::{Component, HasSelf, Linker, Resource, ResourceTable};
use wasmtime::{Engine, Store, StoreLimitsBuilder};
use wasmtime_wasi::WasiCtx;

mod bindings {
    wasmtime::component::bindgen!({
        path: "wit",
        world: "sync-plugin",
        with: {
            "onekwords:plugins/files.source": super::host::Source,
            "onekwords:plugins/files.sink": super::host::Sink,
        },
    });
}

mod detect_bindings {
    // The imports are the same as sync plugins', so they share them.
    wasmtime::component::bindgen!({
        path: "wit",
        world: "detect-plugin",
        with: {
            "onekwords:plugins/log": super::bindings::onekwords::plugins::log,
            "onekwords:plugins/files": super::bindings::onekwords::plugins::files,
            "onekwords:plugins/http": super::bindings::onekwords::plugins::http,
            "onekwords:plugins/settings": super::bindings::onekwords::plugins::settings,
        },
    });
}

pub use bindings::exports::onekwords::plugins::sync::{
    Capabilities, LocalItem, Multipart, PluginInfo, RemoteItem, SyncError,
};
pub use bindings::onekwords::plugins::settings::{ConfigField, FieldKind};
use bindings::{SyncPlugin, SyncPluginPre};
pub use detect_bindings::exports::onekwords::plugins::detect::{
    DetectError, DetectedObject, Image, ProviderInfo,
};
use detect_bindings::{DetectPlugin, DetectPluginPre};

/// The most memory a plugin instance may use.
const MEMORY_LIMIT: usize = 256 << 20;

/// The plugins built into the app, by `build.rs`.
const BUILT_IN: &[(&str, &[u8])] = &[
    (
        "s3",
        include_bytes!(concat!(env!("OUT_DIR"), "/plugins/s3.wasm")),
    ),
    (
        "openai-vision",
        include_bytes!(concat!(env!("OUT_DIR"), "/plugins/openai-vision.wasm")),
    ),
];

/// A plugin's settings, by `ConfigField::key`.
pub type Config = Vec<(String, String)>;

/// The loaded plugins.
pub struct Plugins {
    plugins: Vec<Arc<Plugin>>,
    detectors: Vec<Arc<Detector>>,
    /// Plugins that couldn't be loaded, and why.
    pub failed: Vec<(String, String)>,
}

impl Plugins {
    /// Loads the built-in plugins, and those in `dir` (if given). Plugins
    /// that fail to load are skipped and listed in `failed`; a plugin in
    /// `dir` with the same id as an earlier one replaces it.
    pub fn load(dir: Option<&Path>) -> Result<Self> {
        let runtime = Arc::new(Runtime::new()?);
        let mut plugins = Self {
            plugins: Vec::new(),
            detectors: Vec::new(),
            failed: Vec::new(),
        };
        for (name, wasm) in BUILT_IN {
            plugins.add(name.to_string(), load(&runtime, wasm));
        }
        let mut files: Vec<PathBuf> = dir
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .into_iter()
            .flatten()
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|e| e == "wasm"))
            .collect();
        files.sort();
        for path in files {
            let plugin = std::fs::read(&path)
                .with_context(|| format!("reading {}", path.display()))
                .and_then(|wasm| load(&runtime, &wasm));
            plugins.add(path.display().to_string(), plugin);
        }
        Ok(plugins)
    }

    fn add(&mut self, source: String, plugin: Result<Loaded>) {
        match plugin {
            Ok(Loaded::Sync(plugin)) => {
                self.plugins.retain(|p| p.info.id != plugin.info.id);
                self.plugins.push(Arc::new(plugin));
            }
            Ok(Loaded::Detect(detector)) => {
                self.detectors.retain(|d| d.info.id != detector.info.id);
                self.detectors.push(Arc::new(detector));
            }
            Err(error) => self.failed.push((source, format!("{error:#}"))),
        }
    }

    /// The sync plugins.
    pub fn all(&self) -> &[Arc<Plugin>] {
        &self.plugins
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Plugin>> {
        self.plugins.iter().find(|plugin| plugin.info.id == id)
    }

    /// The detection plugins.
    pub fn detectors(&self) -> &[Arc<Detector>] {
        &self.detectors
    }

    pub fn detector(&self, id: &str) -> Option<&Arc<Detector>> {
        self.detectors
            .iter()
            .find(|detector| detector.info.id == id)
    }
}

enum Loaded {
    Sync(Plugin),
    Detect(Detector),
}

/// Compiles a plugin, of whichever kind it is.
fn load(runtime: &Arc<Runtime>, wasm: &[u8]) -> Result<Loaded> {
    let component = Component::new(&runtime.engine, wasm)
        .map_err(|e| anyhow!("{e:?}"))
        .context("compiling the plugin")?;
    let pre = runtime
        .linker
        .instantiate_pre(&component)
        .map_err(|e| anyhow!("{e:?}"))
        .context("linking the plugin")?;
    if let Ok(pre) = SyncPluginPre::new(pre.clone()) {
        return Plugin::new(runtime, pre).map(Loaded::Sync);
    }
    match DetectPluginPre::new(pre) {
        Ok(pre) => Detector::new(runtime, pre).map(Loaded::Detect),
        Err(error) => {
            Err(anyhow!("{error:?}")).context("it's neither a sync plugin nor a detection plugin")
        }
    }
}

/// What every plugin shares: the Wasmtime engine and the host's imports.
struct Runtime {
    engine: Engine,
    linker: Linker<host::State>,
}

impl Runtime {
    fn new() -> Result<Self> {
        let engine = Engine::default();
        let mut linker = Linker::new(&engine);
        // Rust's standard library imports WASI even when it isn't used; the
        // context these get is empty.
        wasmtime_wasi::p2::add_to_linker_sync(&mut linker).map_err(|e| anyhow!("{e:?}"))?;
        // Detection plugins' imports are the same.
        SyncPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(Self { engine, linker })
    }

    /// A store for an instance of the plugin `id`, which may only reach
    /// `hosts`.
    fn store(&self, id: &str, hosts: Vec<String>) -> Store<host::State> {
        let state = host::State {
            plugin: id.to_owned(),
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).build(),
            http: host::Http::new(hosts),
            sent: None,
        };
        let mut store = Store::new(&self.engine, state);
        store.limiter(|state| &mut state.limits);
        store
    }
}

/// `config` trimmed, without empty settings, and with the defaults of
/// `fields` for those missing; or why not, if a required one's missing.
pub fn complete_config(fields: &[ConfigField], config: Config) -> Result<Config> {
    let mut settings: std::collections::BTreeMap<String, String> = config
        .into_iter()
        .map(|(key, value)| (key, value.trim().to_owned()))
        .filter(|(_, value)| !value.is_empty())
        .collect();
    for field in fields {
        match (settings.contains_key(&field.key), &field.default) {
            (true, _) => {}
            (false, Some(default)) => {
                settings.insert(field.key.clone(), default.clone());
            }
            (false, None) if field.required => anyhow::bail!("{} is required.", field.label),
            (false, None) => {}
        }
    }
    Ok(settings.into_iter().collect())
}

/// The hosts a plugin may reach with `config`: those in its info, and its
/// URL settings'.
fn allowed_hosts(hosts: &[String], fields: &[ConfigField], config: &Config) -> Vec<String> {
    let mut hosts = hosts.to_vec();
    for field in fields {
        if field.kind != FieldKind::Url {
            continue;
        }
        let url = config.iter().find(|(key, _)| *key == field.key);
        if let Some(host) = url.and_then(|(_, url)| host::host_of(url)) {
            hosts.push(host);
        }
    }
    hosts
}

/// A compiled sync plugin.
pub struct Plugin {
    pub info: PluginInfo,
    runtime: Arc<Runtime>,
    pre: SyncPluginPre<host::State>,
}

impl Plugin {
    fn new(runtime: &Arc<Runtime>, pre: SyncPluginPre<host::State>) -> Result<Self> {
        let mut store = runtime.store("", Vec::new());
        let instance = pre
            .instantiate(&mut store)
            .map_err(|e| anyhow!("{e:?}"))
            .context("starting the plugin")?;
        let info = instance
            .onekwords_plugins_sync()
            .call_info(&mut store)
            .map_err(|e| anyhow!("{e:?}"))?;
        if info.id.is_empty() {
            return Err(anyhow!("the plugin has no id"));
        }
        Ok(Self {
            info,
            runtime: runtime.clone(),
            pre,
        })
    }

    /// Starts an instance of the plugin for a target with `config`. It may
    /// only reach the hosts in its `info.hosts` and its URL settings.
    pub fn session(&self, config: Config) -> Result<Session> {
        let hosts = allowed_hosts(&self.info.hosts, &self.info.config, &config);
        let mut store = self.runtime.store(&self.info.id, hosts);
        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| anyhow!("{e:?}"))
            .context("starting the plugin")?;
        Ok(Session {
            store,
            instance,
            config,
        })
    }
}

/// A running instance of a sync plugin, for one target. Calls return the
/// plugin's own errors inside `Ok`; `Err` means the plugin crashed (or the
/// host failed), after which the session shouldn't be used again.
pub struct Session {
    store: Store<host::State>,
    instance: SyncPlugin,
    config: Config,
}

impl Session {
    pub fn validate(&mut self) -> Result<Result<(), SyncError>> {
        self.instance
            .onekwords_plugins_sync()
            .call_validate(&mut self.store, &self.config)
            .map_err(|e| anyhow!("{e:?}"))
    }

    /// Adds the bytes of files sent from now on to `sent`.
    pub fn count_sent(&mut self, sent: Arc<AtomicU64>) {
        self.store.data_mut().sent = Some(sent);
    }

    /// Uploads the file at `path`.
    pub fn upload(
        &mut self,
        item: &LocalItem,
        path: &Path,
    ) -> Result<Result<RemoteItem, SyncError>> {
        let source = host::Source::open(path)?;
        let source = self.store.data_mut().table.push(source)?;
        let result = self.instance.onekwords_plugins_sync().call_upload(
            &mut self.store,
            &self.config,
            item,
            Resource::new_borrow(source.rep()),
        );
        self.store.data_mut().table.delete(source).ok();
        result.map_err(|e| anyhow!("{e:?}"))
    }

    /// Starts uploading a file in parts, returning the upload's id.
    pub fn begin_upload(&mut self, item: &LocalItem) -> Result<Result<String, SyncError>> {
        self.instance
            .onekwords_plugins_sync()
            .call_begin_upload(&mut self.store, &self.config, item)
            .map_err(|e| anyhow!("{e:?}"))
    }

    /// Uploads part `part` (from 1) of the file at `path`: `len` bytes from
    /// `offset`.
    pub fn upload_part(
        &mut self,
        item: &LocalItem,
        upload: &str,
        part: u32,
        path: &Path,
        offset: u64,
        len: u64,
    ) -> Result<Result<String, SyncError>> {
        let source = host::Source::open_part(path, offset, len)?;
        let source = self.store.data_mut().table.push(source)?;
        let result = self.instance.onekwords_plugins_sync().call_upload_part(
            &mut self.store,
            &self.config,
            item,
            upload,
            part,
            Resource::new_borrow(source.rep()),
        );
        self.store.data_mut().table.delete(source).ok();
        result.map_err(|e| anyhow!("{e:?}"))
    }

    pub fn finish_upload(
        &mut self,
        item: &LocalItem,
        upload: &str,
        parts: &[String],
    ) -> Result<Result<RemoteItem, SyncError>> {
        self.instance
            .onekwords_plugins_sync()
            .call_finish_upload(&mut self.store, &self.config, item, upload, parts)
            .map_err(|e| anyhow!("{e:?}"))
    }

    pub fn abort_upload(
        &mut self,
        item: &LocalItem,
        upload: &str,
    ) -> Result<Result<(), SyncError>> {
        self.instance
            .onekwords_plugins_sync()
            .call_abort_upload(&mut self.store, &self.config, item, upload)
            .map_err(|e| anyhow!("{e:?}"))
    }
}

impl SyncError {
    pub fn message(&self) -> &str {
        match self {
            SyncError::Config(message)
            | SyncError::Transient(message)
            | SyncError::Rejected(message) => message,
            SyncError::Unsupported => "The plugin doesn't support this.",
        }
    }
}

/// A compiled detection plugin.
pub struct Detector {
    pub info: ProviderInfo,
    runtime: Arc<Runtime>,
    pre: DetectPluginPre<host::State>,
}

impl Detector {
    fn new(runtime: &Arc<Runtime>, pre: DetectPluginPre<host::State>) -> Result<Self> {
        let mut store = runtime.store("", Vec::new());
        let instance = pre
            .instantiate(&mut store)
            .map_err(|e| anyhow!("{e:?}"))
            .context("starting the plugin")?;
        let info = instance
            .onekwords_plugins_detect()
            .call_info(&mut store)
            .map_err(|e| anyhow!("{e:?}"))?;
        if info.id.is_empty() {
            return Err(anyhow!("the plugin has no id"));
        }
        Ok(Self {
            info,
            runtime: runtime.clone(),
            pre,
        })
    }

    /// Starts an instance of the plugin with `config`. It may only reach
    /// the hosts in its `info.hosts` and its URL settings.
    pub fn session(&self, config: Config) -> Result<DetectSession> {
        let hosts = allowed_hosts(&self.info.hosts, &self.info.config, &config);
        let mut store = self.runtime.store(&self.info.id, hosts);
        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| anyhow!("{e:?}"))
            .context("starting the plugin")?;
        Ok(DetectSession {
            store,
            instance,
            config,
        })
    }
}

/// A running instance of a detection plugin. Like `Session`, `Err` means
/// the plugin crashed.
pub struct DetectSession {
    store: Store<host::State>,
    instance: DetectPlugin,
    config: Config,
}

impl DetectSession {
    pub fn validate(&mut self) -> Result<Result<(), DetectError>> {
        self.instance
            .onekwords_plugins_detect()
            .call_validate(&mut self.store, &self.config)
            .map_err(|e| anyhow!("{e:?}"))
    }

    pub fn detect(&mut self, image: &Image) -> Result<Result<Vec<DetectedObject>, DetectError>> {
        self.instance
            .onekwords_plugins_detect()
            .call_detect(&mut self.store, &self.config, image)
            .map_err(|e| anyhow!("{e:?}"))
    }
}

impl DetectError {
    pub fn message(&self) -> &str {
        match self {
            DetectError::Config(message)
            | DetectError::Transient(message)
            | DetectError::Rejected(message) => message,
        }
    }
}

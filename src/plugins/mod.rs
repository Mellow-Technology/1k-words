//! Sync plugins: WebAssembly components, run with Wasmtime, that copy a
//! library's files to and from external services. Their interface is
//! `wit/sync.wit`.
//!
//! Plugins are sandboxed: they get no file system, environment or sockets,
//! only what `host` provides — reading the file being synced, and HTTP
//! requests to the hosts their settings name. The built-in plugins
//! (`plugins/*`, built by `build.rs`) are embedded in the app; more can be
//! added as `.wasm` files in the app's `plugins` folder.
//!
//! A plugin only moves files. What's been synced where is recorded in the
//! library's catalog (see `library::sync`).

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
            "onekwords:sync/files.source": super::host::Source,
            "onekwords:sync/files.sink": super::host::Sink,
        },
    });
}

pub use bindings::exports::onekwords::sync::plugin::{
    Capabilities, ConfigField, FieldKind, LocalItem, PluginInfo, RemoteItem, SyncError,
};
use bindings::{SyncPlugin, SyncPluginPre};

/// The most memory a plugin instance may use.
const MEMORY_LIMIT: usize = 256 << 20;

/// The plugins built into the app, by `build.rs`.
const BUILT_IN: &[(&str, &[u8])] = &[(
    "s3",
    include_bytes!(concat!(env!("OUT_DIR"), "/plugins/s3.wasm")),
)];

/// A target's settings for its plugin, by `ConfigField::key`.
pub type Config = Vec<(String, String)>;

/// The loaded plugins.
pub struct Plugins {
    plugins: Vec<Arc<Plugin>>,
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
            failed: Vec::new(),
        };
        for (name, wasm) in BUILT_IN {
            plugins.add(name.to_string(), Plugin::new(&runtime, wasm));
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
                .and_then(|wasm| Plugin::new(&runtime, &wasm));
            plugins.add(path.display().to_string(), plugin);
        }
        Ok(plugins)
    }

    fn add(&mut self, source: String, plugin: Result<Plugin>) {
        match plugin {
            Ok(plugin) => {
                self.plugins.retain(|p| p.info.id != plugin.info.id);
                self.plugins.push(Arc::new(plugin));
            }
            Err(error) => self.failed.push((source, format!("{error:#}"))),
        }
    }

    pub fn all(&self) -> &[Arc<Plugin>] {
        &self.plugins
    }

    pub fn get(&self, id: &str) -> Option<&Arc<Plugin>> {
        self.plugins.iter().find(|plugin| plugin.info.id == id)
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
        SyncPlugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|e| anyhow!("{e:?}"))?;
        Ok(Self { engine, linker })
    }
}

/// A compiled plugin.
pub struct Plugin {
    pub info: PluginInfo,
    runtime: Arc<Runtime>,
    pre: SyncPluginPre<host::State>,
}

impl Plugin {
    fn new(runtime: &Arc<Runtime>, wasm: &[u8]) -> Result<Self> {
        let component = Component::new(&runtime.engine, wasm)
            .map_err(|e| anyhow!("{e:?}"))
            .context("compiling the plugin")?;
        let pre = runtime
            .linker
            .instantiate_pre(&component)
            .and_then(SyncPluginPre::new)
            .map_err(|e| anyhow!("{e:?}"))
            .context("linking the plugin")?;
        let mut plugin = Self {
            info: PluginInfo {
                id: String::new(),
                name: String::new(),
                version: String::new(),
                capabilities: Capabilities::empty(),
                config: Vec::new(),
                hosts: Vec::new(),
            },
            runtime: runtime.clone(),
            pre,
        };
        let (mut store, instance) = plugin.instantiate(Vec::new())?;
        plugin.info = instance
            .onekwords_sync_plugin()
            .call_info(&mut store)
            .map_err(|e| anyhow!("{e:?}"))?;
        if plugin.info.id.is_empty() {
            return Err(anyhow!("the plugin has no id"));
        }
        Ok(plugin)
    }

    fn instantiate(&self, hosts: Vec<String>) -> Result<(Store<host::State>, SyncPlugin)> {
        let state = host::State {
            plugin: self.info.id.clone(),
            wasi: WasiCtx::builder().build(),
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new().memory_size(MEMORY_LIMIT).build(),
            http: host::Http::new(hosts),
            sent: None,
        };
        let mut store = Store::new(&self.runtime.engine, state);
        store.limiter(|state| &mut state.limits);
        let instance = self
            .pre
            .instantiate(&mut store)
            .map_err(|e| anyhow!("{e:?}"))
            .context("starting the plugin")?;
        Ok((store, instance))
    }

    /// Starts an instance of the plugin for a target with `config`. It may
    /// only reach the hosts in its `info.hosts` and its URL settings.
    pub fn session(&self, config: Config) -> Result<Session> {
        let mut hosts = self.info.hosts.clone();
        for field in &self.info.config {
            if field.kind != FieldKind::Url {
                continue;
            }
            let url = config.iter().find(|(key, _)| *key == field.key);
            if let Some(host) = url.and_then(|(_, url)| host::host_of(url)) {
                hosts.push(host);
            }
        }
        let (store, instance) = self.instantiate(hosts)?;
        Ok(Session {
            store,
            instance,
            config,
        })
    }
}

/// A running instance of a plugin, for one target. Calls return the
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
            .onekwords_sync_plugin()
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
        let result = self.instance.onekwords_sync_plugin().call_upload(
            &mut self.store,
            &self.config,
            item,
            Resource::new_borrow(source.rep()),
        );
        self.store.data_mut().table.delete(source).ok();
        result.map_err(|e| anyhow!("{e:?}"))
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

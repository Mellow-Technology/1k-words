//! What the app provides to plugins: the imports in `wit/sync.wit`.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::Result;
use ureq::http;
use wasmtime::StoreLimits;
use wasmtime::component::{Resource, ResourceTable};
use wasmtime_wasi::{WasiCtx, WasiCtxView, WasiView};

use super::bindings::onekwords::sync::{files, http as plugin_http, log};

/// The most a plugin may read from a file at once.
const MAX_READ: u32 = 4 << 20;
/// The largest response body kept in memory for a plugin.
const MAX_RESPONSE: u64 = 16 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// Without data moving either way.
const IDLE_TIMEOUT: Duration = Duration::from_secs(120);

/// A plugin instance's store data.
pub struct State {
    /// The plugin's id, for its log messages.
    pub plugin: String,
    pub wasi: WasiCtx,
    pub table: ResourceTable,
    pub limits: StoreLimits,
    pub http: Http,
    /// Counts the bytes of files sent (see `Session::count_sent`).
    pub sent: Option<Arc<AtomicU64>>,
}

impl WasiView for State {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

/// A file a plugin may read.
pub struct Source {
    path: PathBuf,
    file: File,
    size: u64,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let file = File::open(path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path: path.to_path_buf(),
            file,
            size,
        })
    }
}

/// A file a plugin may write.
pub struct Sink {
    file: File,
}

impl log::Host for State {
    fn write(&mut self, level: log::Level, message: String) {
        let level = match level {
            log::Level::Debug => return,
            log::Level::Info => "info",
            log::Level::Warn => "warning",
            log::Level::Error => "error",
        };
        eprintln!("{} plugin {level}: {message}", self.plugin);
    }
}

impl files::Host for State {}

impl files::HostSource for State {
    fn size(&mut self, source: Resource<Source>) -> u64 {
        self.table.get(&source).map_or(0, |source| source.size)
    }

    fn read(&mut self, source: Resource<Source>, offset: u64, len: u32) -> Result<Vec<u8>, String> {
        let source = self.table.get_mut(&source).map_err(|e| e.to_string())?;
        let len = len.min(MAX_READ) as u64;
        let len = len.min(source.size.saturating_sub(offset));
        let mut bytes = Vec::with_capacity(len as usize);
        source
            .file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| (&source.file).take(len).read_to_end(&mut bytes))
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    fn drop(&mut self, source: Resource<Source>) -> wasmtime::Result<()> {
        self.table.delete(source)?;
        Ok(())
    }
}

impl files::HostSink for State {
    fn write(&mut self, sink: Resource<Sink>, bytes: Vec<u8>) -> Result<(), String> {
        let sink = self.table.get_mut(&sink).map_err(|e| e.to_string())?;
        sink.file.write_all(&bytes).map_err(|e| e.to_string())
    }

    fn drop(&mut self, sink: Resource<Sink>) -> wasmtime::Result<()> {
        self.table.delete(sink)?;
        Ok(())
    }
}

/// HTTP for a plugin, limited to some hosts.
pub struct Http {
    agent: ureq::Agent,
    /// Requests may go to these hosts and their subdomains.
    hosts: Vec<String>,
}

impl Http {
    pub fn new(hosts: Vec<String>) -> Self {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            // A redirect could lead anywhere; plugins follow them themselves.
            .max_redirects(0)
            .timeout_connect(Some(CONNECT_TIMEOUT))
            .timeout_recv_response(Some(IDLE_TIMEOUT))
            .timeout_recv_body(Some(IDLE_TIMEOUT))
            .timeout_send_body(Some(IDLE_TIMEOUT))
            .build()
            .into();
        let hosts = hosts.into_iter().map(|h| h.to_ascii_lowercase()).collect();
        Self { agent, hosts }
    }

    /// Checks the plugin may make `request`, and builds it.
    fn request(&self, request: &plugin_http::Request) -> Result<http::request::Builder, String> {
        let uri: http::Uri = request.url.parse().map_err(|_| "invalid URL".to_owned())?;
        if !matches!(uri.scheme_str(), Some("http" | "https")) {
            return Err("only http and https URLs are allowed".into());
        }
        let host = uri.host().unwrap_or_default().to_ascii_lowercase();
        let allowed = self.hosts.iter().any(|h| {
            host == *h
                || host
                    .strip_suffix(h.as_str())
                    .is_some_and(|s| s.ends_with('.'))
        });
        if !allowed {
            return Err(format!("the plugin may not connect to {host}"));
        }
        let method = http::Method::from_bytes(request.method.as_bytes())
            .map_err(|_| format!("invalid method {}", request.method))?;
        let mut builder = http::Request::builder().method(method).uri(uri);
        for (name, value) in &request.headers {
            // The app sets these for the body it sends.
            if name.eq_ignore_ascii_case("content-length")
                || name.eq_ignore_ascii_case("transfer-encoding")
            {
                continue;
            }
            builder = builder.header(name, value);
        }
        Ok(builder)
    }
}

fn response_head(response: &http::Response<ureq::Body>) -> plugin_http::Response {
    plugin_http::Response {
        status: response.status().as_u16(),
        headers: response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_owned(),
                    String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )
            })
            .collect(),
        body: Vec::new(),
    }
}

fn read_body(mut response: http::Response<ureq::Body>) -> Result<plugin_http::Response, String> {
    let mut head = response_head(&response);
    head.body = response
        .body_mut()
        .with_config()
        .limit(MAX_RESPONSE)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok(head)
}

impl plugin_http::Host for State {
    fn send(
        &mut self,
        request: plugin_http::Request,
        body: Vec<u8>,
    ) -> Result<plugin_http::Response, String> {
        let request = self
            .http
            .request(&request)?
            .header("content-length", body.len())
            .body(body)
            .map_err(|e| e.to_string())?;
        let response = self.http.agent.run(request).map_err(|e| e.to_string())?;
        read_body(response)
    }

    fn send_file(
        &mut self,
        request: plugin_http::Request,
        body: Resource<Source>,
    ) -> Result<plugin_http::Response, String> {
        let source = self.table.get(&body).map_err(|e| e.to_string())?;
        // A separate handle, so the plugin's reads don't move this one.
        let file = File::open(&source.path).map_err(|e| e.to_string())?;
        let size = source.size;
        let request = self
            .http
            .request(&request)?
            .header("content-length", size)
            .body(ureq::SendBody::from_owned_reader(Counting {
                inner: file.take(size),
                sent: self.sent.clone(),
            }))
            .map_err(|e| e.to_string())?;
        let response = self.http.agent.run(request).map_err(|e| e.to_string())?;
        read_body(response)
    }

    fn receive_file(
        &mut self,
        request: plugin_http::Request,
        into: Resource<Sink>,
    ) -> Result<plugin_http::Response, String> {
        let request = self
            .http
            .request(&request)?
            .body(())
            .map_err(|e| e.to_string())?;
        let mut response = self.http.agent.run(request).map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return read_body(response);
        }
        let head = response_head(&response);
        let sink = self.table.get_mut(&into).map_err(|e| e.to_string())?;
        std::io::copy(&mut response.body_mut().as_reader(), &mut sink.file)
            .map_err(|e| e.to_string())?;
        Ok(head)
    }
}

/// A file being sent, counting its bytes as they're read.
struct Counting<R> {
    inner: R,
    sent: Option<Arc<AtomicU64>>,
}

impl<R: std::io::Read> std::io::Read for Counting<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if let Some(sent) = &self.sent {
            sent.fetch_add(read as u64, Ordering::Relaxed);
        }
        Ok(read)
    }
}

/// The host of `url`, if it's an http or https URL.
pub fn host_of(url: &str) -> Option<String> {
    let uri: http::Uri = url.trim().parse().ok()?;
    matches!(uri.scheme_str(), Some("http" | "https"))
        .then(|| uri.host().map(str::to_ascii_lowercase))
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(url: &str) -> plugin_http::Request {
        plugin_http::Request {
            method: "GET".into(),
            url: url.into(),
            headers: Vec::new(),
        }
    }

    #[test]
    fn allows_only_listed_hosts_and_their_subdomains() {
        let http = Http::new(vec![host_of("https://S3.example.com:9000/").unwrap()]);
        for url in [
            "https://s3.example.com/bucket/key",
            "http://bucket.s3.example.com:9000/key",
        ] {
            assert!(http.request(&request(url)).is_ok(), "{url}");
        }
        for url in [
            "https://example.com/",
            "https://evil-s3.example.com/",
            "https://s3.example.com.evil.net/",
            "ftp://s3.example.com/",
            "not a url",
        ] {
            assert!(http.request(&request(url)).is_err(), "{url}");
        }
    }
}

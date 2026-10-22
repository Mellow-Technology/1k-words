//! A sync plugin that uploads to S3 and S3-compatible storage (AWS, Cloudflare
//! R2, Backblaze B2, MinIO, Wasabi...).
//!
//! Each file is stored under the target's prefix by the path the app gives
//! it (the library's folder, then the file's path in the library), e.g.
//! `photos/1K Words-3f2a9c1e/Originals/2026/10/15/IMG_0001.JPG`.

mod sigv4;

use std::time::{SystemTime, UNIX_EPOCH};

use exports::onekwords::sync::plugin::{
    Capabilities, Config, ConfigField, FieldKind, Guest, Listing, LocalItem, PluginInfo,
    RemoteItem, SyncError,
};
use onekwords::sync::files::{Sink, Source};
use onekwords::sync::http::{self, Request, Response};

wit_bindgen::generate!({
    path: "../../wit",
    world: "sync-plugin",
});

/// How much of a file is read at a time, to hash it.
const READ_SIZE: u32 = 1 << 20;

struct S3;

impl Guest for S3 {
    fn info() -> PluginInfo {
        let field = |key: &str, label: &str, kind, required, default: Option<&str>, help: &str| {
            ConfigField {
                key: key.into(),
                label: label.into(),
                kind,
                required,
                default: default.map(Into::into),
                help: Some(help.into()).filter(|help: &String| !help.is_empty()),
            }
        };
        PluginInfo {
            id: "s3".into(),
            name: "S3-Compatible Storage".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            capabilities: Capabilities::UPLOAD,
            config: vec![
                field(
                    "endpoint",
                    "Endpoint",
                    FieldKind::Url,
                    true,
                    None,
                    "e.g. https://s3.us-east-1.amazonaws.com or https://<account>.r2.cloudflarestorage.com",
                ),
                field(
                    "region",
                    "Region",
                    FieldKind::Text,
                    true,
                    Some("us-east-1"),
                    "Cloudflare R2 uses \"auto\".",
                ),
                field("bucket", "Bucket", FieldKind::Text, true, None, ""),
                field(
                    "prefix",
                    "Folder",
                    FieldKind::Text,
                    false,
                    None,
                    "Where in the bucket to put the library, e.g. photos/",
                ),
                field(
                    "access_key_id",
                    "Access Key ID",
                    FieldKind::Text,
                    true,
                    None,
                    "",
                ),
                field(
                    "secret_access_key",
                    "Secret Access Key",
                    FieldKind::Secret,
                    true,
                    None,
                    "",
                ),
                field(
                    "virtual_hosted",
                    "Virtual-hosted–style URLs",
                    FieldKind::Flag,
                    false,
                    Some("false"),
                    "Address the bucket as <bucket>.<endpoint> rather than <endpoint>/<bucket>.",
                ),
            ],
            hosts: Vec::new(),
        }
    }

    fn validate(config: Config) -> Result<(), SyncError> {
        let target = Target::new(&config)?;
        let response = target.send("HEAD", "", &[], &sigv4::sha256_hex(b""), |request| {
            http::send(&request, &[])
        })?;
        match response.status {
            200..=299 => Ok(()),
            403 => Err(SyncError::Config(
                "Access denied. Check the access key and secret, and that they can use the bucket."
                    .into(),
            )),
            404 => Err(SyncError::Config(format!(
                "There's no bucket named \"{}\".",
                target.bucket
            ))),
            301 | 400 => Err(SyncError::Config(
                "The bucket isn't in that region, or the endpoint is wrong.".into(),
            )),
            _ => Err(error_from(&response)),
        }
    }

    fn upload(config: Config, item: LocalItem, data: &Source) -> Result<RemoteItem, SyncError> {
        let target = Target::new(&config)?;
        let key = format!("{}{}", target.prefix, item.path);
        let hash = sha256_of(data)?;
        let headers = [("content-type".to_owned(), item.media_type.clone())];
        let response = target.send("PUT", &key, &headers, &hash, |request| {
            http::send_file(&request, data)
        })?;
        if !(200..300).contains(&response.status) {
            return Err(error_from(&response));
        }
        let etag = header(&response, "etag").map(|etag| etag.trim_matches('"').to_owned());
        Ok(RemoteItem {
            key,
            path: Some(item.path),
            version: etag,
            size: Some(item.size),
        })
    }

    fn download(_: Config, _: RemoteItem, _: &Sink) -> Result<(), SyncError> {
        Err(SyncError::Unsupported)
    }

    fn list(_: Config, _: Option<String>) -> Result<Listing, SyncError> {
        Err(SyncError::Unsupported)
    }

    fn delete(_: Config, _: RemoteItem) -> Result<(), SyncError> {
        Err(SyncError::Unsupported)
    }
}

export!(S3);

/// A bucket, from a target's settings.
struct Target<'a> {
    /// `http://` or `https://`.
    scheme: &'a str,
    /// The endpoint's host (and port).
    authority: &'a str,
    region: &'a str,
    bucket: &'a str,
    /// Empty, or ending in `/`.
    prefix: String,
    access_key_id: &'a str,
    secret_access_key: &'a str,
    virtual_hosted: bool,
}

impl<'a> Target<'a> {
    fn new(config: &'a Config) -> Result<Self, SyncError> {
        let get = |key: &str| {
            config
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, value)| value.trim())
                .filter(|value| !value.is_empty())
        };
        let required = |key: &str, label: &str| {
            get(key).ok_or_else(|| SyncError::Config(format!("{label} is required.")))
        };
        let endpoint = required("endpoint", "Endpoint")?;
        let (scheme, rest) = endpoint
            .split_once("://")
            .filter(|(scheme, _)| matches!(*scheme, "http" | "https"))
            .ok_or_else(|| {
                SyncError::Config("The endpoint must start with https:// or http://.".into())
            })?;
        let authority = rest.trim_end_matches('/');
        if authority.is_empty() || authority.contains(['/', '?', '#', '@']) {
            return Err(SyncError::Config(
                "The endpoint should be just a host, like https://s3.amazonaws.com.".into(),
            ));
        }
        let prefix = get("prefix").unwrap_or("").trim_matches('/');
        Ok(Self {
            scheme,
            authority,
            region: required("region", "Region")?,
            bucket: required("bucket", "Bucket")?,
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            access_key_id: required("access_key_id", "Access Key ID")?,
            secret_access_key: required("secret_access_key", "Secret Access Key")?,
            virtual_hosted: get("virtual_hosted") == Some("true"),
        })
    }

    /// Signs and sends a request for the object `key` (or the bucket, if
    /// empty), with the body whose hash is `payload_hash`.
    fn send(
        &self,
        method: &str,
        key: &str,
        headers: &[(String, String)],
        payload_hash: &str,
        send: impl FnOnce(Request) -> Result<Response, http::Error>,
    ) -> Result<Response, SyncError> {
        let (host, path) = if self.virtual_hosted {
            (
                format!("{}.{}", self.bucket, self.authority),
                format!("/{}", sigv4::encode(key, false)),
            )
        } else if key.is_empty() {
            (
                self.authority.to_owned(),
                format!("/{}", sigv4::encode(self.bucket, true)),
            )
        } else {
            (
                self.authority.to_owned(),
                format!(
                    "/{}/{}",
                    sigv4::encode(self.bucket, true),
                    sigv4::encode(key, false)
                ),
            )
        };
        let seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let amz_date = sigv4::amz_date(seconds);
        let mut headers = headers.to_vec();
        headers.extend([
            ("host".to_owned(), host.clone()),
            ("x-amz-date".to_owned(), amz_date.clone()),
            ("x-amz-content-sha256".to_owned(), payload_hash.to_owned()),
        ]);
        let authorization = sigv4::authorization(
            &sigv4::Request {
                method,
                path: &path,
                query: &[],
                headers: &headers,
                payload_hash,
            },
            &sigv4::Credentials {
                access_key_id: self.access_key_id,
                secret_access_key: self.secret_access_key,
            },
            self.region,
            "s3",
            &amz_date,
        );
        headers.push(("authorization".to_owned(), authorization));
        send(Request {
            method: method.to_owned(),
            url: format!("{}://{host}{path}", self.scheme),
            headers,
        })
        .map_err(SyncError::Transient)
    }
}

fn sha256_of(data: &Source) -> Result<String, SyncError> {
    use sha2::{Digest, Sha256};
    let size = data.size();
    let mut hasher = Sha256::new();
    let mut offset = 0;
    while offset < size {
        let chunk = data.read(offset, READ_SIZE).map_err(SyncError::Transient)?;
        if chunk.is_empty() {
            return Err(SyncError::Transient(
                "the file got shorter while reading it".into(),
            ));
        }
        hasher.update(&chunk);
        offset += chunk.len() as u64;
    }
    Ok(sigv4::hex(&hasher.finalize()))
}

fn header<'a>(response: &'a Response, name: &str) -> Option<&'a str> {
    response
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

/// Classifies a failed response by its status and S3 error code.
fn error_from(response: &Response) -> SyncError {
    let body = String::from_utf8_lossy(&response.body);
    let tag = |name: &str| {
        let start = body.find(&format!("<{name}>"))? + name.len() + 2;
        let end = start + body[start..].find(&format!("</{name}>"))?;
        Some(body[start..end].to_owned())
    };
    let code = tag("Code");
    let message = match (&code, tag("Message")) {
        (Some(code), Some(message)) => format!("{code}: {message}"),
        (Some(code), None) => code.clone(),
        _ => format!("HTTP {}", response.status),
    };
    match (response.status, code.as_deref()) {
        (_, Some("SlowDown" | "RequestTimeout" | "RequestTimeTooSkewed" | "InternalError")) => {
            SyncError::Transient(message)
        }
        (
            _,
            Some(
                "AccessDenied"
                | "InvalidAccessKeyId"
                | "SignatureDoesNotMatch"
                | "NoSuchBucket"
                | "AuthorizationHeaderMalformed"
                | "PermanentRedirect",
            ),
        )
        | (401 | 403 | 301, _) => SyncError::Config(message),
        (408 | 429 | 500..=599, _) => SyncError::Transient(message),
        _ => SyncError::Rejected(message),
    }
}

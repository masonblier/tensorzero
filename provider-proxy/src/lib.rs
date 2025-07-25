//! An HTTP/HTTPS proxy that caches non-error responses to disk.
//! Heavily based on https://github.com/hatoo/http-mitm-proxy (MIT-licensed),
//! with the openssl dependency and `default_client` removed.
#![expect(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

mod mitm_server;
mod streaming_body_collector;
mod tls;

use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use bytes::{Bytes, BytesMut};
use clap::{Parser, ValueEnum};
use http::{HeaderName, HeaderValue, Version};
use http_body_util::{combinators::BoxBody, BodyExt, Full};
use hyper::service::service_fn;
use mitm_server::MitmProxy;
use moka::sync::Cache;
use serde::Serialize;
use sha2::{Digest, Sha256};
use streaming_body_collector::StreamingBodyCollector;
use tokio::sync::oneshot;
use tracing::level_filters::LevelFilter;

const CACHE_HEADER_NAME: &str = "x-tensorzero-provider-proxy-cache";

fn make_root_cert() -> rcgen::Issuer<'static, rcgen::KeyPair> {
    let mut param = rcgen::CertificateParams::default();

    param.distinguished_name = rcgen::DistinguishedName::new();
    param.distinguished_name.push(
        rcgen::DnType::CommonName,
        rcgen::DnValue::Utf8String("<HTTP-MITM-PROXY CA>".to_string()),
    );
    param.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
    ];
    param.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);

    let key_pair = rcgen::KeyPair::generate().unwrap();
    rcgen::Issuer::new(param, key_pair)
}

fn hash_value(request: &serde_json::Value) -> Result<String, anyhow::Error> {
    let mut hasher = Sha256::new();
    hasher.update(
        serde_json::to_string(&request).with_context(|| "Failed to stringify request json")?,
    );
    Ok(hex::encode(hasher.finalize()))
}

fn save_cache_body(
    path: PathBuf,
    parts: http::response::Parts,
    body: BytesMut,
) -> Result<(), anyhow::Error> {
    let path_str = path.to_string_lossy().into_owned();
    tracing::info!(path = path_str, "Finished processing request");

    // None of our providers produce image/pdf responses, so this is good enough to exclude
    // things like file fetching (which happen to use the proxied HTTP client in the gateway)
    if let Some(content_type) = parts.headers.get(http::header::CONTENT_TYPE) {
        if content_type.to_str().unwrap().starts_with("image/")
            || content_type
                .to_str()
                .unwrap()
                .starts_with("application/pdf")
        {
            tracing::info!("Skipping caching of response with content type {content_type:?}");
            return Ok(());
        }
    }

    #[derive(Serialize)]
    #[serde(untagged)]
    enum BodyKind {
        Bytes(Bytes),
        String(String),
    }

    let mut reconstructed = match String::from_utf8(body.to_vec()) {
        Ok(body_str) => hyper::Response::from_parts(parts, BodyKind::String(body_str)),
        Err(_) => hyper::Response::from_parts(parts, BodyKind::Bytes(body.into())),
    };
    reconstructed.extensions_mut().clear();
    let json_response =
        http_serde_ext::response::serialize(&reconstructed, serde_json::value::Serializer)
            .with_context(|| format!("Failed to serialize response for path {path_str}"))?;
    let json_str = serde_json::to_string(&json_response)
        .with_context(|| format!("Failed to stringify response for path {path_str}"))?;

    // Write the cache response to a temporary file, and then atomically rename it to the final path.
    // If we have multiple concurrent requests to the same path, one of them will win the race.
    // This is fine for our use case, as it shouldn't matter which successful (by HTTP status code)
    // response is cached.
    let mut tmpfile = tempfile::NamedTempFile::new()
        .with_context(|| format!("Failed to create tempfile for path {path_str}"))?;
    tmpfile
        .write_all(json_str.as_bytes())
        .with_context(|| format!("Failed to write to file for path {path_str}"))?;
    tmpfile
        .write_all(b"\n")
        .with_context(|| format!("Failed to write EOL newline to file for path {path_str}"))?;
    tmpfile
        .persist(&path)
        .with_context(|| format!("Failed to rename tempfile to {path_str}"))?;

    tracing::info!(path = path_str, "Wrote response to cache");
    Ok(())
}

const HEADER_TRUE: HeaderValue = HeaderValue::from_static("true");
const HEADER_FALSE: HeaderValue = HeaderValue::from_static("false");

async fn check_cache<
    E: std::fmt::Debug + 'static,
    T: Future<Output = Result<hyper::Response<BoxBody<Bytes, E>>, anyhow::Error>>,
    F: FnOnce() -> T,
>(
    start_time: std::time::SystemTime,
    args: &Args,
    mut request: hyper::Request<Bytes>,
    missing: F,
) -> Result<hyper::Response<BoxBody<Bytes, E>>, anyhow::Error> {
    request.extensions_mut().clear();
    let mut sanitized_header = false;
    if args.sanitize_bearer_auth {
        if let Some(auth_header) = request.headers().get("Authorization") {
            if auth_header.to_str().unwrap().starts_with("Bearer ") {
                request.headers_mut().insert(
                    "Authorization",
                    HeaderValue::from_static("Bearer TENSORZERO_PROVIDER_PROXY_TOKEN"),
                );
                sanitized_header = true;
            }
        }
    }
    if args.sanitize_aws_sigv4 {
        let header_names = [
            "authorization",
            "x-amz-date",
            "amz-sdk-invocation-id",
            "user-agent",
            "x-amz-user-agent",
            "amz-sdk-request",
        ];
        for header_name in &header_names {
            if request.headers().contains_key(*header_name) {
                request.headers_mut().insert(
                    *header_name,
                    HeaderValue::from_static("TENSORZERO_PROVIDER_PROXY_TOKEN"),
                );
                sanitized_header = true;
            }
        }
    }
    if args.sanitize_model_headers {
        let header_names = ["Modal-Key", "Modal-Secret"];
        for header_name in &header_names {
            if request.headers().contains_key(*header_name) {
                request.headers_mut().insert(
                    *header_name,
                    HeaderValue::from_static("TENSORZERO_PROVIDER_PROXY_TOKEN"),
                );
                sanitized_header = true;
            }
        }
    }
    let json_request = http_serde_ext::request::serialize(&request, serde_json::value::Serializer)
        .with_context(|| "Failed to serialize request")?;
    let hash = hash_value(&json_request)?;
    let filename = format!(
        "{}-{}",
        request.uri().host().expect("Missing request host"),
        hash
    );

    let path = args.cache_path.join(filename);
    let path_str = path.to_string_lossy().into_owned();

    let use_cache = || match args.mode {
        CacheMode::ReadOnly => Ok::<_, anyhow::Error>(true),
        CacheMode::ReadWrite => Ok(true),
        CacheMode::ReadOldWriteNew => {
            let file_mtime = std::fs::metadata(&path)
                .with_context(|| format!("Failed to read cache file metadata for {path_str}"))?
                .modified()
                .with_context(|| format!("Failed to read cache file mtime for {path_str}"))?;
            Ok(file_mtime <= start_time)
        }
    };

    let (mut resp, cache_hit) = if path.exists() && use_cache()? {
        tracing::info!(sanitized_header, "Cache hit: {}", path_str);
        let path_str_clone = path_str.clone();
        let resp = tokio::task::spawn_blocking(move || {
            let file = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read cache file {path_str}"))?;
            let response: serde_json::Value = serde_json::from_str(&file).with_context(|| {
                format!("Failed to deserialize response to JSON from {path_str}")
            })?;
            let response: hyper::Response<Bytes> = http_serde_ext::response::deserialize(response)
                .with_context(|| format!("Failed to deserialize HTTP response from {path_str}"))?;
            Ok::<_, anyhow::Error>(
                response.map(|b| BoxBody::new(Full::new(b).map_err(|e| match e {}))),
            )
        })
        .await
        .with_context(|| format!("Failed to await tokio spawn_blocking for {path_str_clone}"))??;
        (resp, HEADER_TRUE)
    } else {
        tracing::info!("Cache miss: {}", path_str);
        let response = match missing().await {
            Ok(response) => response,
            Err(e) => {
                tracing::error!(
                    e = e.as_ref() as &dyn std::error::Error,
                    "Failed to forward request"
                );
                let body = Full::new(Bytes::from(format!("Failed to forward request: {e:?}")));
                http::Response::builder()
                    .status(http::StatusCode::BAD_GATEWAY)
                    .body(BoxBody::new(body.map_err(|e| match e {})))
                    .with_context(|| "Failed to build response")?
            }
        };
        if response.status().is_success() {
            let (parts, body) = response.into_parts();
            let mut hyper_response = hyper::Response::from_parts(parts.clone(), body);
            // We need to clear the extensions in order to be able to serialize the response
            hyper_response.extensions_mut().clear();

            let write = match args.mode {
                CacheMode::ReadOnly => false,
                CacheMode::ReadWrite => true,
                CacheMode::ReadOldWriteNew => true,
            };

            // Start streaming the response to the client, running the provided callback once the whole body has been received
            // This lets us forward streaming responses without needing to wait for the entire response, while
            // still caching the entire response to disk.
            // Note that we make a `StreamingBodyCollector` even when `write` is false, so that
            // the HTTP behavior is consistent regardless of whether `write` is enabled.
            let body_collector = hyper_response.map(|b| {
                StreamingBodyCollector::new(
                    b,
                    Box::new(move |body| {
                        if write {
                            tokio::task::spawn_blocking(move || {
                                if let Err(e) = save_cache_body(path, parts, body) {
                                    tracing::error!(
                                        err = e.as_ref() as &dyn std::error::Error,
                                        "Failed to save cache body"
                                    );
                                }
                            });
                        }
                    }),
                )
            });

            (body_collector.map(|b| BoxBody::new(b)), HEADER_FALSE)
        } else {
            tracing::warn!("Skipping caching of non-success response: {:?}", response);
            (response, HEADER_FALSE)
        }
    };
    // Insert this header at the very end, to ensure that we never store this
    // header in the cache.
    resp.headers_mut().insert(CACHE_HEADER_NAME, cache_hit);
    Ok(resp)
}

#[derive(ValueEnum, Clone, Debug)]
pub enum CacheMode {
    /// Only read from the cache, never write to it.
    ReadOnly,
    /// Read from the cache, and write to it when a cache miss occurs.
    ReadWrite,
    /// Read entries from the cache that were created before the provider-proxy start time.
    /// Writes to the cache when a miss occurs, or if the cache entry was written after the provider-proxy start time
    /// (e.g. by this instance)
    /// This allows our e2e tests to retry when they get a bad response from the provider,
    /// without provider-proxy serving the cached bad response.
    ReadOldWriteNew,
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Path to the cache directory
    #[arg(long, default_value = "request_cache")]
    pub cache_path: PathBuf,
    /// Port to listen on
    #[arg(long, default_value = "3003")]
    pub port: u16,
    /// If `true`, replaces `Authorization: Bearer <token>` with `Authorization: Bearer TENSORZERO_PROVIDER_PROXY_TOKEN`
    /// when constructing a cache key.
    #[arg(long, default_value = "true")]
    pub sanitize_bearer_auth: bool,
    #[arg(long, default_value = "true")]
    pub sanitize_aws_sigv4: bool,
    #[arg(long, default_value = "true")]
    pub sanitize_model_headers: bool,
    #[arg(long, default_value = "read-old-write-new")]
    pub mode: CacheMode,
}

fn find_duplicate_header(headers: &http::HeaderMap) -> Option<HeaderName> {
    for header_name in headers.keys() {
        if headers.get_all(header_name).iter().count() > 1 {
            return Some(header_name.clone());
        }
    }
    None
}

fn is_openrouter_request(uri: &http::Uri) -> bool {
    uri.host()
        .map(|h| h.eq_ignore_ascii_case("openrouter.ai"))
        .unwrap_or(false)
}

pub async fn run_server(args: Args, server_started: oneshot::Sender<SocketAddr>) {
    use tracing_subscriber::EnvFilter;

    #[expect(clippy::print_stderr)]
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .try_init()
        .inspect_err(|e| eprintln!("Failed to initialize tracing: {e}"));

    let start_time = std::time::SystemTime::now();

    let args = Arc::new(args);

    std::fs::create_dir_all(&args.cache_path).expect("Failed to create cache directory");

    let _ = rustls::crypto::ring::default_provider()
        .install_default()
        .inspect_err(|e| tracing::error!("Failed to install rustls ring provider: {e:?}"));

    let root_cert = make_root_cert();

    let proxy = MitmProxy::new(
        // This is the root cert that will be used to sign the fake certificates
        Some(root_cert),
        Some(Cache::new(128)),
    );

    let client = reqwest::Client::new();
    let args_clone = args.clone();
    let (server_addr, server) = proxy
        .bind(
            ("127.0.0.1", args.port),
            service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let client = client.clone();
                let args = args_clone.clone();
                async move {
                    let (parts, body) = req.into_parts();

                    // On OpenRouter requests we want to take advantage of their custom headers identifying the referer.
                    // If these are missing, we fail with a bad request so an E2E test catches it in the CI.
                    tracing::debug!("Headers: {:?}", &parts.headers);
                    if is_openrouter_request(&parts.uri) {
                        let has_title = parts.headers.get("X-Title").map(|v| v.as_bytes() == b"TensorZero").unwrap_or(false);
                        let has_referer = parts.headers.get("HTTP-Referer").map(|v| v.as_bytes() == b"https://www.tensorzero.com/").unwrap_or(false);

                        if !has_title || !has_referer {
                            let missing = if !has_title && !has_referer {
                                "X-Title and HTTP-Referer"
                            } else if !has_title {
                                "X-Title"
                            } else {
                                "HTTP-Referer"
                            };

                            tracing::error!(url = ?parts.uri, "Missing or incorrect required header(s) for OpenRouter: {missing}");
                            return Ok(http::Response::builder()
                                .status(http::StatusCode::BAD_REQUEST)
                                .body(BoxBody::new(reqwest::Body::from(
                                    format!("provider-proxy: Missing or incorrect required header(s) for OpenRouter: {missing}"),
                                )))
                                .unwrap());
                        }
                    }
                    // While duplicate headers are allowed by the HTTP spec (the values get concatenated),
                    // we never intentionally send duplicate headers from tensorzero.
                    // We check for this and error to catch mistakes in our code
                    if let Some(header) = find_duplicate_header(&parts.headers) {
                        tracing::error!(url = ?parts.uri, "Duplicate header in request: `{header}`");
                        return Ok(http::Response::builder()
                            // Return a weird status code to increase the chances of this causing a test failure
                            .status(http::StatusCode::IM_A_TEAPOT)
                            .body(BoxBody::new(reqwest::Body::from(
                                format!("provider-proxy: Duplicate header: {header}"),
                            )))
                            .unwrap());
                    }
                    let body_bytes = body
                        .collect()
                        .await
                        .with_context(|| "Failed to collect body")?
                        .to_bytes();
                    let bytes_request = hyper::Request::from_parts(parts, body_bytes);
                    let response = check_cache(start_time, &args, bytes_request.clone(), || async {
                        let mut request: reqwest::Request =
                            bytes_request.try_into().with_context(|| {
                                "Failed to convert Request from `hyper` to `reqwest`"
                            })?;
                        // Don't explicitly request HTTP2 - let the connection upgrade if the
                        // remote server supports it
                        *request.version_mut() = Version::default();
                        Ok(http::Response::from(client.execute(request).await?).map(BoxBody::new))
                    })
                    .await?;

                    Ok::<_, anyhow::Error>(response)
                }
            }),
        )
        .await
        .unwrap();

    tracing::info!(?args, "HTTP Proxy is listening on http://{server_addr}");
    server_started
        .send(server_addr)
        .expect("Failed to send server started signal");
    server.await;
}

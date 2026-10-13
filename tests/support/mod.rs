//! In-process HTTP servers for the integration tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

pub async fn spawn(app: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Serves `bytes` at `/pkg` with byte ranges, `/latest` redirects there, and
/// `/plain` ignores ranges. Counts range requests and bytes served.
#[derive(Clone)]
pub struct PackageServer {
    pub bytes: Arc<Vec<u8>>,
    pub requests: Arc<AtomicUsize>,
    pub served: Arc<AtomicUsize>,
}

impl PackageServer {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Arc::new(bytes),
            requests: Arc::default(),
            served: Arc::default(),
        }
    }

    pub async fn start(&self) -> SocketAddr {
        let app = Router::new()
            .route("/pkg", get(range))
            .route(
                "/latest",
                get(|| async { (StatusCode::FOUND, [(header::LOCATION, "/pkg?token=abc")]) }),
            )
            .route("/plain", get(plain))
            .with_state(self.clone());
        spawn(app).await
    }
}

async fn range(State(server): State<PackageServer>, headers: HeaderMap) -> Response {
    server.requests.fetch_add(1, Ordering::SeqCst);
    let len = server.bytes.len();
    let Some((start, end)) = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
    else {
        return (StatusCode::OK, server.bytes.as_ref().clone()).into_response();
    };
    if start > end || end >= len {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    }
    server.served.fetch_add(end - start + 1, Ordering::SeqCst);
    Response::builder()
        .status(StatusCode::PARTIAL_CONTENT)
        .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
        .header(header::ETAG, "\"fixture-v1\"")
        .header(header::LAST_MODIFIED, "Tue, 29 Sep 2026 17:37:45 GMT")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=\"Fixture_6.4.0.xapk\"",
        )
        .body(Body::from(server.bytes[start..=end].to_vec()))
        .unwrap()
}

async fn plain(State(server): State<PackageServer>) -> Response {
    (StatusCode::OK, server.bytes.as_ref().clone()).into_response()
}

/// A stand-in for Haruki-Sekai-API's internal endpoints.
#[derive(Clone, Default)]
pub struct MockNode {
    pub token: String,
    /// region -> (appVersion, appHash)
    pub versions: Arc<Mutex<std::collections::BTreeMap<String, (String, String)>>>,
    pub posts: Arc<Mutex<Vec<Value>>>,
    pub auth_headers: Arc<Mutex<Vec<String>>>,
}

impl MockNode {
    pub fn new(token: &str) -> Self {
        Self {
            token: token.into(),
            ..Self::default()
        }
    }

    pub fn set(&self, region: &str, version: &str, hash: &str) {
        self.versions
            .lock()
            .unwrap()
            .insert(region.into(), (version.into(), hash.into()));
    }

    pub async fn start(&self) -> SocketAddr {
        let app = Router::new()
            .route("/internal/master/{server}/version", get(node_version))
            .route("/internal/app-identity", post(node_identity))
            .with_state(self.clone());
        spawn(app).await
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        let value = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        self.auth_headers.lock().unwrap().push(value.clone());
        value == format!("Bearer {}", self.token)
    }
}

/// Mirrors the node's zero-patch normalization for Nuverse regions.
fn effective(region: &str, version: &str) -> String {
    if matches!(region, "jp" | "en") {
        return version.into();
    }
    let parts: Vec<&str> = version.split('.').collect();
    if parts.len() == 3 {
        format!("{}.{}.0", parts[0], parts[1])
    } else {
        version.into()
    }
}

async fn node_version(
    State(node): State<MockNode>,
    Path(server): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !node.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let versions = node.versions.lock().unwrap();
    match versions.get(&server) {
        Some((version, hash)) => Json(json!({
            "appVersion": version, "appHash": hash, "dataVersion": "1.0.0.1",
            "assetVersion": "1.0.0.1", "assetHash": "", "cdnVersion": 0
        }))
        .into_response(),
        None => Json(json!({"ok": false, "kind": "not_found", "message": "unknown region"}))
            .into_response(),
    }
}

async fn node_identity(State(node): State<MockNode>, headers: HeaderMap, body: Bytes) -> Response {
    if !node.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let request: Value = serde_json::from_slice(&body).unwrap();
    node.posts.lock().unwrap().push(request.clone());
    let server = request["server"].as_str().unwrap().to_owned();
    let version = effective(&server, request["appVersion"].as_str().unwrap_or_default());
    let hash = request["appHash"].as_str().unwrap_or_default().to_owned();
    let mut versions = node.versions.lock().unwrap();
    let changed = versions.get(&server) != Some(&(version.clone(), hash.clone()));
    versions.insert(server, (version.clone(), hash.clone()));
    Json(json!({"ok": true, "data": {"changed": changed, "appVersion": version, "appHash": hash}}))
        .into_response()
}

/// Records webhook deliveries (headers + body).
#[derive(Clone, Default)]
pub struct HookReceiver {
    pub deliveries: Arc<Mutex<Vec<(HeaderMap, Bytes)>>>,
}

impl HookReceiver {
    pub async fn start(&self) -> SocketAddr {
        let app = Router::new()
            .route(
                "/hook",
                post(
                    |State(r): State<HookReceiver>, headers: HeaderMap, body: Bytes| async move {
                        r.deliveries.lock().unwrap().push((headers, body));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(self.clone());
        spawn(app).await
    }
}

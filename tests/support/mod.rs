//! In-process HTTP servers for the integration tests.
#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
    /// When set, `GET .../version` answers this bare status instead.
    pub version_status: Arc<Mutex<Option<StatusCode>>>,
    /// Stores Nuverse versions as posted, like a node with
    /// `pin_app_version_patch_to_zero = false` (default: zeroes the patch).
    pub keep_patch: Arc<AtomicBool>,
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
fn effective(region: &str, version: &str, keep_patch: bool) -> String {
    if keep_patch || matches!(region, "jp" | "en") {
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
    if let Some(status) = *node.version_status.lock().unwrap() {
        return status.into_response();
    }
    let versions = node.versions.lock().unwrap();
    match versions.get(&server) {
        Some((version, hash)) => Json(json!({
            "appVersion": version, "appHash": hash, "dataVersion": "1.0.0.1",
            "assetVersion": "1.0.0.1", "assetHash": "", "cdnVersion": 0
        }))
        .into_response(),
        // What the node answers when the version file is missing.
        None => Json(json!({
            "ok": false, "kind": "io",
            "message": "version file: No such file or directory (os error 2)"
        }))
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
    let version = effective(
        &server,
        request["appVersion"].as_str().unwrap_or_default(),
        node.keep_patch.load(Ordering::SeqCst),
    );
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

/// One app as the mock QooApp store reports it.
#[derive(Clone, Debug)]
pub struct QooApp {
    pub package_id: String,
    pub version_name: String,
    pub md5: String,
    /// When set, listed as the `UnityDataAssetPack` split at this URL.
    pub asset_pack_url: Option<String>,
}

/// A stand-in for the QooApp v11 store API and its download endpoint, which
/// redirects to `package_url`.
#[derive(Clone, Default)]
pub struct QooStore {
    pub token: String,
    /// app id -> app
    pub apps: Arc<Mutex<std::collections::BTreeMap<String, QooApp>>>,
    pub package_url: Arc<Mutex<String>>,
    pub store_requests: Arc<AtomicUsize>,
    pub downloads: Arc<AtomicUsize>,
    /// Headers of every store and download request.
    pub seen: Arc<Mutex<Vec<HeaderMap>>>,
}

impl QooStore {
    pub fn new(token: &str, package_url: String) -> Self {
        Self {
            token: token.into(),
            package_url: Arc::new(Mutex::new(package_url)),
            ..Self::default()
        }
    }

    pub fn set(&self, app_id: &str, app: QooApp) {
        self.apps.lock().unwrap().insert(app_id.into(), app);
    }

    pub async fn start(&self) -> SocketAddr {
        let app = Router::new()
            .route("/store/v11/apps/{id}", get(qoo_app))
            .route("/v11/apps/{package}/download", get(qoo_download))
            .with_state(self.clone());
        spawn(app).await
    }

    fn authorized(&self, headers: &HeaderMap) -> bool {
        self.seen.lock().unwrap().push(headers.clone());
        headers.get("x-user-token").and_then(|v| v.to_str().ok()) == Some(self.token.as_str())
    }
}

async fn qoo_app(
    State(store): State<QooStore>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    store.store_requests.fetch_add(1, Ordering::SeqCst);
    if !store.authorized(&headers) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"code": 401, "message": "login required"})),
        )
            .into_response();
    }
    let Some(app) = store.apps.lock().unwrap().get(&id).cloned() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let splits: Vec<Value> = app
        .asset_pack_url
        .iter()
        .map(|url| json!({"url": url, "md5": format!("{}-pack", app.md5)}))
        .chain(std::iter::once(
            json!({"url": "https://cdn.example.invalid/config.arm64_v8a.apk", "md5": "abi"}),
        ))
        .collect();
    Json(json!({
        "code": 0,
        "data": {
            "id": id,
            "packageId": app.package_id,
            "apk": {"versionName": app.version_name, "versionCode": 21901, "baseApkMd5": app.md5},
            "splitApks": splits,
        }
    }))
    .into_response()
}

async fn qoo_download(
    State(store): State<QooStore>,
    Path(package): Path<String>,
    headers: HeaderMap,
) -> Response {
    store.downloads.fetch_add(1, Ordering::SeqCst);
    if !store.authorized(&headers) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let target = format!(
        "{}?package={package}&sig=signed",
        store.package_url.lock().unwrap()
    );
    (StatusCode::FOUND, [(header::LOCATION, target)]).into_response()
}

/// A stand-in for a Haruki Sekai Asset Updater's
/// `PUT /v2/regions/{region}/app-version`: records every request and keeps the
/// Unity version per region. `fail_with` makes it answer that status instead.
#[derive(Clone, Default)]
pub struct MockUpdater {
    /// (region, headers, body) of every PUT.
    pub puts: Arc<Mutex<Vec<(String, HeaderMap, Value)>>>,
    pub unity: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
    pub fail_with: Arc<Mutex<Option<StatusCode>>>,
    /// Answer `200` with this raw body instead of the updater's JSON.
    pub raw_reply: Arc<Mutex<Option<(StatusCode, &'static str)>>>,
}

impl MockUpdater {
    pub async fn start(&self) -> SocketAddr {
        let app = Router::new()
            .route(
                "/v2/regions/{region}/app-version",
                axum::routing::put(updater_put),
            )
            .with_state(self.clone());
        spawn(app).await
    }
}

async fn updater_put(
    State(updater): State<MockUpdater>,
    Path(region): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    updater
        .puts
        .lock()
        .unwrap()
        .push((region.clone(), headers, body.clone()));
    if let Some((status, body)) = *updater.raw_reply.lock().unwrap() {
        return (status, body).into_response();
    }
    if let Some(status) = *updater.fail_with.lock().unwrap() {
        return (status, Json(json!({"message": "mock failure"}))).into_response();
    }
    let unity = body["unity_version"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let previous = updater.unity.lock().unwrap().insert(region, unity.clone());
    let message = if previous.as_deref() == Some(unity.as_str()) {
        "unchanged".to_owned()
    } else {
        format!("unity_version -> {unity}")
    };
    Json(json!({"message": message, "region": {}})).into_response()
}

//! Reader sync (SYNC_SPEC rev 3.2) hosted inside TailDrive under `/v1/reader`.
//!
//! The rev 3.1 paths are mounted unchanged below that prefix (`/v1/reader/v1/changes`, …), so a
//! client only swaps its base URL for the `url` in the pairing QR. Records live in their own
//! `reader.db`; book files share TailDrive's StorageBackend under `reader/sha256/<hex>` keys.

mod blobs;
mod error;
mod store;
pub mod wire;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{
    Extension, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State, rejection::BytesRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde::Deserialize;

use crate::{App, auth, config::Config, db::StartupResult, storage::StorageBackend};
use blobs::Blobs;
use error::{SyncError, SyncResult};
use store::{Device, Store};

pub(crate) struct Reader {
    store: Store,
    blobs: Blobs,
    pair_url: String,
    redeem_failures: Mutex<HashMap<String, Vec<Instant>>>,
    crash_after_commit: bool,
}

/// Debug builds only: `TAILDRIVE_READER_CRASH_AT=push_before_commit|push_after_commit` aborts the
/// process at that point of a push, for crash-recovery tests (SYNC_SPEC §12.3).
fn crash_hook() -> Option<String> {
    if cfg!(debug_assertions) {
        std::env::var("TAILDRIVE_READER_CRASH_AT").ok().filter(|v| !v.is_empty())
    } else {
        None
    }
}

impl Reader {
    pub(crate) async fn open(config: &Config, storage: Arc<dyn StorageBackend>) -> StartupResult<Arc<Self>> {
        tokio::fs::create_dir_all(config.data_dir.join("reader").join("uploads")).await?;
        let hook = crash_hook();
        let store = Store::open(config.data_dir.join("reader.db"), hook.as_deref() == Some("push_before_commit")).await?;
        let blobs = Blobs::new(storage, config.data_dir.clone(), config.free_floor_bytes, config.reader_book_quota_bytes);
        blobs.recover(&store).await.map_err(|e| format!("reader upload recovery failed: {}", e.message))?;
        Ok(Arc::new(Self {
            store,
            blobs,
            pair_url: format!("{}/v1/reader", config.reader_public_url.trim_end_matches('/')),
            redeem_failures: Mutex::new(HashMap::new()),
            crash_after_commit: hook.as_deref() == Some("push_after_commit"),
        }))
    }
}

#[derive(Clone, Copy)]
struct Peer(Option<SocketAddr>);

async fn peer_layer(mut request: Request, next: Next) -> Response {
    let peer = request.extensions().get::<ConnectInfo<SocketAddr>>().map(|ConnectInfo(addr)| *addr);
    request.extensions_mut().insert(Peer(peer));
    next.run(request).await
}

pub(crate) fn router() -> Router<App> {
    Router::new()
        .route("/v1/health", get(health))
        .route("/v1/pairing/tokens", post(issue_token))
        .route("/v1/pairing/redeem", post(redeem))
        .route("/v1/devices", get(list_devices))
        .route("/v1/devices/{device_id}", delete(revoke))
        .route("/v1/changes", get(pull).post(push))
        .route("/v1/conflicts", get(conflicts))
        .route("/v1/conflicts/{conflict_id}/resolve", post(resolve))
        .route("/v1/books", get(books).post(register))
        .route("/v1/blobs/{sha256}", get(get_blob).head(head_blob).put(put_blob))
        .fallback(async || SyncError::not_found("no such route"))
        .method_not_allowed_fallback(async || {
            SyncError::new(StatusCode::METHOD_NOT_ALLOWED, "bad_request", "method not allowed")
        })
        .layer(DefaultBodyLimit::max(store::MAX_BATCH_BYTES + 64 * 1024))
        .layer(middleware::from_fn(peer_layer))
}

fn reader(app: &App) -> Arc<Reader> {
    app.state.reader.clone()
}

fn json(status: StatusCode, bytes: Vec<u8>) -> Response {
    let mut response = (status, bytes).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let (scheme, token) = headers.get(header::AUTHORIZATION)?.to_str().ok()?.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then(|| token.trim().to_owned())
}

async fn device(reader: &Reader, headers: &HeaderMap) -> SyncResult<Device> {
    let token = bearer(headers).ok_or_else(|| SyncError::unauthorized("device credential required"))?;
    reader.store.run(move |conn, store| store::authenticate(conn, &token, store.clock.now())).await
}

/// Pairing and revocation accept a `mac` device credential, or TailDrive's own auth
/// (loopback with a Tailscale identity via `tailscale serve`, or a TailDrive API token).
async fn admin(app: &App, reader: &Reader, peer: Peer, headers: &HeaderMap) -> SyncResult<String> {
    if bearer(headers).is_some() {
        match device(reader, headers).await {
            Ok(device) if device.kind == "mac" => return Ok(device.id),
            Ok(_) => return Err(SyncError::forbidden("only mac devices can do this")),
            Err(_) => {}
        }
    }
    match auth::authenticate(peer.0, headers, &app.state.token_hashes) {
        Ok(principal) => Ok(format!("taildrive:{}", principal.tailnet_user.unwrap_or_else(|| "api-token".to_owned()))),
        Err(_) => Err(SyncError::unauthorized("a mac device credential or TailDrive authentication is required")),
    }
}

fn body_bytes(body: Result<Bytes, BytesRejection>) -> SyncResult<Bytes> {
    body.map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            SyncError::too_large("request body is too large")
        } else {
            SyncError::bad(error.body_text())
        }
    })
}

async fn health(State(app): State<App>) -> SyncResult<Response> {
    let bytes = reader(&app).store.run(|conn, _| store::health(conn)).await?;
    Ok(json(StatusCode::OK, bytes))
}

async fn issue_token(
    State(app): State<App>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> SyncResult<Response> {
    let reader = reader(&app);
    let issuer = admin(&app, &reader, peer, &headers).await?;
    let request: store::TokenRequest =
        serde_json::from_slice(&body_bytes(body)?).map_err(|e| SyncError::bad(format!("invalid body: {e}")))?;
    let url = reader.pair_url.clone();
    let bytes = reader
        .store
        .run(move |conn, store| store::issue_token(conn, &request.kind, &issuer, &url, store.clock.now()))
        .await?;
    Ok(json(StatusCode::OK, bytes))
}

async fn redeem(
    State(app): State<App>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> SyncResult<Response> {
    let reader = reader(&app);
    // Behind `tailscale serve` every peer is loopback, so the Tailscale login is part of the key.
    let key = format!(
        "{}|{}",
        peer.0.map(|p| p.ip().to_string()).unwrap_or_default(),
        headers.get("tailscale-user-login").and_then(|v| v.to_str().ok()).unwrap_or_default()
    );
    {
        let mut failures = reader.redeem_failures.lock().expect("rate map");
        let recent = failures.entry(key.clone()).or_default();
        recent.retain(|at| at.elapsed() < Duration::from_secs(60));
        if recent.len() >= 10 {
            return Err(SyncError::new(StatusCode::TOO_MANY_REQUESTS, "rate_limited", "too many failed redeems; wait a minute"));
        }
    }
    let body = body_bytes(body)?;
    let result = reader.store.run(move |conn, store| store::redeem(conn, &body, store.clock.now())).await;
    match result {
        Ok(bytes) => Ok(json(StatusCode::OK, bytes)),
        Err(error) => {
            if error.status == StatusCode::FORBIDDEN {
                reader.redeem_failures.lock().expect("rate map").entry(key).or_default().push(Instant::now());
            }
            Err(error)
        }
    }
}

async fn list_devices(State(app): State<App>, headers: HeaderMap) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    Ok(json(StatusCode::OK, reader.store.run(|conn, _| store::list_devices(conn)).await?))
}

async fn revoke(
    State(app): State<App>,
    Extension(peer): Extension<Peer>,
    headers: HeaderMap,
    Path(device_id): Path<String>,
) -> SyncResult<StatusCode> {
    let reader = reader(&app);
    admin(&app, &reader, peer, &headers).await?;
    reader.store.run(move |conn, store| store::revoke(conn, &device_id, store.clock.now())).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn push(State(app): State<App>, headers: HeaderMap, body: Result<Bytes, BytesRejection>) -> SyncResult<Response> {
    let reader = reader(&app);
    let device = device(&reader, &headers).await?;
    let body = body_bytes(body)?;
    let bytes = reader.store.run(move |conn, store| store::push(conn, store, &device.id, &body)).await?;
    if reader.crash_after_commit {
        tracing::warn!("TAILDRIVE_READER_CRASH_AT=push_after_commit: aborting before the response");
        std::process::abort();
    }
    Ok(json(StatusCode::OK, bytes))
}

#[derive(Deserialize)]
struct PullQuery {
    since: Option<i64>,
    limit: Option<i64>,
}

async fn pull(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<PullQuery>, axum::extract::rejection::QueryRejection>,
) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    let Query(query) = query.map_err(|e| SyncError::bad(e.body_text()))?;
    let since = query.since.ok_or_else(|| SyncError::bad("since is required"))?;
    let limit = query.limit.unwrap_or(500);
    Ok(json(StatusCode::OK, reader.store.run(move |conn, _| store::pull(conn, since, limit)).await?))
}

#[derive(Deserialize)]
struct ConflictQuery {
    #[serde(rename = "bookId")]
    book_id: Option<String>,
}

async fn conflicts(
    State(app): State<App>,
    headers: HeaderMap,
    query: Result<Query<ConflictQuery>, axum::extract::rejection::QueryRejection>,
) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    let Query(query) = query.map_err(|e| SyncError::bad(e.body_text()))?;
    let bytes = reader
        .store
        .run(move |conn, _| store::list_conflicts(conn, query.book_id.as_deref()))
        .await?;
    Ok(json(StatusCode::OK, bytes))
}

async fn resolve(
    State(app): State<App>,
    headers: HeaderMap,
    Path(conflict_id): Path<String>,
    body: Result<Bytes, BytesRejection>,
) -> SyncResult<Response> {
    let reader = reader(&app);
    let device = device(&reader, &headers).await?;
    let body = body_bytes(body)?;
    let bytes = reader
        .store
        .run(move |conn, _| store::resolve(conn, &device.id, &conflict_id, &body))
        .await?;
    Ok(json(StatusCode::OK, bytes))
}

async fn books(State(app): State<App>, headers: HeaderMap) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    Ok(json(StatusCode::OK, reader.store.run(|conn, _| store::list_books(conn)).await?))
}

async fn register(State(app): State<App>, headers: HeaderMap, body: Result<Bytes, BytesRejection>) -> SyncResult<Response> {
    let reader = reader(&app);
    let device = device(&reader, &headers).await?;
    let body = body_bytes(body)?;
    let bytes = reader
        .store
        .run(move |conn, store| store::register_book(conn, &device.id, &body, store.clock.now()))
        .await?;
    Ok(json(StatusCode::OK, bytes))
}

fn blob_sha(sha: String) -> SyncResult<String> {
    if store::is_sha256(&sha) { Ok(sha) } else { Err(SyncError::bad("blob path must be 64 lowercase hex")) }
}

async fn head_blob(State(app): State<App>, headers: HeaderMap, Path(sha): Path<String>) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    reader.blobs.head(&reader.store, blob_sha(sha)?).await
}

async fn get_blob(State(app): State<App>, headers: HeaderMap, Path(sha): Path<String>) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    reader.blobs.get(&reader.store, blob_sha(sha)?, &headers).await
}

async fn put_blob(State(app): State<App>, headers: HeaderMap, Path(sha): Path<String>, body: Body) -> SyncResult<Response> {
    let reader = reader(&app);
    device(&reader, &headers).await?;
    reader.blobs.put(&reader.store, blob_sha(sha)?, &headers, body).await
}

use axum::{
    Extension, Json, Router,
    body::Body,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, PathRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    middleware,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use futures_util::TryStreamExt;
use serde::Deserialize;
use taildrive_core::{
    models::{
        CreateShareRequest, FileItem, HealthResponse, InitUploadRequest, ListFilesResponse,
        ListSharesResponse,
    },
    path::{DrivePath, parse_folder_path, parse_path},
    update::{ReleaseManifest, current_build_info},
};
use tokio_util::io::ReaderStream;

use crate::{
    App,
    auth::{self, Principal},
    content,
    db::{self, WriteFile},
    error::{ApiError, ApiResult},
    storage::{ByteStream, StorageError},
};

pub(crate) fn router(app: App) -> Router {
    let authenticated = Router::new()
        .route("/health", get(health))
        .route("/files", get(list_files).put(put_file).delete(delete_file))
        .route("/files/content", get(get_content))
        .route("/folders", post(create_folder))
        .route("/uploads", post(init_upload))
        .route("/uploads/{id}", axum::routing::delete(abort_upload))
        .route("/uploads/{id}/parts/{part_num}", put(put_part))
        .route("/uploads/{id}/complete", post(complete_upload))
        .route("/shares", get(list_shares).post(create_share))
        .route("/shares/{id}", axum::routing::delete(revoke_share))
        .route("/updates/manifest.json", get(get_update_manifest))
        .route("/updates/manifest.json.sig", get(get_update_manifest_sig))
        .route("/updates/artifacts/{name}", get(get_update_artifact))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(
            app.clone(),
            auth::require_auth,
        ));

    Router::new()
        .nest("/v1/reader", crate::reader::router())
        .nest("/v1", authenticated)
        .route("/s/{token}", get(get_share))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(not_found)
        .with_state(app)
}

async fn health(
    State(app): State<App>,
    Extension(caller): Extension<Principal>,
) -> ApiResult<Json<HealthResponse>> {
    let drive = &app.state.drive;
    let latest_build_number = read_latest_build_number(app.state.releases_dir.as_deref()).await;
    Ok(Json(HealthResponse {
        status: "ok".to_owned(),
        node: app.state.node.clone(),
        tailnet_user: caller.tailnet_user,
        storage_driver: drive.storage.driver_name().to_owned(),
        quota: drive.quota().await?,
        server_version: Some(current_build_info().version),
        latest_build_number,
    }))
}

async fn read_latest_build_number(releases_dir: Option<&std::path::Path>) -> Option<u64> {
    let dir = releases_dir?;
    let bytes = tokio::fs::read(dir.join("manifest.json")).await.ok()?;
    let manifest: ReleaseManifest = serde_json::from_slice(&bytes).ok()?;
    Some(manifest.build_number)
}

async fn get_update_manifest(State(app): State<App>) -> ApiResult<Response> {
    serve_release_file(
        app.state.releases_dir.as_deref(),
        "manifest.json",
        "application/json",
        "no-cache",
    )
    .await
}

async fn get_update_manifest_sig(State(app): State<App>) -> ApiResult<Response> {
    serve_release_file(
        app.state.releases_dir.as_deref(),
        "manifest.json.sig",
        "text/plain; charset=utf-8",
        "no-cache",
    )
    .await
}

async fn get_update_artifact(
    State(app): State<App>,
    name: Result<Path<String>, PathRejection>,
) -> ApiResult<Response> {
    let Path(name) = name.map_err(|error| ApiError::bad(error.body_text()))?;
    if !is_safe_artifact_name(&name) {
        return Err(ApiError::bad("invalid artifact name"));
    }
    let Some(dir) = app.state.releases_dir.as_deref() else {
        return Err(ApiError::not_found("updates are not configured on this server"));
    };
    let path = dir.join("artifacts").join(&name);
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found("release artifact not found"));
        }
        Err(err) => return Err(ApiError::internal(err.to_string())),
    };
    let metadata = file
        .metadata()
        .await
        .map_err(|err| ApiError::internal(err.to_string()))?;
    if !metadata.is_file() {
        return Err(ApiError::not_found("release artifact not found"));
    }
    let stream = ReaderStream::new(file);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/octet-stream")
        .header(header::CONTENT_LENGTH, metadata.len().to_string())
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from_stream(stream))
        .map_err(|err| ApiError::internal(err.to_string()))
}

async fn serve_release_file(
    releases_dir: Option<&std::path::Path>,
    filename: &str,
    content_type: &'static str,
    cache_control: &'static str,
) -> ApiResult<Response> {
    let Some(dir) = releases_dir else {
        return Err(ApiError::not_found("updates are not configured on this server"));
    };
    let bytes = match tokio::fs::read(dir.join(filename)).await {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ApiError::not_found("update manifest not found"));
        }
        Err(err) => return Err(ApiError::internal(err.to_string())),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, bytes.len().to_string())
        .header(header::CACHE_CONTROL, cache_control)
        .body(Body::from(bytes))
        .map_err(|err| ApiError::internal(err.to_string()))
}

fn is_safe_artifact_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 160
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_'))
}

#[derive(Deserialize)]
struct ListQuery {
    path: Option<String>,
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn list_files(
    State(app): State<App>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> ApiResult<Json<ListFilesResponse>> {
    let Query(query) = query.map_err(|error| ApiError::bad(error.body_text()))?;
    let path = parse_folder_path(query.path.as_deref())?;
    let limit = query.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::bad("limit must be between 1 and 1000"));
    }
    let cursor = match query.cursor.as_deref() {
        None | Some("") => String::new(),
        Some(raw) => {
            let bytes = URL_SAFE
                .decode(raw)
                .map_err(|_| ApiError::bad("invalid cursor"))?;
            String::from_utf8(bytes).map_err(|_| ApiError::bad("invalid cursor"))?
        }
    };
    let folder = path.clone();
    let mut items = app
        .state
        .drive
        .db
        .run(move |conn| db::list(conn, &folder, cursor, limit + 1))
        .await?;
    let next_cursor = if items.len() > limit {
        items.truncate(limit);
        items.last().map(|item| URL_SAFE.encode(&item.path))
    } else {
        None
    };
    Ok(Json(ListFilesResponse {
        path: path.to_string(),
        items,
        next_cursor,
    }))
}

#[derive(Deserialize)]
struct FolderRequest {
    path: String,
}

async fn create_folder(
    State(app): State<App>,
    payload: Result<Json<FolderRequest>, JsonRejection>,
) -> ApiResult<(StatusCode, Json<FileItem>)> {
    let payload = read_json(payload)?;
    let path = parse_path(&payload.path)?;
    let folder = app
        .state
        .drive
        .db
        .run(move |conn| {
            let tx = conn.transaction()?;
            let folder = db::create_folder(&tx, &path)?;
            tx.commit()?;
            Ok(folder)
        })
        .await?;
    Ok((StatusCode::CREATED, Json(folder)))
}

#[derive(Deserialize)]
struct FileQuery {
    path: Option<String>,
    #[serde(default)]
    recursive: bool,
}

fn file_query(query: Result<Query<FileQuery>, QueryRejection>) -> ApiResult<(DrivePath, bool)> {
    let Query(query) = query.map_err(|error| ApiError::bad(error.body_text()))?;
    Ok((
        parse_path(query.path.as_deref().unwrap_or_default())?,
        query.recursive,
    ))
}

async fn put_file(
    State(app): State<App>,
    query: Result<Query<FileQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<Response> {
    let (path, _) = file_query(query)?;
    let size = content_length(&headers)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::LENGTH_REQUIRED,
            "ERR_LENGTH_REQUIRED",
            "Content-Length is required",
        )
    })?;
    let sha256 = checksum(&headers)?;
    let if_match = header_text(&headers, "if-match")?.map(str::to_owned);
    let mime = mime(&path, header_text(&headers, "content-type")?)?;
    let (upload, created) = app
        .state
        .drive
        .put(
            WriteFile {
                path,
                size,
                sha256,
                mime,
                if_match,
            },
            byte_stream(body),
        )
        .await?;
    Ok((write_status(created), Json(upload)).into_response())
}

async fn get_content(
    State(app): State<App>,
    query: Result<Query<FileQuery>, QueryRejection>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let (path, _) = file_query(query)?;
    let (entry, range, stream) = app.state.drive.open_file(path, &headers).await?;
    content::download(entry, range, stream)
}

async fn delete_file(
    State(app): State<App>,
    query: Result<Query<FileQuery>, QueryRejection>,
) -> ApiResult<Json<taildrive_core::models::DeleteResponse>> {
    let (path, recursive) = file_query(query)?;
    Ok(Json(app.state.drive.delete(path, recursive).await?))
}

async fn init_upload(
    State(app): State<App>,
    payload: Result<Json<InitUploadRequest>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let payload = read_json(payload)?;
    let path = parse_path(&payload.path)?;
    if payload.size_bytes > i64::MAX as u64 {
        return Err(ApiError::bad("size_bytes is too large"));
    }
    let mime = mime(&path, payload.mime.as_deref())?;
    let upload = app
        .state
        .drive
        .init_upload(path, payload.size_bytes, mime)
        .await?;
    Ok((StatusCode::CREATED, Json(upload)))
}

async fn put_part(
    State(app): State<App>,
    path: Result<Path<(String, u64)>, PathRejection>,
    headers: HeaderMap,
    body: Body,
) -> ApiResult<StatusCode> {
    let Path((id, part_num)) = path.map_err(|error| ApiError::bad(error.body_text()))?;
    let length = content_length(&headers)?;
    app.state
        .drive
        .put_part(id, part_num, length, checksum(&headers)?, byte_stream(body))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn complete_upload(
    State(app): State<App>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Path(id) = path.map_err(|error| ApiError::bad(error.body_text()))?;
    let if_match = header_text(&headers, "if-match")?.map(str::to_owned);
    let (upload, created) = app
        .state
        .drive
        .complete(id, checksum(&headers)?, if_match)
        .await?;
    Ok((write_status(created), Json(upload)).into_response())
}

async fn abort_upload(
    State(app): State<App>,
    path: Result<Path<String>, PathRejection>,
) -> ApiResult<StatusCode> {
    let Path(id) = path.map_err(|error| ApiError::bad(error.body_text()))?;
    app.state.drive.abort(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn create_share(
    State(app): State<App>,
    payload: Result<Json<CreateShareRequest>, JsonRejection>,
) -> ApiResult<impl IntoResponse> {
    let payload = read_json(payload)?;
    let path = parse_path(&payload.path)?;
    let mut secret = [0_u8; 32];
    rand::fill(&mut secret);
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(secret);
    let share = app
        .state
        .drive
        .db
        .run(move |conn| db::create_share(conn, &path, token, payload.expires_in_seconds))
        .await?;
    Ok((
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(share),
    ))
}

async fn list_shares(State(app): State<App>) -> ApiResult<impl IntoResponse> {
    let items = app.state.drive.db.run(|conn| db::list_shares(conn)).await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json(ListSharesResponse { items }),
    ))
}

async fn revoke_share(
    State(app): State<App>,
    path: Result<Path<String>, PathRejection>,
) -> ApiResult<StatusCode> {
    let Path(id) = path.map_err(|error| ApiError::bad(error.body_text()))?;
    app.state
        .drive
        .db
        .run(move |conn| db::revoke_share(conn, &id))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn get_share(
    State(app): State<App>,
    path: Result<Path<String>, PathRejection>,
    headers: HeaderMap,
) -> ApiResult<Response> {
    let Path(token) = path.map_err(|error| ApiError::bad(error.body_text()))?;
    let (entry, range, stream) = app.state.drive.open_share(token, &headers).await?;
    content::download(entry, range, stream)
}

fn read_json<T>(payload: Result<Json<T>, JsonRejection>) -> ApiResult<T> {
    payload.map(|Json(value)| value).map_err(|error| {
        if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "ERR_PAYLOAD_TOO_LARGE",
                error.body_text(),
            )
        } else {
            ApiError::bad(error.body_text())
        }
    })
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> ApiResult<Option<&'a str>> {
    headers
        .get(name)
        .map(|header| {
            header
                .to_str()
                .map_err(|_| ApiError::bad(format!("invalid {name} header")))
        })
        .transpose()
}

fn content_length(headers: &HeaderMap) -> ApiResult<Option<u64>> {
    header_text(headers, "content-length")?
        .map(|length| {
            if length.is_empty() || !length.bytes().all(|b| b.is_ascii_digit()) {
                return Err(ApiError::bad(
                    "Content-Length must be a non-negative integer",
                ));
            }
            length
                .parse()
                .map_err(|_| ApiError::bad("Content-Length is too large"))
        })
        .transpose()
}

fn checksum(headers: &HeaderMap) -> ApiResult<String> {
    let value = header_text(headers, "x-content-sha256")?
        .unwrap_or_default()
        .trim();
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(ApiError::bad("X-Content-SHA256 must be 64 hex characters"));
    }
    Ok(value.to_ascii_lowercase())
}

fn mime(path: &DrivePath, provided: Option<&str>) -> ApiResult<String> {
    match provided {
        Some(mime) => mime
            .parse::<mime_guess::Mime>()
            .map(|value| value.to_string())
            .map_err(|_| ApiError::bad("invalid content type")),
        None => Ok(mime_guess::from_path(path.name())
            .first_or_octet_stream()
            .to_string()),
    }
}

fn byte_stream(body: Body) -> ByteStream {
    Box::pin(
        body.into_data_stream()
            .map_err(|_| StorageError::BodyInterrupted),
    )
}

fn write_status(created: bool) -> StatusCode {
    if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    }
}

async fn not_found() -> ApiError {
    ApiError::not_found("no such route")
}

async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "ERR_METHOD_NOT_ALLOWED",
        "method not allowed",
    )
}

//! Reader sync errors use the rev 3.x envelope `{"error":{"code","message"}}`, not TailDrive's.

use axum::{
    Json,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;

#[derive(Debug)]
pub(crate) struct SyncError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    pub headers: Vec<(HeaderName, String)>,
}

pub(crate) type SyncResult<T> = Result<T, SyncError>;

impl SyncError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self { status, code, message: message.into(), headers: Vec::new() }
    }
    pub(crate) fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }
    pub(crate) fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }
    pub(crate) fn forbidden(message: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message)
    }
    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }
    pub(crate) fn too_large(message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", message)
    }
    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "reader sync operation failed");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal server error")
    }
    pub(crate) fn header(mut self, name: HeaderName, value: impl ToString) -> Self {
        self.headers.push((name, value.to_string()));
        self
    }
}

impl IntoResponse for SyncError {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        for (name, value) in self.headers {
            if let Ok(value) = HeaderValue::from_str(&value) {
                headers.insert(name, value);
            }
        }
        if self.status == StatusCode::UNAUTHORIZED {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        let body = json!({ "error": { "code": self.code, "message": self.message } });
        (self.status, headers, Json(body)).into_response()
    }
}

impl From<rusqlite::Error> for SyncError {
    fn from(error: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(failure, _) = &error
            && matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        {
            return Self::new(StatusCode::SERVICE_UNAVAILABLE, "server_busy", "database is busy");
        }
        Self::internal(error)
    }
}

impl From<std::io::Error> for SyncError {
    fn from(error: std::io::Error) -> Self {
        Self::internal(error)
    }
}

impl From<crate::storage::StorageError> for SyncError {
    fn from(error: crate::storage::StorageError) -> Self {
        match error {
            crate::storage::StorageError::NotFound => Self::not_found("blob is unavailable"),
            other => Self::internal(other),
        }
    }
}

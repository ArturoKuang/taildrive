use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use taildrive_core::{
    models::{ErrorBody, ErrorEnvelope},
    path::PathError,
};

pub type ApiResult<T> = Result<T, ApiError>;

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: String,
    details: Option<Box<Value>>,
    size_for_range: Option<u64>,
}

impl ApiError {
    pub(crate) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
            size_for_range: None,
        }
    }

    pub(crate) fn details(mut self, details: Value) -> Self {
        self.details = Some(Box::new(details));
        self
    }

    pub(crate) fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "ERR_BAD_REQUEST", message)
    }

    pub(crate) fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "ERR_NOT_FOUND", message)
    }

    pub(crate) fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "ERR_CONFLICT", message)
    }

    pub(crate) fn internal(error: impl std::fmt::Display) -> Self {
        tracing::error!(%error, "TailDrive operation failed");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "ERR_INTERNAL",
            "internal server error",
        )
    }

    pub(crate) fn range(size: u64) -> Self {
        let mut error = Self::new(
            StatusCode::RANGE_NOT_SATISFIABLE,
            "ERR_RANGE_NOT_SATISFIABLE",
            "Range is not satisfiable",
        )
        .details(json!({ "size_bytes": size }));
        error.size_for_range = Some(size);
        error
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(size) = self.size_for_range {
            headers.insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes */{size}")).unwrap(),
            );
        }
        if self.status == StatusCode::UNAUTHORIZED {
            headers.insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        let body = ErrorEnvelope {
            error: ErrorBody {
                code: self.code.to_owned(),
                message: self.message,
                retryable: self.status == StatusCode::INTERNAL_SERVER_ERROR,
                details: self.details.map_or_else(|| json!({}), |details| *details),
            },
        };
        (self.status, headers, Json(body)).into_response()
    }
}

impl From<PathError> for ApiError {
    fn from(error: PathError) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "ERR_INVALID_PATH",
            error.to_string(),
        )
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(error: rusqlite::Error) -> Self {
        Self::internal(error)
    }
}

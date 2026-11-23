use std::{future::Future, time::Duration};

use bytes::Bytes;
use futures_util::Stream;
use reqwest::{
    Body, Method, Response, StatusCode, Url,
    header::{CONTENT_LENGTH, CONTENT_TYPE, IF_MATCH, RANGE},
};
use serde::de::DeserializeOwned;

use crate::models::*;

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("invalid TailDrive base URL")]
    InvalidBaseUrl,
    #[error(transparent)]
    Transport(#[from] reqwest::Error),
    #[error("TailDrive returned {status}: {code}: {message}", code = .body.error.code, message = .body.error.message)]
    Api {
        status: StatusCode,
        body: Box<ErrorEnvelope>,
    },
    #[error("TailDrive returned {0} without an error envelope")]
    UnexpectedStatus(StatusCode),
}

impl ClientError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(error) => {
                error.is_connect()
                    || error.is_timeout()
                    || error.is_body()
                    || error.is_request() && !error.is_builder()
            }
            Self::Api { status, body } => {
                body.error.retryable || *status == StatusCode::TOO_MANY_REQUESTS
            }
            Self::UnexpectedStatus(status) => {
                status.is_server_error() || *status == StatusCode::TOO_MANY_REQUESTS
            }
            Self::InvalidBaseUrl => false,
        }
    }
}

#[derive(Clone)]
pub struct TailDriveClient {
    http: reqwest::Client,
    base: Url,
    bearer: Option<String>,
}

pub struct UploadBody {
    pub content_length: u64,
    pub sha256: String,
    pub body: Body,
}

impl UploadBody {
    pub fn from_bytes(bytes: Bytes) -> Self {
        Self {
            content_length: bytes.len() as u64,
            sha256: crate::hash::sha256_hex(&bytes),
            body: bytes.into(),
        }
    }

    pub fn streaming<S, E>(content_length: u64, sha256: String, stream: S) -> Self
    where
        S: Stream<Item = Result<Bytes, E>> + Send + 'static,
        E: Into<Box<dyn std::error::Error + Send + Sync>>,
    {
        Self {
            content_length,
            sha256,
            body: Body::wrap_stream(stream),
        }
    }

    fn apply(self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(CONTENT_LENGTH, self.content_length)
            .header("x-content-sha256", self.sha256)
            .body(self.body)
    }
}

impl TailDriveClient {
    pub fn new(base: &str, bearer: Option<String>) -> Result<Self, ClientError> {
        let base = Url::parse(base).map_err(|_| ClientError::InvalidBaseUrl)?;
        if !matches!(base.scheme(), "http" | "https")
            || base.host().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
        {
            return Err(ClientError::InvalidBaseUrl);
        }
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, base, bearer })
    }

    fn request(&self, method: Method, segments: &[&str]) -> reqwest::RequestBuilder {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("validated base URL")
            .pop_if_empty()
            .extend(segments);
        let request = self.http.request(method, url);
        match &self.bearer {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    pub async fn health(&self) -> Result<HealthResponse, ClientError> {
        json(self.request(Method::GET, &["v1", "health"])).await
    }

    pub async fn list_files(
        &self,
        path: &str,
        cursor: Option<&str>,
        limit: Option<u16>,
    ) -> Result<ListFilesResponse, ClientError> {
        let mut request = self
            .request(Method::GET, &["v1", "files"])
            .query(&[("path", path)]);
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        if let Some(limit) = limit {
            request = request.query(&[("limit", limit)]);
        }
        json(request).await
    }

    pub async fn put_file(
        &self,
        path: &str,
        body: UploadBody,
        mime: Option<&str>,
        if_match: Option<&str>,
    ) -> Result<UploadResponse, ClientError> {
        let mut request = self
            .request(Method::PUT, &["v1", "files"])
            .query(&[("path", path)]);
        if let Some(mime) = mime {
            request = request.header(CONTENT_TYPE, mime);
        }
        json(body.apply(conditional(request, if_match))).await
    }

    /// Returns response headers and a streaming body, including for a partial download.
    pub async fn download(&self, path: &str, range: Option<&str>) -> Result<Response, ClientError> {
        let mut request = self
            .request(Method::GET, &["v1", "files", "content"])
            .query(&[("path", path)]);
        if let Some(range) = range {
            request = request.header(RANGE, range);
        }
        checked(request).await
    }

    pub async fn delete_file(
        &self,
        path: &str,
        recursive: bool,
    ) -> Result<DeleteResponse, ClientError> {
        json(
            self.request(Method::DELETE, &["v1", "files"])
                .query(&[("path", path)])
                .query(&[("recursive", recursive)]),
        )
        .await
    }

    pub async fn create_folder(&self, path: &str) -> Result<FileItem, ClientError> {
        json(
            self.request(Method::POST, &["v1", "folders"])
                .json(&serde_json::json!({ "path": path })),
        )
        .await
    }

    pub async fn init_upload(
        &self,
        upload: &InitUploadRequest,
    ) -> Result<InitUploadResponse, ClientError> {
        json(self.request(Method::POST, &["v1", "uploads"]).json(upload)).await
    }

    pub async fn put_part(&self, id: &str, part: u64, body: UploadBody) -> Result<(), ClientError> {
        checked(body.apply(self.request(
            Method::PUT,
            &["v1", "uploads", id, "parts", &part.to_string()],
        )))
        .await?;
        Ok(())
    }

    pub async fn complete_upload(
        &self,
        id: &str,
        sha256: &str,
        if_match: Option<&str>,
    ) -> Result<UploadResponse, ClientError> {
        let request = self
            .request(Method::POST, &["v1", "uploads", id, "complete"])
            .header("x-content-sha256", sha256);
        json(conditional(request, if_match)).await
    }

    pub async fn abort_upload(&self, id: &str) -> Result<(), ClientError> {
        checked(self.request(Method::DELETE, &["v1", "uploads", id])).await?;
        Ok(())
    }

    pub async fn create_share(&self, share: &CreateShareRequest) -> Result<ShareItem, ClientError> {
        json(self.request(Method::POST, &["v1", "shares"]).json(share)).await
    }

    pub async fn list_shares(&self) -> Result<ListSharesResponse, ClientError> {
        json(self.request(Method::GET, &["v1", "shares"])).await
    }

    pub async fn revoke_share(&self, id: &str) -> Result<(), ClientError> {
        checked(self.request(Method::DELETE, &["v1", "shares", id])).await?;
        Ok(())
    }
}

fn conditional(
    request: reqwest::RequestBuilder,
    if_match: Option<&str>,
) -> reqwest::RequestBuilder {
    match if_match {
        Some(etag) => request.header(IF_MATCH, etag),
        None => request,
    }
}

async fn json<T: DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T, ClientError> {
    Ok(checked(request).await?.json().await?)
}

async fn checked(request: reqwest::RequestBuilder) -> Result<Response, ClientError> {
    let response = request.send().await?;
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    match response.json().await {
        Ok(body) => Err(ClientError::Api { status, body }),
        Err(_) => Err(ClientError::UnexpectedStatus(status)),
    }
}

pub const RETRY_BACKOFF: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
];

/// Retries a replayable operation after transient failures. The caller supplies
/// a new request/body each time and chooses which operations are safe to replay.
pub async fn retry<T, F, Fut>(mut operation: F) -> Result<T, ClientError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ClientError>>,
{
    for delay in RETRY_BACKOFF {
        match operation().await {
            Err(error) if error.is_retryable() => tokio::time::sleep(delay).await,
            result => return result,
        }
    }
    operation().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn backoff_retries_only_transient_failures() {
        let start = tokio::time::Instant::now();
        let mut attempts = 0;
        let result = retry(|| {
            attempts += 1;
            let status = if attempts < 4 {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::CONFLICT
            };
            std::future::ready(Err::<(), _>(ClientError::UnexpectedStatus(status)))
        })
        .await;
        assert!(matches!(
            result,
            Err(ClientError::UnexpectedStatus(StatusCode::CONFLICT))
        ));
        assert_eq!(attempts, 4);
        assert_eq!(start.elapsed(), Duration::from_secs(7));

        let start = tokio::time::Instant::now();
        let _ = retry(|| std::future::ready(Err::<(), _>(ClientError::InvalidBaseUrl))).await;
        assert_eq!(start.elapsed(), Duration::ZERO);
    }
}

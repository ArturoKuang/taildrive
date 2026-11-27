//! Content-addressed book store (SYNC_SPEC §5.7). Object key `reader/sha256/<hex>` in TailDrive's
//! StorageBackend, so a duplicate upload is a no-op. Uploads stream to `<data>/reader/uploads/<hex>.part`,
//! resume by offset, and are SHA-256 verified before they are published.

use std::{
    collections::HashMap,
    io::Read,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    body::Body,
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use serde_json::json;
use taildrive_core::hash::Sha256Hasher;
use tokio::{fs, io::AsyncWriteExt, task::spawn_blocking};

use super::{
    error::{SyncError, SyncResult},
    store::{self, Store},
};
use crate::{
    content,
    storage::{StorageBackend, free_on_disk},
};

const MAX_BLOB_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const FSYNC_EVERY: u64 = 8 * 1024 * 1024;
const STALE_UPLOAD: Duration = Duration::from_secs(7 * 24 * 60 * 60);
const UPLOAD_OFFSET: HeaderName = HeaderName::from_static("upload-offset");

pub(crate) struct Blobs {
    storage: Arc<dyn StorageBackend>,
    uploads_dir: PathBuf,
    data_dir: PathBuf,
    /// TailDrive's free-disk floor. It applies to book uploads only; record pushes never check it.
    floor: u64,
    /// Book-store quota, separate from TailDrive's files quota. `None` = no cap (the default).
    quota: Option<u64>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

pub(crate) fn object_key(sha: &str) -> String {
    format!("reader/sha256/{sha}")
}

fn etag(sha: &str) -> String {
    format!("\"{sha}\"")
}

impl Blobs {
    pub(crate) fn new(storage: Arc<dyn StorageBackend>, data_dir: PathBuf, floor: u64, quota: Option<u64>) -> Self {
        Self {
            storage,
            uploads_dir: data_dir.join("reader").join("uploads"),
            data_dir,
            floor,
            quota,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn part(&self, sha: &str) -> PathBuf {
        self.uploads_dir.join(format!("{sha}.part"))
    }

    async fn part_len(&self, sha: &str) -> u64 {
        fs::metadata(self.part(sha)).await.map_or(0, |m| m.len())
    }

    fn lock(&self, sha: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("blob lock map");
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(sha.to_owned()).or_default().clone()
    }

    async fn free_bytes(&self) -> SyncResult<u64> {
        Ok(match self.storage.disk_free_bytes().await? {
            Some(free) => free,
            None => free_on_disk(self.data_dir.clone()).await?,
        })
    }

    /// Startup (§8.4): a full-size `.part` whose hash matches is finished, a mismatching one is
    /// deleted, uploads older than 7 days are dropped, and orphan `.part` files are removed.
    pub(crate) async fn recover(&self, store: &Store) -> SyncResult<()> {
        fs::create_dir_all(&self.uploads_dir).await?;
        let uploads = store.run(|conn, _| Ok(store::uploads(conn)?)).await?;
        let now = store.clock.now().unix_timestamp();
        let mut known = Vec::new();
        for (sha, total, started_at) in uploads {
            let len = fs::metadata(self.part(&sha)).await.map(|m| m.len()).ok();
            let stale = now - started_at > STALE_UPLOAD.as_secs() as i64;
            match len {
                Some(len) if len == total && !stale => {
                    if self.finish(store, &sha, total).await.is_err() {
                        tracing::warn!(%sha, "discarded a complete upload whose hash did not match");
                    }
                }
                Some(_) if !stale => known.push(format!("{sha}.part")),
                _ => self.discard(store, &sha).await?,
            }
        }
        let mut dir = fs::read_dir(&self.uploads_dir).await?;
        while let Some(entry) = dir.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !known.contains(&name) {
                let _ = fs::remove_file(entry.path()).await;
            }
        }
        Ok(())
    }

    async fn discard(&self, store: &Store, sha: &str) -> SyncResult<()> {
        match fs::remove_file(self.part(sha)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        let sha = sha.to_owned();
        store.run(move |conn, _| Ok(store::forget_upload(conn, &sha)?)).await
    }

    /// Hash the complete `.part`; publish it under its content key on a match.
    async fn finish(&self, store: &Store, sha: &str, total: u64) -> SyncResult<()> {
        let path = self.part(sha);
        let hash_path = path.clone();
        let actual = spawn_blocking(move || -> std::io::Result<String> {
            let mut file = std::fs::File::open(hash_path)?;
            let mut hasher = Sha256Hasher::new();
            let mut buf = vec![0_u8; 1024 * 1024];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            Ok(hasher.finish())
        })
        .await
        .map_err(SyncError::internal)??;
        if actual != sha {
            self.discard(store, sha).await?;
            return Err(SyncError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "hash_mismatch",
                "upload discarded: content does not match its SHA-256",
            ));
        }
        let key = object_key(sha);
        self.storage.publish(&key, &path).await?;
        // local-fs renames the file away; other backends copy it, so drop the spool copy.
        let _ = fs::remove_file(&path).await;
        let sha = sha.to_owned();
        store
            .run(move |conn, store| Ok(store::insert_blob(conn, &sha, total, &key, store.clock.now())?))
            .await
    }

    pub(crate) async fn head(&self, store: &Store, sha: String) -> SyncResult<Response> {
        let lookup = sha.clone();
        match store.run(move |conn, _| Ok(store::blob_size(conn, &lookup)?)).await? {
            Some(size) => Ok((
                StatusCode::OK,
                [
                    (header::CONTENT_LENGTH, size.to_string()),
                    (header::ETAG, etag(&sha)),
                    (header::ACCEPT_RANGES, "bytes".to_owned()),
                ],
            )
                .into_response()),
            None => Ok((StatusCode::NOT_FOUND, [(UPLOAD_OFFSET, self.part_len(&sha).await.to_string())]).into_response()),
        }
    }

    pub(crate) async fn get(&self, store: &Store, sha: String, headers: &HeaderMap) -> SyncResult<Response> {
        let lookup = sha.clone();
        let size = store
            .run(move |conn, _| Ok(store::blob_size(conn, &lookup)?))
            .await?
            .ok_or_else(|| SyncError::not_found("no such blob"))?;
        let tag = etag(&sha);
        let honor_range = headers
            .get(header::IF_RANGE)
            .is_none_or(|value| value.to_str().is_ok_and(|v| v.trim() == tag));
        let range = if honor_range {
            content::byte_range(headers, size).map_err(|_| {
                SyncError::new(StatusCode::RANGE_NOT_SATISFIABLE, "range_not_satisfiable", "Range is not satisfiable")
                    .header(header::CONTENT_RANGE, format!("bytes */{size}"))
            })?
        } else {
            None
        };
        let span = range.clone().unwrap_or(0..size);
        let stream = self.storage.open(&object_key(&sha), span.clone()).await?;
        let mut response = Response::new(Body::from_stream(stream));
        let h = response.headers_mut();
        h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
        h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        h.insert(header::ETAG, HeaderValue::from_str(&tag).expect("etag"));
        h.insert(header::CONTENT_LENGTH, HeaderValue::from(span.end - span.start));
        if range.is_some() {
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            response.headers_mut().insert(
                header::CONTENT_RANGE,
                HeaderValue::from_str(&format!("bytes {}-{}/{size}", span.start, span.end - 1)).expect("range"),
            );
        }
        Ok(response)
    }

    pub(crate) async fn put(&self, store: &Store, sha: String, headers: &HeaderMap, body: Body) -> SyncResult<Response> {
        let lookup = sha.clone();
        if let Some(size) = store.run(move |conn, _| Ok(store::blob_size(conn, &lookup)?)).await? {
            // Duplicate upload: the content is already stored once. The body is never read.
            return Ok((StatusCode::OK, axum::Json(json!({ "sha256": sha, "size": size }))).into_response());
        }
        let length = header_u64(headers, header::CONTENT_LENGTH)?;
        let (start, total, length) = match headers.get(header::CONTENT_RANGE) {
            Some(value) => {
                let (start, end, total) = value
                    .to_str()
                    .ok()
                    .and_then(parse_content_range)
                    .ok_or_else(|| SyncError::bad("Content-Range must be bytes start-end/total"))?;
                if length.is_some_and(|l| l != end - start + 1) {
                    return Err(SyncError::bad("Content-Length does not match Content-Range"));
                }
                (start, total, end - start + 1)
            }
            None => {
                let length = length.ok_or_else(|| SyncError::bad("Content-Length or Content-Range is required"))?;
                (0, length, length)
            }
        };
        if total > MAX_BLOB_BYTES {
            return Err(SyncError::too_large("books are limited to 4 GiB"));
        }
        let lock = self.lock(&sha);
        let Ok(_guard) = lock.try_lock_owned() else {
            return Err(offset_mismatch("another upload of this blob is in progress", self.part_len(&sha).await));
        };
        let key = sha.clone();
        let known_total = store.run(move |conn, _| Ok(store::upload_total(conn, &key)?)).await?;
        let mut offset = if known_total == Some(total) { self.part_len(&sha).await } else { 0 };
        if start == 0 {
            offset = 0; // a whole-body PUT (or a resume from zero) restarts the upload
        }
        if start != offset {
            return Err(offset_mismatch("Content-Range start must equal Upload-Offset", offset));
        }
        // The free-disk floor gates book uploads only, so a full Mac mini pauses books while
        // highlights, notes and words keep syncing. 507 disk_full lets clients show that state.
        let free = self.free_bytes().await?;
        let remaining = total - start;
        if self.floor.saturating_add(remaining) > free {
            return Err(SyncError::new(
                StatusCode::INSUFFICIENT_STORAGE,
                "disk_full",
                "the server is below its free-disk floor; book uploads are paused, records still sync",
            ));
        }
        if let Some(quota) = self.quota {
            let used = store.run(|conn, _| Ok(store::blob_bytes_total(conn)?)).await?;
            if used.saturating_add(total) > quota {
                return Err(SyncError::new(StatusCode::INSUFFICIENT_STORAGE, "quota_exceeded", "the book store quota is full"));
            }
        }
        let key = sha.clone();
        store
            .run(move |conn, store| Ok(store::start_upload(conn, &key, total, store.clock.now())?))
            .await?;
        let path = self.part(&sha);
        let mut file = if start == 0 {
            fs::File::create(&path).await?
        } else {
            fs::OpenOptions::new().append(true).open(&path).await?
        };
        let mut written = 0_u64;
        let mut since_sync = 0_u64;
        let mut interrupted = false;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let Ok(chunk) = chunk else {
                interrupted = true;
                break;
            };
            let room = (length - written) as usize;
            if chunk.len() > room {
                file.write_all(&chunk[..room]).await?;
                file.sync_data().await?;
                return Err(SyncError::bad("body is longer than the declared range"));
            }
            file.write_all(&chunk).await?;
            written += chunk.len() as u64;
            since_sync += chunk.len() as u64;
            if since_sync >= FSYNC_EVERY {
                file.sync_data().await?;
                since_sync = 0;
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        let offset = start + written;
        if offset < total {
            if interrupted {
                tracing::info!(%sha, offset, "book upload interrupted; resumable");
            }
            return Ok((StatusCode::ACCEPTED, [(UPLOAD_OFFSET, offset.to_string())], axum::Json(json!({ "uploadOffset": offset })))
                .into_response());
        }
        self.finish(store, &sha, total).await?;
        Ok((StatusCode::CREATED, axum::Json(json!({ "sha256": sha, "size": total }))).into_response())
    }
}

fn offset_mismatch(message: &str, offset: u64) -> SyncError {
    SyncError::new(StatusCode::CONFLICT, "upload_offset_mismatch", message).header(UPLOAD_OFFSET, offset)
}

fn header_u64(headers: &HeaderMap, name: HeaderName) -> SyncResult<Option<u64>> {
    headers
        .get(&name)
        .map(|value| {
            value
                .to_str()
                .ok()
                .filter(|v| !v.is_empty() && v.bytes().all(|b| b.is_ascii_digit()))
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| SyncError::bad(format!("invalid {name} header")))
        })
        .transpose()
}

/// `bytes start-end/total` with `start <= end < total`.
fn parse_content_range(value: &str) -> Option<(u64, u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (span, total) = rest.split_once('/')?;
    let (start, end) = span.split_once('-')?;
    let (start, end, total) = (start.parse().ok()?, end.parse().ok()?, total.parse().ok()?);
    (start <= end && end < total).then_some((start, end, total))
}

#[cfg(test)]
impl Blobs {
    pub(crate) async fn part_exists(&self, sha: &str) -> bool {
        fs::metadata(self.part(sha)).await.is_ok()
    }
}

use std::{
    io,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use taildrive_core::{
    client::{ClientError, TailDriveClient, UploadBody},
    hash::{Sha256Hasher, sha256_hex},
    models::{InitUploadRequest, InitUploadResponse, UploadResponse},
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

use crate::{FfiError, UploadResult};

const SINGLE_UPLOAD_LIMIT: u64 = 8 * 1024 * 1024;
const PROGRESS_CHUNK: usize = 64 * 1024;

/// Return false to cancel. During upload, bytes represent body data sent;
/// completion still depends on the server verifying and committing the file.
/// Dispatch UI updates to the platform's UI thread. Do not invoke blocking
/// TailDrive client operations from this callback.
#[uniffi::export(callback_interface)]
pub trait TransferObserver: Send + Sync {
    fn progress(&self, bytes: u64, total: u64) -> bool;
}

fn report(observer: Option<&dyn TransferObserver>, bytes: u64, total: u64) -> Result<(), FfiError> {
    if observer.is_some_and(|observer| !observer.progress(bytes, total)) {
        return Err(FfiError::Cancelled);
    }
    Ok(())
}

struct UploadProgress {
    observer: Option<Arc<dyn TransferObserver>>,
    total: u64,
    cancelled: Arc<AtomicBool>,
}

impl UploadProgress {
    fn report(&self, sent: u64) -> Result<(), FfiError> {
        report(self.observer.as_deref(), sent, self.total)
    }

    fn body(&self, bytes: Vec<u8>, offset: u64) -> UploadBody {
        let Some(observer) = self.observer.clone() else {
            return UploadBody::from_bytes(bytes.into());
        };
        if bytes.is_empty() {
            return UploadBody::from_bytes(Bytes::new());
        }
        let cancelled = self.cancelled.clone();
        let total = self.total;
        let size = bytes.len() as u64;
        let digest = sha256_hex(&bytes);
        let chunks = stream::unfold((Bytes::from(bytes), offset), move |(mut bytes, sent)| {
            let next = if bytes.is_empty() {
                None
            } else if observer.progress(sent, total) {
                let chunk = bytes.split_to(bytes.len().min(PROGRESS_CHUNK));
                let next_offset = sent + chunk.len() as u64;
                Some((Ok(chunk), (bytes, next_offset)))
            } else {
                cancelled.store(true, Ordering::Relaxed);
                Some((
                    Err(io::Error::from(io::ErrorKind::Interrupted)),
                    (Bytes::new(), sent),
                ))
            };
            std::future::ready(next)
        });
        UploadBody::streaming(size, digest, chunks)
    }

    fn result<T>(&self, response: Result<T, ClientError>) -> Result<T, FfiError> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(FfiError::Cancelled);
        }
        Ok(response?)
    }
}

pub(crate) async fn upload(
    client: &TailDriveClient,
    remote: &str,
    local: &Path,
    if_match: Option<&str>,
    observer: Option<Arc<dyn TransferObserver>>,
) -> Result<UploadResult, FfiError> {
    let mut file = File::open(local).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() {
        return Err(FfiError::Io {
            message: "upload source must be a regular file".into(),
        });
    }
    let size = metadata.len();
    let progress = UploadProgress {
        observer,
        total: size,
        cancelled: Arc::new(AtomicBool::new(false)),
    };
    let mime = mime_guess::from_path(remote).first_raw();
    progress.report(0)?;
    let response = if size <= SINGLE_UPLOAD_LIMIT {
        let mut bytes = vec![0; size as usize];
        file.read_exact(&mut bytes).await?;
        progress.report(0)?;
        let body = progress.body(bytes, 0);
        progress.result(client.put_file(remote, body, mime, if_match).await)?
    } else {
        let session = client
            .init_upload(&InitUploadRequest {
                path: remote.to_owned(),
                size_bytes: size,
                mime: mime.map(str::to_owned),
            })
            .await?;
        let result = upload_parts(client, &session, file, if_match, &progress).await;
        if result.is_err() {
            let _ = client.abort_upload(&session.upload_id).await;
        }
        result?
    };
    let _ = progress.report(size);
    Ok(UploadResult {
        item: response.item,
        quota: response.quota,
    })
}

async fn upload_parts(
    client: &TailDriveClient,
    session: &InitUploadResponse,
    mut file: File,
    if_match: Option<&str>,
    progress: &UploadProgress,
) -> Result<UploadResponse, FfiError> {
    if !(1..=SINGLE_UPLOAD_LIMIT).contains(&session.part_size) {
        return Err(FfiError::Integrity {
            message: "server returned an unsupported upload part size".into(),
        });
    }

    let mut sha256 = Sha256Hasher::new();
    let mut sent = 0;
    let size = progress.total;
    for part in 1..=size.div_ceil(session.part_size) {
        progress.report(sent)?;
        let mut bytes = vec![0; (size - sent).min(session.part_size) as usize];
        file.read_exact(&mut bytes).await?;
        sha256.update(&bytes);
        let count = bytes.len() as u64;
        let body = progress.body(bytes, sent);
        progress.result(client.put_part(&session.upload_id, part, body).await)?;
        sent += count;
    }
    progress.report(sent)?;
    Ok(client
        .complete_upload(&session.upload_id, &sha256.finish(), if_match)
        .await?)
}

pub(crate) async fn download(
    client: &TailDriveClient,
    remote: &str,
    dest: &Path,
    observer: Option<&dyn TransferObserver>,
) -> Result<(), FfiError> {
    report(observer, 0, 0)?;
    let response = client.download(remote, None).await?;
    let expected_hash = response
        .headers()
        .get("x-content-sha256")
        .and_then(|header| header.to_str().ok())
        .ok_or_else(|| FfiError::Integrity {
            message: "download has no SHA-256 checksum".into(),
        })?
        .to_owned();
    let total = response.content_length().unwrap_or(0);
    let dir = dest
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let (file, temp_path) = tempfile::NamedTempFile::new_in(dir)?.into_parts();
    let mut file = File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut sha256 = Sha256Hasher::new();
    let mut received = 0;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(ClientError::from)?;
        file.write_all(&chunk).await?;
        sha256.update(&chunk);
        received += chunk.len() as u64;
        report(observer, received, total)?;
    }
    if sha256.finish() != expected_hash {
        return Err(FfiError::Integrity {
            message: "download SHA-256 does not match the server checksum".into(),
        });
    }
    file.sync_all().await?;
    report(observer, received, total)?;
    temp_path.persist(dest).map_err(|error| error.error)?;
    Ok(())
}

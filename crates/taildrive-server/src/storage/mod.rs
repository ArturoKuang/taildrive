mod local;
mod s3;

use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
    pin::Pin,
    sync::atomic::{AtomicU64, Ordering},
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use taildrive_core::hash::Sha256Hasher;
use tempfile::TempPath;
use tokio::{fs, io::AsyncWriteExt};
use tokio_util::task::TaskTracker;

pub use local::LocalFsStorage;
pub(crate) use local::free_on_disk;
pub use s3::{S3Config, S3Storage};

pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, StorageError>> + Send>>;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("blob is missing")]
    NotFound,
    #[error("upload body interrupted")]
    BodyInterrupted,
    #[error("blob does not match X-Content-SHA256 / expected size")]
    Mismatch {
        expected_size: u64,
        actual_size: u64,
        expected_sha256: String,
        actual_sha256: String,
    },
    #[error(transparent)]
    Io(io::Error),
    #[error(transparent)]
    Remote(opendal::Error),
    #[error(transparent)]
    Task(#[from] tokio::task::JoinError),
}

impl From<io::Error> for StorageError {
    fn from(error: io::Error) -> Self {
        match error.kind() {
            io::ErrorKind::NotFound => Self::NotFound,
            _ => Self::Io(error),
        }
    }
}

impl From<opendal::Error> for StorageError {
    fn from(error: opendal::Error) -> Self {
        match error.kind() {
            opendal::ErrorKind::NotFound => Self::NotFound,
            _ => Self::Remote(error),
        }
    }
}

#[async_trait]
pub trait StorageBackend: Send + Sync + 'static {
    fn driver_name(&self) -> &'static str;
    /// Publish a verified staging file under a new server-generated key.
    async fn publish(&self, key: &str, staged: &Path) -> Result<(), StorageError>;
    /// Open or verify the blob before returning a stream. The range is half-open.
    async fn open(&self, key: &str, range: Range<u64>) -> Result<ByteStream, StorageError>;
    /// Idempotently remove a blob.
    async fn delete(&self, key: &str) -> Result<(), StorageError>;
    /// Bytes available to this process, if reported by the backing store.
    async fn disk_free_bytes(&self) -> Result<Option<u64>, StorageError>;
}

pub(crate) struct Spool {
    dir: PathBuf,
    tasks: TaskTracker,
}

pub(crate) struct StagedFile {
    path: Option<TempPath>,
    tasks: TaskTracker,
}

impl Spool {
    pub(crate) fn new(dir: PathBuf, tasks: TaskTracker) -> Self {
        Self { dir, tasks }
    }

    pub(crate) async fn stage(
        &self,
        mut stream: ByteStream,
        size: u64,
        sha256: &str,
        disk_remaining: &AtomicU64,
    ) -> Result<StagedFile, StorageError> {
        let dir = self.dir.clone();
        let tasks = self.tasks.clone();
        let (file, mut staged) = self
            .tasks
            .spawn_blocking(move || {
                let (file, path) = tempfile::NamedTempFile::new_in(dir)?.into_parts();
                Ok::<_, io::Error>((
                    file,
                    StagedFile {
                        path: Some(path),
                        tasks,
                    },
                ))
            })
            .await??;
        let result = write_verified(
            fs::File::from_std(file),
            &mut stream,
            size,
            sha256,
            disk_remaining,
        )
        .await;
        if let Err(error) = result {
            staged.remove().await;
            return Err(error);
        }
        Ok(staged)
    }
}

impl StagedFile {
    pub(crate) fn path(&self) -> &Path {
        self.path.as_ref().expect("active staging file").as_ref()
    }

    pub(crate) async fn remove(&mut self) {
        if let Some(task) = self.queue_removal() {
            let _ = task.await;
        }
    }

    fn queue_removal(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        self.path.take().map(|path| {
            self.tasks.spawn_blocking(move || {
                if let Err(error) = path.close()
                    && error.kind() != io::ErrorKind::NotFound
                {
                    tracing::warn!(%error, "could not remove staging file");
                }
            })
        })
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        self.queue_removal();
    }
}

async fn write_verified(
    mut file: fs::File,
    stream: &mut ByteStream,
    expected_size: u64,
    expected_sha256: &str,
    disk_remaining: &AtomicU64,
) -> Result<(), StorageError> {
    let mut size = 0_u64;
    let mut hash = Sha256Hasher::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        size = size.saturating_add(chunk.len() as u64);
        hash.update(&chunk);
        if size > expected_size {
            break;
        }
        file.write_all(&chunk).await?;
        // Tokio's File buffers a blocking write. Flush before crediting it to statvfs.
        file.flush().await?;
        disk_remaining.store(expected_size - size, Ordering::Relaxed);
    }
    let actual_sha256 = hash.finish();
    if size != expected_size || actual_sha256 != expected_sha256 {
        return Err(StorageError::Mismatch {
            expected_size,
            actual_size: size,
            expected_sha256: expected_sha256.to_owned(),
            actual_sha256,
        });
    }
    file.sync_all().await?;
    Ok(())
}

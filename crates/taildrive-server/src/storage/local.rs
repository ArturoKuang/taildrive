use std::{
    io,
    ops::Range,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use futures_util::TryStreamExt;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncSeekExt},
    task::spawn_blocking,
};
use tokio_util::io::ReaderStream;

use super::{ByteStream, StorageBackend, StorageError};

#[derive(Clone)]
pub struct LocalFsStorage {
    root: PathBuf,
}

impl LocalFsStorage {
    pub async fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(root.join("blobs")).await?;
        fs::create_dir_all(root.join("tmp")).await?;
        Ok(Self { root })
    }

    fn blob(&self, key: &str) -> PathBuf {
        self.root.join("blobs").join(key)
    }
}

#[async_trait]
impl StorageBackend for LocalFsStorage {
    fn driver_name(&self) -> &'static str {
        "local-fs"
    }

    async fn publish(&self, key: &str, staged: &Path) -> Result<(), StorageError> {
        let dest = self.blob(key);
        let parent = dest.parent().expect("blob directory");
        fs::create_dir_all(parent).await?;
        fs::rename(staged, &dest).await?;
        Ok(())
    }

    async fn open(&self, key: &str, range: Range<u64>) -> Result<ByteStream, StorageError> {
        let mut file = fs::File::open(self.blob(key)).await?;
        let metadata = file.metadata().await?;
        if !metadata.is_file() || metadata.len() < range.end {
            return Err(StorageError::NotFound);
        }
        file.seek(io::SeekFrom::Start(range.start)).await?;
        Ok(Box::pin(
            ReaderStream::new(file.take(range.end - range.start)).map_err(StorageError::from),
        ))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        match fs::remove_file(self.blob(key)).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    async fn disk_free_bytes(&self) -> Result<Option<u64>, StorageError> {
        free_on_disk(self.root.clone()).await.map(Some)
    }
}

pub(crate) async fn free_on_disk(root: PathBuf) -> Result<u64, StorageError> {
    spawn_blocking(move || {
        // nix wraps libc::statvfs and releases the call from the async executor.
        let stat = nix::sys::statvfs::statvfs(&root).map_err(io::Error::from)?;
        let blocks = u64::try_from(u128::from(stat.blocks_available())).unwrap_or(u64::MAX);
        let block_size = u64::try_from(u128::from(stat.fragment_size())).unwrap_or(u64::MAX);
        Ok(blocks.saturating_mul(block_size))
    })
    .await?
}

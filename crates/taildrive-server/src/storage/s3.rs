use std::{ops::Range, path::Path};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt, stream};
use opendal::{Operator, services::S3};
use tokio::fs;
use tokio_util::io::ReaderStream;

use super::{ByteStream, StorageBackend, StorageError};

pub struct S3Config {
    pub endpoint: Option<String>,
    pub region: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
}

pub struct S3Storage {
    operator: Operator,
}

impl S3Storage {
    pub fn new(config: &S3Config) -> Result<Self, StorageError> {
        opendal::install_default();
        let mut builder = S3::default()
            .region(&config.region)
            .bucket(&config.bucket)
            .access_key_id(&config.access_key)
            .secret_access_key(&config.secret_key)
            .disable_config_load()
            .disable_ec2_metadata();
        if let Some(endpoint) = &config.endpoint {
            builder = builder.endpoint(endpoint);
        }
        Ok(Self::from_operator(Operator::new(builder)?))
    }

    pub fn from_operator(operator: Operator) -> Self {
        Self { operator }
    }
}

#[async_trait]
impl StorageBackend for S3Storage {
    fn driver_name(&self) -> &'static str {
        "s3"
    }

    async fn publish(&self, key: &str, staged: &Path) -> Result<(), StorageError> {
        let file = fs::File::open(staged).await?;
        let mut chunks = ReaderStream::with_capacity(file, 64 * 1024);
        let mut writer = self
            .operator
            .writer_with(key)
            .chunk(8 * 1024 * 1024)
            .await?;
        let result: Result<(), StorageError> = async {
            while let Some(chunk) = chunks.try_next().await? {
                writer.write(chunk).await?;
            }
            writer.close().await?;
            Ok(())
        }
        .await;
        if result.is_err()
            && let Err(error) = writer.abort().await
        {
            tracing::warn!(%error, "could not abort S3 multipart write");
        }
        result
    }

    async fn open(&self, key: &str, range: Range<u64>) -> Result<ByteStream, StorageError> {
        if range.is_empty() {
            self.operator.stat(key).await?;
            return Ok(Box::pin(stream::empty()));
        }
        let reader = self.operator.reader(key).await?;
        let mut bytes = reader
            .into_bytes_stream(range)
            .await?
            .map_err(StorageError::from);
        // The OpenDAL stream opens the GET on its first poll.
        let first = bytes.try_next().await?.ok_or(StorageError::NotFound)?;
        Ok(Box::pin(stream::once(async { Ok(first) }).chain(bytes)))
    }

    async fn delete(&self, key: &str) -> Result<(), StorageError> {
        self.operator.delete(key).await?;
        Ok(())
    }

    async fn disk_free_bytes(&self) -> Result<Option<u64>, StorageError> {
        Ok(None)
    }
}

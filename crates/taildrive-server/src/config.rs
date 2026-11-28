use std::{env, net::SocketAddr, path::PathBuf, sync::Arc};

use crate::storage::{LocalFsStorage, S3Config, S3Storage, StorageBackend, StorageError};

pub const FIVE_GIB: u64 = 5 * 1024 * 1024 * 1024;

pub struct Config {
    pub data_dir: PathBuf,
    pub quota_bytes: u64,
    pub free_floor_bytes: u64,
    pub api_tokens: Vec<String>,
    pub node_name: String,
    pub web_dir: Option<PathBuf>,
    pub releases_dir: Option<PathBuf>,
    pub bind: SocketAddr,
    pub storage: StorageConfig,
    /// Base URL clients reach this server at; the pairing QR carries `<url>/v1/reader`.
    pub reader_public_url: String,
    /// Reader book-store quota in bytes, separate from `quota_bytes`. `None` = no cap.
    pub reader_book_quota_bytes: Option<u64>,
}

pub const DEFAULT_READER_PUBLIC_URL: &str = "https://arturos-mac-mini.tail8dc761.ts.net";

pub enum StorageConfig {
    LocalFs,
    S3(S3Config),
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{0} is missing or invalid")]
    Invalid(&'static str),
    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl Config {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            quota_bytes: FIVE_GIB,
            free_floor_bytes: FIVE_GIB,
            api_tokens: Vec::new(),
            node_name: "macmini-ci".to_owned(),
            web_dir: None,
            releases_dir: None,
            bind: SocketAddr::from(([127, 0, 0, 1], 8787)),
            storage: StorageConfig::LocalFs,
            reader_public_url: DEFAULT_READER_PUBLIC_URL.to_owned(),
            reader_book_quota_bytes: None,
        }
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        let mut config = Self::new(env_or("TAILDRIVE_DATA_DIR", "./data"));
        config.quota_bytes = number("TAILDRIVE_QUOTA_BYTES", config.quota_bytes)?;
        config.free_floor_bytes = number("TAILDRIVE_FREE_FLOOR_BYTES", config.free_floor_bytes)?;
        config.api_tokens = env_or("TAILDRIVE_API_TOKENS", "")
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
            .collect();
        config.node_name = env_or("TAILDRIVE_NODE_NAME", &config.node_name);
        config.web_dir = env::var_os("TAILDRIVE_WEB_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        config.releases_dir = env::var_os("TAILDRIVE_RELEASES_DIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from);
        config.bind = env_or("TAILDRIVE_BIND", "127.0.0.1:8787")
            .parse()
            .map_err(|_| ConfigError::Invalid("TAILDRIVE_BIND"))?;
        config.reader_public_url = env_or("TAILDRIVE_READER_PUBLIC_URL", DEFAULT_READER_PUBLIC_URL);
        config.reader_book_quota_bytes =
            Some(number("TAILDRIVE_READER_BOOK_QUOTA_BYTES", 0)?).filter(|bytes| *bytes > 0);
        config.storage = match env_or("TAILDRIVE_STORAGE_DRIVER", "local-fs").as_str() {
            "local-fs" => StorageConfig::LocalFs,
            "s3" => StorageConfig::S3(S3Config {
                endpoint: env::var("TAILDRIVE_S3_ENDPOINT")
                    .ok()
                    .filter(|s| !s.is_empty()),
                region: env_or("TAILDRIVE_S3_REGION", "us-east-1"),
                bucket: env_or("TAILDRIVE_S3_BUCKET", "taildrive"),
                access_key: required("TAILDRIVE_S3_ACCESS_KEY")?,
                secret_key: required("TAILDRIVE_S3_SECRET_KEY")?,
            }),
            _ => return Err(ConfigError::Invalid("TAILDRIVE_STORAGE_DRIVER")),
        };
        Ok(config)
    }

    pub(crate) async fn build_storage(&self) -> Result<Arc<dyn StorageBackend>, ConfigError> {
        match &self.storage {
            StorageConfig::LocalFs => Ok(Arc::new(
                LocalFsStorage::new(&self.data_dir)
                    .await
                    .map_err(StorageError::from)?,
            )),
            StorageConfig::S3(config) => Ok(Arc::new(S3Storage::new(config)?)),
        }
    }
}

fn env_or(key: &str, default: &str) -> String {
    env::var(key).unwrap_or_else(|_| default.to_owned())
}

fn number(key: &'static str, default: u64) -> Result<u64, ConfigError> {
    match env::var(key) {
        Ok(value) => value.parse().map_err(|_| ConfigError::Invalid(key)),
        Err(env::VarError::NotPresent) => Ok(default),
        Err(_) => Err(ConfigError::Invalid(key)),
    }
}

fn required(key: &'static str) -> Result<String, ConfigError> {
    env::var(key)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(ConfigError::Invalid(key))
}

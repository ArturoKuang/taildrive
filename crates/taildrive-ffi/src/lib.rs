mod models;
mod sync;
mod transfer;

use std::{fs::File, io::Read, sync::Arc, time::Duration};

use taildrive_core::{
    client::{ClientError, TailDriveClient},
    hash::Sha256Hasher,
    models::CreateShareRequest,
    path::{PathError, conflict_path, parse_folder_path, parse_path},
};
use tokio::runtime::{Builder, Runtime};

pub use models::*;
pub use sync::*;
pub use transfer::TransferObserver;

uniffi::setup_scaffolding!();

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum FfiError {
    #[error("{message}")]
    InvalidPath { message: String },
    #[error("{message}")]
    Configuration { message: String },
    #[error("{message}")]
    Io { message: String },
    #[error("{message}")]
    Transport { message: String, retryable: bool },
    #[error("HTTP {status}: {code}: {message}")]
    Http {
        status: u16,
        code: String,
        message: String,
        retryable: bool,
    },
    #[error("{message}")]
    Integrity { message: String },
    #[error("transfer cancelled")]
    Cancelled,
}

impl From<PathError> for FfiError {
    fn from(error: PathError) -> Self {
        Self::InvalidPath {
            message: error.to_string(),
        }
    }
}

impl From<std::io::Error> for FfiError {
    fn from(error: std::io::Error) -> Self {
        Self::Io {
            message: error.to_string(),
        }
    }
}

impl From<ClientError> for FfiError {
    fn from(error: ClientError) -> Self {
        let retryable = error.is_retryable();
        match error {
            ClientError::Api { status, body } => Self::Http {
                status: status.as_u16(),
                code: body.error.code,
                message: body.error.message,
                retryable,
            },
            ClientError::UnexpectedStatus(status) => Self::Http {
                status: status.as_u16(),
                code: String::new(),
                message: status.to_string(),
                retryable,
            },
            ClientError::Transport(error) => Self::Transport {
                message: error.without_url().to_string(),
                retryable,
            },
            ClientError::InvalidBaseUrl => Self::Configuration {
                message: error.to_string(),
            },
        }
    }
}

#[derive(uniffi::Object)]
pub struct TailDriveFfiClient {
    client: TailDriveClient,
    runtime: Runtime,
}

#[uniffi::export]
impl TailDriveFfiClient {
    #[uniffi::constructor]
    pub fn new(base_url: String, bearer_token: Option<String>) -> Result<Arc<Self>, FfiError> {
        let client =
            TailDriveClient::with_io_timeout(&base_url, bearer_token, Duration::from_secs(30))?;
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()?;
        Ok(Arc::new(Self { client, runtime }))
    }

    pub fn health(&self) -> Result<HealthResponse, FfiError> {
        Ok(self.runtime.block_on(self.client.health())?)
    }

    pub fn list_files(
        &self,
        path: String,
        cursor: Option<String>,
        limit: Option<u16>,
    ) -> Result<ListFilesResponse, FfiError> {
        let path = parse_folder_path(Some(&path))?;
        Ok(self.runtime.block_on(self.client.list_files(
            path.as_str(),
            cursor.as_deref(),
            limit,
        ))?)
    }

    pub fn create_folder(&self, path: String) -> Result<FileItem, FfiError> {
        let path = parse_path(&path)?;
        Ok(self
            .runtime
            .block_on(self.client.create_folder(path.as_str()))?)
    }

    pub fn upload_file(
        &self,
        remote_path: String,
        local_file_path: String,
        if_match: Option<String>,
    ) -> Result<UploadResult, FfiError> {
        self.upload_with_progress(remote_path, local_file_path, if_match, None)
    }

    pub fn upload_with_progress(
        &self,
        remote_path: String,
        local_file_path: String,
        if_match: Option<String>,
        observer: Option<Box<dyn TransferObserver>>,
    ) -> Result<UploadResult, FfiError> {
        let remote = parse_path(&remote_path)?;
        self.runtime.block_on(transfer::upload(
            &self.client,
            remote.as_str(),
            local_file_path.as_ref(),
            if_match.as_deref(),
            observer.map(Arc::from),
        ))
    }

    pub fn download_file(
        &self,
        remote_path: String,
        dest_local_path: String,
    ) -> Result<(), FfiError> {
        self.download_with_progress(remote_path, dest_local_path, None)
    }

    pub fn download_with_progress(
        &self,
        remote_path: String,
        dest_local_path: String,
        observer: Option<Box<dyn TransferObserver>>,
    ) -> Result<(), FfiError> {
        let remote = parse_path(&remote_path)?;
        self.runtime.block_on(transfer::download(
            &self.client,
            remote.as_str(),
            dest_local_path.as_ref(),
            observer.as_deref(),
        ))
    }

    pub fn delete_path(&self, path: String, recursive: bool) -> Result<DeleteResponse, FfiError> {
        let path = parse_path(&path)?;
        Ok(self
            .runtime
            .block_on(self.client.delete_file(path.as_str(), recursive))?)
    }

    pub fn create_share(
        &self,
        path: String,
        expires_in_seconds: u64,
    ) -> Result<ShareItem, FfiError> {
        let request = CreateShareRequest {
            path: parse_path(&path)?.to_string(),
            expires_in_seconds,
        };
        Ok(self.runtime.block_on(self.client.create_share(&request))?)
    }

    pub fn list_shares(&self) -> Result<Vec<ShareItem>, FfiError> {
        Ok(self.runtime.block_on(self.client.list_shares())?.items)
    }

    pub fn revoke_share(&self, id: String) -> Result<(), FfiError> {
        Ok(self.runtime.block_on(self.client.revoke_share(&id))?)
    }
}

#[uniffi::export]
pub fn normalize_drive_path(raw: String) -> Result<String, FfiError> {
    Ok(parse_path(&raw)?.to_string())
}

#[uniffi::export]
pub fn format_conflict_path(path: String, host: String, ts: String) -> Result<String, FfiError> {
    Ok(conflict_path(&parse_path(&path)?, &host, &ts).to_string())
}

#[uniffi::export]
pub fn hash_file_sha256(local_path: String) -> Result<String, FfiError> {
    let mut file = File::open(local_path)?;
    let mut buffer = vec![0; 64 * 1024];
    let mut sha256 = Sha256Hasher::new();
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(sha256.finish());
        }
        sha256.update(&buffer[..read]);
    }
}

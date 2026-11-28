use std::{path::Path, sync::OnceLock};

use taildrive_core::update::{
    self, UpdateError, UpdaterClient, default_manifest_url, default_public_key_b64,
};
use tokio::runtime::{Builder, Runtime};

use crate::{FfiError, TransferObserver};

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct BuildInfo {
    pub version: String,
    pub channel: String,
    pub build_number: u64,
    pub commit_sha: String,
    pub short_sha: String,
    pub built_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct UpdateRelease {
    pub version: String,
    pub channel: String,
    pub build_number: u64,
    pub commit_sha: String,
    pub short_sha: String,
    pub published_at: String,
    pub release_url: String,
    pub notes: String,
    pub artifact_key: String,
    pub download_url: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum UpdateCheck {
    UpToDate {
        current: BuildInfo,
    },
    Available {
        current: BuildInfo,
        release: UpdateRelease,
    },
}

impl From<update::BuildInfo> for BuildInfo {
    fn from(info: update::BuildInfo) -> Self {
        Self {
            version: info.version,
            channel: info.channel,
            build_number: info.build_number,
            commit_sha: info.commit_sha,
            short_sha: info.short_sha,
            built_at: info.built_at,
        }
    }
}

impl From<update::UpdateRelease> for UpdateRelease {
    fn from(release: update::UpdateRelease) -> Self {
        Self {
            version: release.version,
            channel: release.channel,
            build_number: release.build_number,
            commit_sha: release.commit_sha,
            short_sha: release.short_sha,
            published_at: release.published_at,
            release_url: release.release_url,
            notes: release.notes,
            artifact_key: release.artifact_key,
            download_url: release.download_url,
            sha256: release.sha256,
            size_bytes: release.size_bytes,
        }
    }
}

impl From<UpdateRelease> for update::UpdateRelease {
    fn from(release: UpdateRelease) -> Self {
        Self {
            version: release.version,
            channel: release.channel,
            build_number: release.build_number,
            commit_sha: release.commit_sha,
            short_sha: release.short_sha,
            published_at: release.published_at,
            release_url: release.release_url,
            notes: release.notes,
            artifact_key: release.artifact_key,
            download_url: release.download_url,
            sha256: release.sha256,
            size_bytes: release.size_bytes,
        }
    }
}

impl From<UpdateError> for FfiError {
    fn from(error: UpdateError) -> Self {
        match error {
            UpdateError::InvalidUrl(message) => Self::Configuration { message },
            UpdateError::Transport(err) => Self::Transport {
                message: err.without_url().to_string(),
                retryable: true,
            },
            UpdateError::HttpStatus(status) => Self::Http {
                status: status.as_u16(),
                code: "ERR_UPDATE_HTTP".to_owned(),
                message: status.to_string(),
                retryable: status.is_server_error() || status.as_u16() == 429,
            },
            UpdateError::InvalidSignature
            | UpdateError::ChecksumMismatch { .. }
            | UpdateError::SizeMismatch { .. } => Self::Integrity {
                message: error.to_string(),
            },
            UpdateError::InvalidManifest(message)
            | UpdateError::MissingArtifact(message)
            | UpdateError::Crypto(message) => Self::Configuration { message },
            UpdateError::Io(err) => Self::Io {
                message: err.to_string(),
            },
            UpdateError::Cancelled => Self::Cancelled,
        }
    }
}

fn updater_runtime() -> Result<&'static Runtime, FfiError> {
    static RUNTIME: OnceLock<Result<Runtime, String>> = OnceLock::new();
    match RUNTIME.get_or_init(|| {
        Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .map_err(|err| err.to_string())
    }) {
        Ok(rt) => Ok(rt),
        Err(message) => Err(FfiError::Io {
            message: message.clone(),
        }),
    }
}

#[uniffi::export]
pub fn current_build_info() -> BuildInfo {
    update::current_build_info().into()
}

#[uniffi::export]
pub fn check_for_update(
    artifact_key: String,
    manifest_url: Option<String>,
    allow_dev_upgrade: bool,
) -> Result<UpdateCheck, FfiError> {
    check_for_update_with_key(artifact_key, manifest_url, None, allow_dev_upgrade)
}

#[uniffi::export]
pub fn check_for_update_with_key(
    artifact_key: String,
    manifest_url: Option<String>,
    public_key_b64: Option<String>,
    allow_dev_upgrade: bool,
) -> Result<UpdateCheck, FfiError> {
    check_for_update_server_first(
        artifact_key,
        None,
        None,
        manifest_url,
        public_key_b64,
        allow_dev_upgrade,
    )
}

#[uniffi::export]
pub fn check_for_update_server_first(
    artifact_key: String,
    server_base_url: Option<String>,
    bearer_token: Option<String>,
    fallback_manifest_url: Option<String>,
    public_key_b64: Option<String>,
    allow_dev_upgrade: bool,
) -> Result<UpdateCheck, FfiError> {
    let runtime = updater_runtime()?;
    let client = UpdaterClient::new()?;
    let current = update::current_build_info();
    let fallback_url = fallback_manifest_url
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(default_manifest_url);
    let key = public_key_b64
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(default_public_key_b64);

    let check = runtime.block_on(client.check_for_update_server_first(
        &current,
        &artifact_key,
        server_base_url.as_deref(),
        bearer_token.as_deref(),
        &fallback_url,
        &key,
        allow_dev_upgrade,
    ))?;
    Ok(match check {
        update::UpdateCheck::UpToDate { current } => UpdateCheck::UpToDate {
            current: current.into(),
        },
        update::UpdateCheck::Available { current, release } => UpdateCheck::Available {
            current: current.into(),
            release: (*release).into(),
        },
    })
}

#[uniffi::export]
pub fn download_update_artifact(
    release: UpdateRelease,
    dest_path: String,
    observer: Option<Box<dyn TransferObserver>>,
) -> Result<(), FfiError> {
    download_update_artifact_with_auth(release, dest_path, None, None, observer)
}

#[uniffi::export]
pub fn download_update_artifact_with_auth(
    release: UpdateRelease,
    dest_path: String,
    auth_origin: Option<String>,
    bearer_token: Option<String>,
    observer: Option<Box<dyn TransferObserver>>,
) -> Result<(), FfiError> {
    let runtime = updater_runtime()?;
    let client = UpdaterClient::new()?;
    let core_release: update::UpdateRelease = release.into();
    runtime.block_on(client.download_verified_artifact_with_auth(
        &core_release,
        Path::new(&dest_path),
        auth_origin.as_deref(),
        bearer_token.as_deref(),
        |bytes, total| match observer.as_deref() {
            Some(cb) => cb.progress(bytes, total),
            None => true,
        },
    ))?;
    Ok(())
}

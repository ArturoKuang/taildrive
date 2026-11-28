use std::{
    collections::BTreeMap,
    path::Path,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use futures_util::StreamExt;
use reqwest::{StatusCode, Url, redirect::Policy};
use ring::{
    rand::SystemRandom,
    signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey},
};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio::io::AsyncWriteExt;

use crate::hash::Sha256Hasher;

pub const DEFAULT_RELEASE_BASE_URL: &str =
    "https://github.com/ArturoKuang/taildrive/releases/download";
pub const DEFAULT_MANIFEST_URL: &str =
    "https://github.com/ArturoKuang/taildrive/releases/download/main-latest/manifest.json";
// Embedded Ed25519 public key (raw 32-byte key, base64-encoded) matching
// the repository's TAILDRIVE_UPDATE_SIGNING_KEY secret.
pub const DEFAULT_PUBLIC_KEY_B64: &str = "m9XNhzyeKbEUCl1mzgyONt8LsQdx1kAjibEoIifCdOQ=";

pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildInfo {
    pub version: String,
    pub channel: String,
    pub build_number: u64,
    pub commit_sha: String,
    pub short_sha: String,
    pub built_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseArtifact {
    pub url: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    pub channel: String,
    pub version: String,
    pub build_number: u64,
    pub commit_sha: String,
    pub published_at: String,
    pub release_url: String,
    #[serde(default)]
    pub notes: String,
    pub artifacts: BTreeMap<String, ReleaseArtifact>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateCheck {
    UpToDate {
        current: BuildInfo,
    },
    Available {
        current: BuildInfo,
        release: Box<UpdateRelease>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum UpdateError {
    #[error("invalid update URL: {0}")]
    InvalidUrl(String),
    #[error("network error while checking for updates: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("update server returned HTTP {0}")]
    HttpStatus(StatusCode),
    #[error("update manifest signature verification failed")]
    InvalidSignature,
    #[error("invalid update manifest: {0}")]
    InvalidManifest(String),
    #[error("no release artifact found for target '{0}'")]
    MissingArtifact(String),
    #[error("artifact checksum mismatch (expected {expected}, got {actual})")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("artifact size mismatch (expected {expected} bytes, got {actual} bytes)")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("update I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("update download cancelled")]
    Cancelled,
    #[error("signing key error: {0}")]
    Crypto(String),
}

pub fn current_build_info() -> BuildInfo {
    let base_version = env!("CARGO_PKG_VERSION");
    let channel = option_env!("TAILDRIVE_CHANNEL")
        .filter(|value| !value.is_empty())
        .unwrap_or("dev")
        .to_owned();
    let build_number = option_env!("TAILDRIVE_BUILD_NUMBER")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let commit_sha = option_env!("TAILDRIVE_COMMIT_SHA")
        .filter(|value| !value.is_empty())
        .unwrap_or("dev")
        .to_owned();
    let short_sha = short_commit_sha(&commit_sha);
    let built_at = option_env!("TAILDRIVE_BUILT_AT").unwrap_or("").to_owned();
    let version = if channel == "dev" || build_number == 0 {
        format!("{base_version}-dev")
    } else {
        format!("{base_version}-{channel}.{build_number}+{short_sha}")
    };

    BuildInfo {
        version,
        channel,
        build_number,
        commit_sha,
        short_sha,
        built_at,
    }
}

pub fn default_manifest_url() -> String {
    option_env!("TAILDRIVE_UPDATE_MANIFEST_URL")
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_MANIFEST_URL)
        .to_owned()
}

pub fn default_public_key_b64() -> String {
    option_env!("TAILDRIVE_UPDATE_PUBLIC_KEY")
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PUBLIC_KEY_B64)
        .to_owned()
}

pub fn manifest_url_for_tag(tag: &str) -> String {
    let trimmed = tag.trim().trim_matches('/');
    format!("{DEFAULT_RELEASE_BASE_URL}/{trimmed}/manifest.json")
}

pub fn server_manifest_url(server_base_url: &str) -> Result<String, UpdateError> {
    let mut base = validate_update_url(server_base_url.trim())?;
    base.set_query(None);
    base.set_fragment(None);
    {
        let mut segments = base
            .path_segments_mut()
            .map_err(|_| UpdateError::InvalidUrl(server_base_url.to_owned()))?;
        segments.pop_if_empty();
        segments.extend(["v1", "updates", "manifest.json"]);
    }
    Ok(base.to_string())
}

pub fn resolve_artifact_url(manifest_url: &str, artifact_url: &str) -> Result<Url, UpdateError> {
    let resolved = if artifact_url.starts_with('/') {
        let base = validate_update_url(manifest_url)?;
        base.join(artifact_url)
            .map_err(|_| UpdateError::InvalidUrl(artifact_url.to_owned()))?
    } else {
        Url::parse(artifact_url).map_err(|_| UpdateError::InvalidUrl(artifact_url.to_owned()))?
    };
    validate_update_url(resolved.as_str())
}

fn same_origin(a: &Url, b: &Url) -> bool {
    a.scheme() == b.scheme()
        && a.host_str() == b.host_str()
        && a.port_or_known_default() == b.port_or_known_default()
}

pub fn short_commit_sha(commit_sha: &str) -> String {
    commit_sha.chars().take(7).collect()
}

pub fn is_update_available(
    current: &BuildInfo,
    manifest: &ReleaseManifest,
    allow_dev_upgrade: bool,
) -> bool {
    if manifest.commit_sha.is_empty() {
        return false;
    }
    if current.channel == "dev" || current.build_number == 0 {
        return allow_dev_upgrade && manifest.commit_sha != current.commit_sha;
    }
    manifest.commit_sha != current.commit_sha && manifest.build_number > current.build_number
}

pub fn generate_signing_keypair() -> Result<(String, String), UpdateError> {
    let rng = SystemRandom::new();
    let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng)
        .map_err(|_| UpdateError::Crypto("failed to generate Ed25519 keypair".to_owned()))?;
    let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref())
        .map_err(|_| UpdateError::Crypto("failed to parse generated Ed25519 keypair".to_owned()))?;
    Ok((
        BASE64.encode(pkcs8.as_ref()),
        BASE64.encode(pair.public_key().as_ref()),
    ))
}

pub fn sign_manifest_bytes(manifest_bytes: &[u8], pkcs8_b64: &str) -> Result<String, UpdateError> {
    let pkcs8 = BASE64
        .decode(pkcs8_b64.trim())
        .map_err(|_| UpdateError::Crypto("invalid base64 signing key".to_owned()))?;
    let pair = Ed25519KeyPair::from_pkcs8(&pkcs8)
        .map_err(|_| UpdateError::Crypto("invalid PKCS#8 Ed25519 signing key".to_owned()))?;
    let sig = pair.sign(manifest_bytes);
    Ok(BASE64.encode(sig.as_ref()))
}

pub fn verify_manifest_signature(
    manifest_bytes: &[u8],
    sig_b64: &str,
    public_key_b64: &str,
) -> Result<(), UpdateError> {
    let public_key = BASE64
        .decode(public_key_b64.trim())
        .map_err(|_| UpdateError::InvalidSignature)?;
    let signature = BASE64
        .decode(sig_b64.trim())
        .map_err(|_| UpdateError::InvalidSignature)?;
    UnparsedPublicKey::new(&ED25519, public_key)
        .verify(manifest_bytes, &signature)
        .map_err(|_| UpdateError::InvalidSignature)
}

pub fn validate_update_url(raw: &str) -> Result<Url, UpdateError> {
    let url = Url::parse(raw).map_err(|_| UpdateError::InvalidUrl(raw.to_owned()))?;
    match url.scheme() {
        "https" => Ok(url),
        "http" => {
            let is_loopback = matches!(
                url.host_str(),
                Some("127.0.0.1") | Some("localhost") | Some("[::1]") | Some("::1")
            );
            if is_loopback {
                Ok(url)
            } else {
                Err(UpdateError::InvalidUrl(
                    "only HTTPS (or loopback HTTP for testing) is allowed".to_owned(),
                ))
            }
        }
        _ => Err(UpdateError::InvalidUrl(raw.to_owned())),
    }
}

#[derive(Clone)]
pub struct UpdaterClient {
    http: reqwest::Client,
}

impl UpdaterClient {
    pub fn new() -> Result<Self, UpdateError> {
        let info = current_build_info();
        let http = reqwest::Client::builder()
            .redirect(Policy::limited(10))
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(format!("taildrive-updater/{}", info.version))
            .build()?;
        Ok(Self { http })
    }

    pub async fn fetch_verified_manifest(
        &self,
        manifest_url: &str,
        public_key_b64: &str,
    ) -> Result<ReleaseManifest, UpdateError> {
        self.fetch_verified_manifest_with_auth(manifest_url, public_key_b64, None)
            .await
    }

    pub async fn fetch_verified_manifest_with_auth(
        &self,
        manifest_url: &str,
        public_key_b64: &str,
        bearer_token: Option<&str>,
    ) -> Result<ReleaseManifest, UpdateError> {
        let url = validate_update_url(manifest_url)?;
        let sig_url = validate_update_url(&format!("{manifest_url}.sig"))?;

        let mut manifest_req = self.http.get(url);
        let mut sig_req = self.http.get(sig_url);
        if let Some(token) = bearer_token.filter(|t| !t.trim().is_empty()) {
            manifest_req = manifest_req.bearer_auth(token);
            sig_req = sig_req.bearer_auth(token);
        }

        let manifest_resp = manifest_req.send().await?;
        if !manifest_resp.status().is_success() {
            return Err(UpdateError::HttpStatus(manifest_resp.status()));
        }
        let manifest_bytes = manifest_resp.bytes().await?;

        let sig_resp = sig_req.send().await?;
        if !sig_resp.status().is_success() {
            return Err(UpdateError::HttpStatus(sig_resp.status()));
        }
        let sig_text = sig_resp.text().await?;

        verify_manifest_signature(&manifest_bytes, &sig_text, public_key_b64)?;

        let manifest: ReleaseManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|error| UpdateError::InvalidManifest(error.to_string()))?;
        if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
            return Err(UpdateError::InvalidManifest(format!(
                "unsupported manifest schema_version {}",
                manifest.schema_version
            )));
        }
        if manifest.commit_sha.is_empty() || manifest.version.is_empty() {
            return Err(UpdateError::InvalidManifest(
                "manifest is missing version or commit_sha".to_owned(),
            ));
        }
        Ok(manifest)
    }

    pub async fn check_for_update(
        &self,
        current: &BuildInfo,
        artifact_key: &str,
        manifest_url: &str,
        public_key_b64: &str,
        allow_dev_upgrade: bool,
    ) -> Result<UpdateCheck, UpdateError> {
        self.check_for_update_with_auth(
            current,
            artifact_key,
            manifest_url,
            public_key_b64,
            None,
            allow_dev_upgrade,
        )
        .await
    }

    pub async fn check_for_update_with_auth(
        &self,
        current: &BuildInfo,
        artifact_key: &str,
        manifest_url: &str,
        public_key_b64: &str,
        bearer_token: Option<&str>,
        allow_dev_upgrade: bool,
    ) -> Result<UpdateCheck, UpdateError> {
        let manifest = self
            .fetch_verified_manifest_with_auth(manifest_url, public_key_b64, bearer_token)
            .await?;
        let artifact = manifest
            .artifacts
            .get(artifact_key)
            .ok_or_else(|| UpdateError::MissingArtifact(artifact_key.to_owned()))?;
        let resolved_url = resolve_artifact_url(manifest_url, &artifact.url)?;

        if !is_update_available(current, &manifest, allow_dev_upgrade) {
            return Ok(UpdateCheck::UpToDate {
                current: current.clone(),
            });
        }

        let release = manifest_to_release(&manifest, artifact_key, artifact, resolved_url.as_str());
        Ok(UpdateCheck::Available {
            current: current.clone(),
            release: Box::new(release),
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn check_for_update_server_first(
        &self,
        current: &BuildInfo,
        artifact_key: &str,
        server_base_url: Option<&str>,
        bearer_token: Option<&str>,
        fallback_manifest_url: &str,
        public_key_b64: &str,
        allow_dev_upgrade: bool,
    ) -> Result<UpdateCheck, UpdateError> {
        if let Some(base) = server_base_url.map(str::trim).filter(|s| !s.is_empty())
            && let Ok(server_url) = server_manifest_url(base)
            && let Ok(check) = self
                .check_for_update_with_auth(
                    current,
                    artifact_key,
                    &server_url,
                    public_key_b64,
                    bearer_token,
                    allow_dev_upgrade,
                )
                .await
        {
            return Ok(check);
        }

        self.check_for_update_with_auth(
            current,
            artifact_key,
            fallback_manifest_url,
            public_key_b64,
            None,
            allow_dev_upgrade,
        )
        .await
    }

    pub async fn fetch_release_artifact(
        &self,
        artifact_key: &str,
        manifest_url: &str,
        public_key_b64: &str,
    ) -> Result<UpdateRelease, UpdateError> {
        let manifest = self
            .fetch_verified_manifest(manifest_url, public_key_b64)
            .await?;
        let artifact = manifest
            .artifacts
            .get(artifact_key)
            .ok_or_else(|| UpdateError::MissingArtifact(artifact_key.to_owned()))?;
        let resolved_url = resolve_artifact_url(manifest_url, &artifact.url)?;
        Ok(manifest_to_release(
            &manifest,
            artifact_key,
            artifact,
            resolved_url.as_str(),
        ))
    }

    pub async fn download_verified_artifact<F>(
        &self,
        release: &UpdateRelease,
        dest_path: &Path,
        on_progress: F,
    ) -> Result<(), UpdateError>
    where
        F: FnMut(u64, u64) -> bool,
    {
        self.download_verified_artifact_with_auth(release, dest_path, None, None, on_progress)
            .await
    }

    pub async fn download_verified_artifact_with_auth<F>(
        &self,
        release: &UpdateRelease,
        dest_path: &Path,
        auth_origin: Option<&str>,
        bearer_token: Option<&str>,
        mut on_progress: F,
    ) -> Result<(), UpdateError>
    where
        F: FnMut(u64, u64) -> bool,
    {
        let url = validate_update_url(&release.download_url)?;
        let parent = dest_path
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        tokio::fs::create_dir_all(parent).await?;

        let mut request = self.http.get(url.clone());
        if let (Some(origin_raw), Some(token)) = (
            auth_origin.map(str::trim).filter(|s| !s.is_empty()),
            bearer_token.filter(|t| !t.trim().is_empty()),
        ) && let Ok(origin_url) = validate_update_url(origin_raw)
            && same_origin(&url, &origin_url)
        {
            request = request.bearer_auth(token);
        }

        let response = request.send().await?;
        if !response.status().is_success() {
            return Err(UpdateError::HttpStatus(response.status()));
        }

        let total = if release.size_bytes > 0 {
            release.size_bytes
        } else {
            response.content_length().unwrap_or(0)
        };

        let temp = NamedTempFile::new_in(parent)?;
        let std_file = temp.reopen()?;
        let mut file = tokio::fs::File::from_std(std_file);
        let mut hasher = Sha256Hasher::new();
        let mut downloaded = 0u64;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            downloaded = downloaded.saturating_add(chunk.len() as u64);
            if !on_progress(downloaded, total) {
                return Err(UpdateError::Cancelled);
            }
        }
        file.flush().await?;
        file.sync_all().await?;
        drop(file);

        if release.size_bytes > 0 && downloaded != release.size_bytes {
            return Err(UpdateError::SizeMismatch {
                expected: release.size_bytes,
                actual: downloaded,
            });
        }

        let actual_sha = hasher.finish();
        let expected_sha = release.sha256.to_ascii_lowercase();
        if actual_sha != expected_sha {
            return Err(UpdateError::ChecksumMismatch {
                expected: expected_sha,
                actual: actual_sha,
            });
        }

        temp.persist(dest_path)
            .map_err(|error| UpdateError::Io(error.error))?;
        Ok(())
    }
}

fn manifest_to_release(
    manifest: &ReleaseManifest,
    artifact_key: &str,
    artifact: &ReleaseArtifact,
    resolved_download_url: &str,
) -> UpdateRelease {
    UpdateRelease {
        version: manifest.version.clone(),
        channel: manifest.channel.clone(),
        build_number: manifest.build_number,
        commit_sha: manifest.commit_sha.clone(),
        short_sha: short_commit_sha(&manifest.commit_sha),
        published_at: manifest.published_at.clone(),
        release_url: manifest.release_url.clone(),
        notes: manifest.notes.clone(),
        artifact_key: artifact_key.to_owned(),
        download_url: resolved_download_url.to_owned(),
        sha256: artifact.sha256.clone(),
        size_bytes: artifact.size,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha256_hex;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn ed25519_sign_and_verify_round_trip() {
        let (priv_b64, pub_b64) = generate_signing_keypair().expect("keygen");
        let payload = br#"{"schema_version":1,"version":"0.1.0-main.5+abcdef1"}"#;
        let sig = sign_manifest_bytes(payload, &priv_b64).expect("sign");
        verify_manifest_signature(payload, &sig, &pub_b64).expect("valid sig");

        let tampered = br#"{"schema_version":1,"version":"0.1.0-main.6+abcdef1"}"#;
        assert!(matches!(
            verify_manifest_signature(tampered, &sig, &pub_b64),
            Err(UpdateError::InvalidSignature)
        ));

        let (_, other_pub) = generate_signing_keypair().expect("other keygen");
        assert!(matches!(
            verify_manifest_signature(payload, &sig, &other_pub),
            Err(UpdateError::InvalidSignature)
        ));
    }

    #[test]
    fn update_comparison_respects_dev_and_build_numbers() {
        let manifest = ReleaseManifest {
            schema_version: 1,
            channel: "main".to_owned(),
            version: "0.1.0-main.10+1111111".to_owned(),
            build_number: 10,
            commit_sha: "1111111111111111111111111111111111111111".to_owned(),
            published_at: "2026-11-27T21:00:00Z".to_owned(),
            release_url: "https://example.com/releases/tag/main-10".to_owned(),
            notes: String::new(),
            artifacts: BTreeMap::new(),
        };

        let dev = BuildInfo {
            version: "0.1.0-dev".to_owned(),
            channel: "dev".to_owned(),
            build_number: 0,
            commit_sha: "dev".to_owned(),
            short_sha: "dev".to_owned(),
            built_at: String::new(),
        };
        assert!(!is_update_available(&dev, &manifest, false));
        assert!(is_update_available(&dev, &manifest, true));

        let older = BuildInfo {
            version: "0.1.0-main.9+0000000".to_owned(),
            channel: "main".to_owned(),
            build_number: 9,
            commit_sha: "0000000000000000000000000000000000000000".to_owned(),
            short_sha: "0000000".to_owned(),
            built_at: String::new(),
        };
        assert!(is_update_available(&older, &manifest, false));

        let same = BuildInfo {
            version: manifest.version.clone(),
            channel: "main".to_owned(),
            build_number: 10,
            commit_sha: manifest.commit_sha.clone(),
            short_sha: "1111111".to_owned(),
            built_at: String::new(),
        };
        assert!(!is_update_available(&same, &manifest, true));

        let newer = BuildInfo {
            version: "0.1.0-main.11+2222222".to_owned(),
            channel: "main".to_owned(),
            build_number: 11,
            commit_sha: "2222222222222222222222222222222222222222".to_owned(),
            short_sha: "2222222".to_owned(),
            built_at: String::new(),
        };
        assert!(!is_update_available(&newer, &manifest, true));
    }

    async fn spawn_static_server(routes: Vec<(String, Vec<u8>)>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    if let Some((_, body)) = routes.iter().find(|(p, _)| p == path) {
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(header.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                    } else {
                        let _ = socket
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await;
                    }
                });
            }
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn end_to_end_check_and_verified_download() {
        let (priv_b64, pub_b64) = generate_signing_keypair().expect("keygen");
        let payload = b"taildrive release binary payload".to_vec();
        let payload_sha = sha256_hex(&payload);

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);

        // Start a real mock server first to learn its exact URL.
        let base = spawn_static_server(vec![
            ("/artifact.bin".to_owned(), payload.clone()),
            ("/bad.bin".to_owned(), b"corrupted".to_vec()),
        ])
        .await;
        let _ = addr;

        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "macos-app-arm64".to_owned(),
            ReleaseArtifact {
                url: format!("{base}/artifact.bin"),
                sha256: payload_sha.clone(),
                size: payload.len() as u64,
            },
        );
        artifacts.insert(
            "corrupted".to_owned(),
            ReleaseArtifact {
                url: format!("{base}/bad.bin"),
                sha256: payload_sha,
                size: 9,
            },
        );
        let manifest = ReleaseManifest {
            schema_version: 1,
            channel: "main".to_owned(),
            version: "0.1.0-main.42+abcdef1".to_owned(),
            build_number: 42,
            commit_sha: "abcdef1234567890abcdef1234567890abcdef12".to_owned(),
            published_at: "2026-11-27T21:00:00Z".to_owned(),
            release_url: "https://github.com/ArturoKuang/taildrive/releases/tag/main-42".to_owned(),
            notes: "Test release".to_owned(),
            artifacts,
        };
        let manifest_bytes = serde_json::to_vec(&manifest).expect("json");
        let sig = sign_manifest_bytes(&manifest_bytes, &priv_b64).expect("sign");

        let manifest_base = spawn_static_server(vec![
            ("/manifest.json".to_owned(), manifest_bytes),
            ("/manifest.json.sig".to_owned(), sig.into_bytes()),
        ])
        .await;

        let updater = UpdaterClient::new().expect("client");
        let current = BuildInfo {
            version: "0.1.0-main.41+0000000".to_owned(),
            channel: "main".to_owned(),
            build_number: 41,
            commit_sha: "0000000000000000000000000000000000000000".to_owned(),
            short_sha: "0000000".to_owned(),
            built_at: String::new(),
        };
        let manifest_url = format!("{manifest_base}/manifest.json");
        let check = updater
            .check_for_update(&current, "macos-app-arm64", &manifest_url, &pub_b64, false)
            .await
            .expect("check");
        let release = match check {
            UpdateCheck::Available { release, .. } => release,
            UpdateCheck::UpToDate { .. } => panic!("expected update"),
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let dest = dir.path().join("downloaded.bin");
        let mut last_seen = 0u64;
        updater
            .download_verified_artifact(&release, &dest, |bytes, _| {
                last_seen = bytes;
                true
            })
            .await
            .expect("download");
        assert_eq!(last_seen, payload.len() as u64);
        assert_eq!(std::fs::read(&dest).expect("read"), payload);

        let bad_release = updater
            .fetch_release_artifact("corrupted", &manifest_url, &pub_b64)
            .await
            .expect("bad release");
        let bad_dest = dir.path().join("bad.bin");
        let err = updater
            .download_verified_artifact(&bad_release, &bad_dest, |_, _| true)
            .await
            .expect_err("must reject bad checksum");
        assert!(matches!(err, UpdateError::ChecksumMismatch { .. }));
        assert!(!bad_dest.exists());
    }
}

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileType {
    File,
    Folder,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileItem {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(rename = "type")]
    pub kind: FileType,
    pub size_bytes: u64,
    pub mime: Option<String>,
    pub sha256: Option<String>,
    pub version: u64,
    pub etag: Option<String>,
    pub updated_at: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaStatus {
    Ok,
    Warn,
    Full,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuotaInfo {
    pub used_bytes: u64,
    pub limit_bytes: u64,
    pub remaining_bytes: u64,
    pub disk_free_bytes: Option<u64>,
    pub disk_floor_bytes: u64,
    pub status: QuotaStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub node: String,
    pub tailnet_user: Option<String>,
    pub storage_driver: String,
    pub quota: QuotaInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_build_number: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListFilesResponse {
    pub path: String,
    pub items: Vec<FileItem>,
    pub next_cursor: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadResponse {
    pub item: FileItem,
    pub quota: QuotaInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteResponse {
    pub deleted: bool,
    pub path: String,
    pub freed_bytes: u64,
    pub quota: QuotaInfo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitUploadRequest {
    pub path: String,
    pub size_bytes: u64,
    pub mime: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InitUploadResponse {
    pub upload_id: String,
    /// Parts are numbered from one. The last part may be shorter.
    pub part_size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateShareRequest {
    pub path: String,
    pub expires_in_seconds: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareItem {
    pub id: String,
    pub path: String,
    /// Relative URL on the server that issued this share.
    pub url: String,
    pub expires_at: String,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ListSharesResponse {
    pub items: Vec<ShareItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details: Value,
}

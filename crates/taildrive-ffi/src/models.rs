pub use taildrive_core::models::{
    DeleteResponse, FileItem, FileType, HealthResponse, ListFilesResponse, QuotaInfo, QuotaStatus,
    ShareItem,
};

#[uniffi::remote(Enum)]
enum FileType {
    File,
    Folder,
}

#[uniffi::remote(Enum)]
enum QuotaStatus {
    Ok,
    Warn,
    Full,
}

#[uniffi::remote(Record)]
struct FileItem {
    id: String,
    name: String,
    path: String,
    kind: FileType,
    size_bytes: u64,
    mime: Option<String>,
    sha256: Option<String>,
    version: u64,
    etag: Option<String>,
    updated_at: String,
}

#[uniffi::remote(Record)]
struct QuotaInfo {
    used_bytes: u64,
    limit_bytes: u64,
    remaining_bytes: u64,
    disk_free_bytes: Option<u64>,
    disk_floor_bytes: u64,
    status: QuotaStatus,
}

#[uniffi::remote(Record)]
struct HealthResponse {
    status: String,
    node: String,
    tailnet_user: Option<String>,
    storage_driver: String,
    quota: QuotaInfo,
}

#[uniffi::remote(Record)]
struct ListFilesResponse {
    path: String,
    items: Vec<FileItem>,
    next_cursor: Option<String>,
}

#[derive(uniffi::Record)]
pub struct UploadResult {
    pub item: FileItem,
    pub quota: QuotaInfo,
}

#[uniffi::remote(Record)]
struct DeleteResponse {
    deleted: bool,
    path: String,
    freed_bytes: u64,
    quota: QuotaInfo,
}

#[uniffi::remote(Record)]
struct ShareItem {
    id: String,
    path: String,
    url: String,
    expires_at: String,
    created_at: String,
}

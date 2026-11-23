export type FileType = 'file' | 'folder';

export interface FileItem {
  id: string;
  name: string;
  path: string;
  type: FileType;
  size_bytes: number;
  mime: string | null;
  sha256: string | null;
  version: number;
  etag: string | null;
  updated_at: string;
}

export type QuotaStatus = 'ok' | 'warn' | 'full';

export interface QuotaInfo {
  used_bytes: number;
  limit_bytes: number;
  remaining_bytes: number;
  disk_free_bytes: number | null;
  disk_floor_bytes: number;
  status: QuotaStatus;
}

export interface HealthResponse {
  status: string;
  node: string;
  tailnet_user: string | null;
  storage_driver: string;
  quota: QuotaInfo;
}

export interface ListFilesResponse {
  path: string;
  items: FileItem[];
  next_cursor: string | null;
}

export interface UploadResponse {
  item: FileItem;
  quota: QuotaInfo;
}

export interface DeleteResponse {
  deleted: boolean;
  path: string;
  freed_bytes: number;
  quota: QuotaInfo;
}

export interface InitUploadRequest {
  path: string;
  size_bytes: number;
  mime: string | null;
}

export interface InitUploadResponse {
  upload_id: string;
  part_size: number;
}

export interface CreateShareRequest {
  path: string;
  expires_in_seconds: number;
}

export interface ShareItem {
  id: string;
  path: string;
  url: string;
  expires_at: string;
  created_at: string;
}

export interface ListSharesResponse {
  items: ShareItem[];
}

export interface ErrorBody {
  code: string;
  message: string;
  retryable: boolean;
  details: Record<string, unknown>;
}

export interface ErrorEnvelope {
  error: ErrorBody;
}

export type NavTab = 'drive' | 'shares' | 'status';
export type SortField = 'name' | 'size' | 'modified';
export type SortDirection = 'asc' | 'desc';

export type ConnectionState = 'connected' | 'offline' | 'disconnected';

import type {
  CreateShareRequest,
  DeleteResponse,
  ErrorEnvelope,
  FileItem,
  HealthResponse,
  InitUploadRequest,
  InitUploadResponse,
  ListFilesResponse,
  ListSharesResponse,
  ShareItem,
  UploadResponse,
} from '../types/api';

const TOKEN_STORAGE_KEY = 'taildrive_bearer_token';
let sessionToken: string | null | undefined;

export function getStoredToken(): string | null {
  if (sessionToken !== undefined) return sessionToken;
  try {
    const val = localStorage.getItem(TOKEN_STORAGE_KEY);
    return val && val.trim().length > 0 ? val.trim() : null;
  } catch {
    return null;
  }
}

export function setStoredToken(token: string | null): void {
  sessionToken = token?.trim() || null;
  try {
    if (token && token.trim().length > 0) {
      localStorage.setItem(TOKEN_STORAGE_KEY, token.trim());
    } else {
      localStorage.removeItem(TOKEN_STORAGE_KEY);
    }
  } catch {
    // Ignore storage failures
  }
}

export class ApiClientError extends Error {
  readonly status: number;
  readonly code: string;
  readonly retryable: boolean;
  readonly details: Record<string, unknown>;
  readonly isNetworkError: boolean;

  constructor(opts: {
    status: number;
    code: string;
    message: string;
    retryable?: boolean;
    details?: Record<string, unknown>;
    isNetworkError?: boolean;
  }) {
    super(opts.message);
    this.name = 'ApiClientError';
    this.status = opts.status;
    this.code = opts.code;
    this.retryable = Boolean(opts.retryable);
    this.details = opts.details ?? {};
    this.isNetworkError = Boolean(opts.isNetworkError);
  }
}

function authHeaders(extra?: Record<string, string>, tokenOverride?: string | null): Record<string, string> {
  const headers: Record<string, string> = { ...(extra ?? {}) };
  const token = tokenOverride !== undefined ? tokenOverride : getStoredToken();
  if (token) {
    headers['Authorization'] = `Bearer ${token}`;
  }
  return headers;
}

async function parseErrorResponse(res: Response): Promise<ApiClientError> {
  let code = `HTTP_${res.status}`;
  let message = res.statusText || `Request failed (${res.status})`;
  let retryable = res.status >= 500 || res.status === 429;
  let details: Record<string, unknown> = {};

  try {
    const json = (await res.json()) as Partial<ErrorEnvelope>;
    if (json && json.error) {
      code = json.error.code || code;
      message = json.error.message || message;
      retryable = Boolean(json.error.retryable) || res.status >= 500 || res.status === 429;
      details = json.error.details ?? {};
    }
  } catch {
    // Non-JSON error body
  }

  return new ApiClientError({ status: res.status, code, message, retryable, details });
}

async function requestJson<T>(
  url: string,
  init: RequestInit = {},
  tokenOverride?: string | null,
): Promise<T> {
  let res: Response;
  try {
    res = await fetch(url, {
      cache: 'no-store',
      ...init,
      headers: authHeaders(init.headers as Record<string, string> | undefined, tokenOverride),
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === 'AbortError') {
      throw err;
    }
    throw new ApiClientError({
      status: 0,
      code: 'ERR_NETWORK',
      message: 'Unable to reach TailDrive server',
      retryable: true,
      isNetworkError: true,
    });
  }

  if (!res.ok) {
    throw await parseErrorResponse(res);
  }

  if (res.status === 204) {
    return undefined as T;
  }
  return (await res.json()) as T;
}

export async function probeHealth(tokenOverride?: string | null, signal?: AbortSignal): Promise<HealthResponse> {
  return requestJson<HealthResponse>('/v1/health', { signal }, tokenOverride);
}

export async function listFiles(
  path: string,
  cursor?: string | null,
  signal?: AbortSignal,
): Promise<ListFilesResponse> {
  const params = new URLSearchParams();
  if (path) params.set('path', path);
  if (cursor) params.set('cursor', cursor);
  params.set('limit', '1000');
  return requestJson<ListFilesResponse>(`/v1/files?${params.toString()}`, {
    signal,
  });
}

export async function listAllFilesInFolder(path: string, signal?: AbortSignal): Promise<FileItem[]> {
  const items: FileItem[] = [];
  let cursor: string | null = null;
  do {
    const res = await listFiles(path, cursor, signal);
    items.push(...res.items);
    cursor = res.next_cursor;
  } while (cursor);
  return items;
}

export async function createFolder(path: string): Promise<FileItem> {
  return requestJson<FileItem>('/v1/folders', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ path }),
  });
}

export async function deleteFile(path: string, recursive = false): Promise<DeleteResponse> {
  const params = new URLSearchParams({ path });
  if (recursive) params.set('recursive', 'true');
  return requestJson<DeleteResponse>(`/v1/files?${params.toString()}`, {
    method: 'DELETE',
  });
}

export function fileContentUrl(path: string): string {
  return `/v1/files/content?${new URLSearchParams({ path })}`;
}

export async function fetchFileContent(
  path: string,
  options?: { range?: string; signal?: AbortSignal },
): Promise<{
  blob: Blob;
  contentType: string;
  contentLength: number | null;
  isPartial: boolean;
}> {
  const extra: Record<string, string> = {};
  if (options?.range) {
    extra['Range'] = options.range;
  }

  let res: Response;
  try {
    res = await fetch(fileContentUrl(path), {
      cache: 'no-store',
      method: 'GET',
      headers: authHeaders(extra),
      signal: options?.signal,
    });
  } catch (err) {
    if (err instanceof DOMException && err.name === 'AbortError') {
      throw err;
    }
    throw new ApiClientError({
      status: 0,
      code: 'ERR_NETWORK',
      message: 'Unable to fetch file content',
      retryable: true,
      isNetworkError: true,
    });
  }

  if (!res.ok) {
    throw await parseErrorResponse(res);
  }

  const blob = await res.blob();
  const contentType = res.headers.get('content-type') || 'application/octet-stream';
  const cl = res.headers.get('content-length');
  return {
    blob,
    contentType,
    contentLength: cl ? Number(cl) : null,
    isPartial: res.status === 206,
  };
}

export async function listShares(signal?: AbortSignal): Promise<ListSharesResponse> {
  return requestJson<ListSharesResponse>('/v1/shares', { signal });
}

export async function createShare(req: CreateShareRequest): Promise<ShareItem> {
  return requestJson<ShareItem>('/v1/shares', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
  });
}

export async function revokeShare(id: string): Promise<void> {
  await requestJson<void>(`/v1/shares/${encodeURIComponent(id)}`, {
    method: 'DELETE',
  });
}

export async function initResumableUpload(req: InitUploadRequest): Promise<InitUploadResponse> {
  return requestJson<InitUploadResponse>('/v1/uploads', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(req),
  });
}

export async function completeResumableUpload(
  uploadId: string,
  sha256: string,
  ifMatch?: string,
): Promise<UploadResponse> {
  const headers: Record<string, string> = { 'X-Content-SHA256': sha256 };
  if (ifMatch) headers['If-Match'] = ifMatch;
  return requestJson<UploadResponse>(`/v1/uploads/${encodeURIComponent(uploadId)}/complete`, {
    method: 'POST',
    headers,
  });
}

export async function abortResumableUpload(uploadId: string): Promise<void> {
  await requestJson<void>(`/v1/uploads/${encodeURIComponent(uploadId)}`, {
    method: 'DELETE',
  });
}

function parseXhrError(xhr: XMLHttpRequest): ApiClientError {
  const status = xhr.status;
  if (status === 0) {
    return new ApiClientError({
      status: 0,
      code: 'ERR_NETWORK',
      message: 'Connection interrupted during upload',
      retryable: true,
      isNetworkError: true,
    });
  }
  let code = `HTTP_${status}`;
  let message = xhr.statusText || `Upload failed (${status})`;
  let retryable = status >= 500 || status === 429;
  let details: Record<string, unknown> = {};
  try {
    const parsed = JSON.parse(xhr.responseText) as Partial<ErrorEnvelope>;
    if (parsed && parsed.error) {
      code = parsed.error.code || code;
      message = parsed.error.message || message;
      retryable = Boolean(parsed.error.retryable) || status >= 500 || status === 429;
      details = parsed.error.details ?? {};
    }
  } catch {
    // ignore non-json
  }
  return new ApiClientError({ status, code, message, retryable, details });
}

export function putSingleFileXhr(opts: {
  path: string;
  file: File;
  sha256: string;
  ifMatch?: string;
  onProgress?: (loaded: number, total: number) => void;
  signal?: AbortSignal;
}): Promise<UploadResponse> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    const params = new URLSearchParams({ path: opts.path });
    xhr.open('PUT', `/v1/files?${params.toString()}`, true);

    const headers = authHeaders({
      'Content-Type': opts.file.type || 'application/octet-stream',
      'X-Content-SHA256': opts.sha256,
      ...(opts.ifMatch ? { 'If-Match': opts.ifMatch } : {}),
    });
    for (const [k, v] of Object.entries(headers)) {
      xhr.setRequestHeader(k, v);
    }

    const onAbort = () => {
      xhr.abort();
      reject(new DOMException('Upload aborted', 'AbortError'));
    };

    if (opts.signal) {
      if (opts.signal.aborted) {
        reject(new DOMException('Upload aborted', 'AbortError'));
        return;
      }
      opts.signal.addEventListener('abort', onAbort, { once: true });
    }

    xhr.upload.onprogress = (ev) => {
      if (ev.lengthComputable) {
        opts.onProgress?.(ev.loaded, ev.total);
      } else {
        opts.onProgress?.(ev.loaded, opts.file.size);
      }
    };

    xhr.onload = () => {
      opts.signal?.removeEventListener('abort', onAbort);
      if (xhr.status >= 200 && xhr.status < 300) {
        try {
          resolve(JSON.parse(xhr.responseText) as UploadResponse);
        } catch {
          reject(
            new ApiClientError({
              status: xhr.status,
              code: 'ERR_INVALID_RESPONSE',
              message: 'Invalid JSON response from server',
            }),
          );
        }
      } else {
        reject(parseXhrError(xhr));
      }
    };

    xhr.onerror = () => {
      opts.signal?.removeEventListener('abort', onAbort);
      reject(parseXhrError(xhr));
    };

    xhr.ontimeout = () => {
      opts.signal?.removeEventListener('abort', onAbort);
      reject(parseXhrError(xhr));
    };

    xhr.send(opts.file);
  });
}

export function putUploadPartXhr(opts: {
  uploadId: string;
  partNum: number;
  blob: Blob;
  sha256: string;
  onProgress?: (loaded: number, total: number) => void;
  signal?: AbortSignal;
}): Promise<void> {
  return new Promise((resolve, reject) => {
    const xhr = new XMLHttpRequest();
    xhr.open(
      'PUT',
      `/v1/uploads/${encodeURIComponent(opts.uploadId)}/parts/${opts.partNum}`,
      true,
    );

    const headers = authHeaders({
      'X-Content-SHA256': opts.sha256,
    });
    for (const [k, v] of Object.entries(headers)) {
      xhr.setRequestHeader(k, v);
    }

    const onAbort = () => {
      xhr.abort();
      reject(new DOMException('Upload aborted', 'AbortError'));
    };

    if (opts.signal) {
      if (opts.signal.aborted) {
        reject(new DOMException('Upload aborted', 'AbortError'));
        return;
      }
      opts.signal.addEventListener('abort', onAbort, { once: true });
    }

    xhr.upload.onprogress = (ev) => {
      opts.onProgress?.(ev.loaded, ev.lengthComputable ? ev.total : opts.blob.size);
    };

    xhr.onload = () => {
      opts.signal?.removeEventListener('abort', onAbort);
      if (xhr.status >= 200 && xhr.status < 300) {
        resolve();
      } else {
        reject(parseXhrError(xhr));
      }
    };

    xhr.onerror = () => {
      opts.signal?.removeEventListener('abort', onAbort);
      reject(parseXhrError(xhr));
    };

    xhr.ontimeout = () => {
      opts.signal?.removeEventListener('abort', onAbort);
      reject(parseXhrError(xhr));
    };

    xhr.send(opts.blob);
  });
}

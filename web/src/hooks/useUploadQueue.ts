import { useEffect, useRef, useState } from 'react';
import {
  ApiClientError,
  abortResumableUpload,
  completeResumableUpload,
  initResumableUpload,
  listAllFilesInFolder,
  probeHealth,
  putSingleFileXhr,
  putUploadPartXhr,
} from '../lib/api';
import { joinDrivePath, parentFolderPath } from '../lib/format';
import { hashFile, sha256Blob } from '../lib/sha256';
import type { FileItem, QuotaInfo } from '../types/api';

const SINGLE_SHOT_THRESHOLD = 8 * 1024 * 1024;
const BACKOFF_MS = [1000, 2000, 4000];

export type UploadPhase =
  | 'queued'
  | 'hashing'
  | 'uploading'
  | 'completing'
  | 'retrying'
  | 'done'
  | 'mismatch'
  | 'failed';

interface Checkpoint {
  id: string;
  partSize: number;
  confirmedBytes: number;
}

export interface UploadTask {
  id: string;
  file: File;
  targetPath: string;
  sizeBytes: number;
  growthBytes: number;
  uploadedBytes: number;
  progress: number;
  phase: UploadPhase;
  sha256: string | null;
  checkpoint: Checkpoint | null;
  errorMessage: string | null;
}

export interface QuotaBlockInfo {
  fileName: string;
  fileSize: number;
  code: 'ERR_QUOTA_EXCEEDED' | 'ERR_DISK_FLOOR';
  message: string;
  usedBytes: number;
  limitBytes: number;
  remainingBytes: number;
  diskFreeBytes: number | null;
  diskFloorBytes: number;
}

interface UseUploadQueueOptions {
  quota: QuotaInfo;
  onUploaded: (item: FileItem, quota: QuotaInfo) => void;
  onQuotaBlocked: (info: QuotaBlockInfo) => void;
  onQueueConflict: (path: string) => void;
  onChecksumMismatch: (fileName: string) => void;
  onApiError: (error: unknown) => void;
}

function retryDelay(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve, reject) => {
    signal.throwIfAborted();
    const onAbort = () => {
      clearTimeout(timer);
      reject(signal.reason);
    };
    const timer = window.setTimeout(() => {
      signal.removeEventListener('abort', onAbort);
      resolve();
    }, ms);
    signal.addEventListener('abort', onAbort, { once: true });
  });
}

function isRetryable(error: unknown) {
  return error instanceof ApiClientError && (error.isNetworkError || error.retryable);
}

function quotaNumber(value: unknown, fallback: number): number {
  return typeof value === 'number' && Number.isFinite(value) ? value : fallback;
}

function isActive(task: UploadTask) {
  return task.phase !== 'done' && task.phase !== 'failed' && task.phase !== 'mismatch';
}

async function discardCheckpoint(checkpoint: Checkpoint) {
  try {
    await abortResumableUpload(checkpoint.id);
  } catch (error) {
    if (!(error instanceof ApiClientError) || error.status !== 404) throw error;
  }
}

let nextTaskId = 1;

export function useUploadQueue(options: UseUploadQueueOptions) {
  const [tasks, setTasks] = useState<UploadTask[]>([]);
  const tasksRef = useRef<UploadTask[]>([]);
  const controllers = useRef(new Map<string, AbortController>());
  const optionsRef = useRef(options);
  optionsRef.current = options;

  useEffect(() => {
    const active = controllers.current;
    return () => {
      for (const controller of active.values()) controller.abort();
    };
  }, []);

  function replaceTasks(next: UploadTask[]) {
    tasksRef.current = next;
    setTasks(next);
  }

  function updateTask(id: string, patch: Partial<UploadTask>) {
    replaceTasks(tasksRef.current.map((task) => task.id === id ? { ...task, ...patch } : task));
  }

  async function runTask(task: UploadTask, rehash = false) {
    const { id, file, sizeBytes, targetPath } = task;
    if (controllers.current.has(id)) return;
    const controller = new AbortController();
    const { signal } = controller;
    controllers.current.set(id, controller);
    let checkpoint = task.checkpoint;

    function finish(response: { item: FileItem; quota: QuotaInfo }) {
      updateTask(id, { phase: 'done', uploadedBytes: sizeBytes, progress: 1, checkpoint: null });
      optionsRef.current.onUploaded(response.item, response.quota);
    }

    try {
      let sha256 = rehash ? null : task.sha256;
      if (!sha256) {
        if (checkpoint) checkpoint = { ...checkpoint, confirmedBytes: 0 };
        updateTask(id, { phase: 'hashing', uploadedBytes: 0, progress: 0, errorMessage: null, checkpoint });
        sha256 = await hashFile(file, (ratio) => updateTask(id, { progress: ratio * 0.15 }), signal);
        updateTask(id, { sha256, progress: 0.15 });
      }

      for (let retry = 0; ; retry++) {
        let completing = false;
        try {
          signal.throwIfAborted();
          updateTask(id, { phase: 'uploading', errorMessage: null });

          if (sizeBytes <= SINGLE_SHOT_THRESHOLD) {
            finish(await putSingleFileXhr({
              path: targetPath, file, sha256, signal,
              onProgress: (loaded) => updateTask(id, {
                uploadedBytes: loaded,
                progress: 0.15 + (sizeBytes ? loaded / sizeBytes : 1) * 0.85,
              }),
            }));
            return;
          }

          if (!checkpoint) {
            const init = await initResumableUpload({ path: targetPath, size_bytes: sizeBytes, mime: file.type || null });
            checkpoint = { id: init.upload_id, partSize: init.part_size, confirmedBytes: 0 };
            updateTask(id, { checkpoint, uploadedBytes: 0, progress: 0.15 });
          }

          const { partSize, id: uploadId } = checkpoint;
          if (!Number.isSafeInteger(partSize) || partSize <= 0) throw new Error('Server supplied an invalid part size.');
          while (checkpoint.confirmedBytes < sizeBytes) {
            signal.throwIfAborted();
            const start = checkpoint.confirmedBytes;
            const end = Math.min(start + partSize, sizeBytes);
            const blob = file.slice(start, end);
            const partHash = await sha256Blob(blob);
            await putUploadPartXhr({
              uploadId, partNum: start / partSize + 1, blob, sha256: partHash, signal,
              onProgress: (loaded) => updateTask(id, {
                uploadedBytes: start + loaded,
                progress: 0.15 + ((start + loaded) / sizeBytes) * 0.8,
              }),
            });
            checkpoint = { ...checkpoint, confirmedBytes: end };
            updateTask(id, { checkpoint, uploadedBytes: end, progress: 0.15 + (end / sizeBytes) * 0.8 });
          }

          signal.throwIfAborted();
          completing = true;
          updateTask(id, { phase: 'completing', progress: 0.96 });
          finish(await completeResumableUpload(uploadId, sha256));
          return;
        } catch (error) {
          signal.throwIfAborted();
          optionsRef.current.onApiError(error);
          const apiError = error instanceof ApiClientError ? error : null;
          if (apiError?.status === 422) {
            if (checkpoint) {
              try {
                await discardCheckpoint(checkpoint);
                checkpoint = null;
              } catch (cleanupError) {
                optionsRef.current.onApiError(cleanupError);
              }
            }
            updateTask(id, {
              phase: 'mismatch', checkpoint, sha256: null,
              errorMessage: 'Integrity mismatch. The upload was discarded.',
            });
            optionsRef.current.onChecksumMismatch(file.name);
            return;
          }
          if (apiError?.status === 507 || apiError?.status === 413) {
            const { quota } = optionsRef.current;
            const details = apiError.details;
            const code = apiError.code === 'ERR_DISK_FLOOR' ? 'ERR_DISK_FLOOR' : 'ERR_QUOTA_EXCEEDED';
            updateTask(id, { phase: 'failed', errorMessage: apiError.message });
            optionsRef.current.onQuotaBlocked({
              fileName: file.name, fileSize: sizeBytes, code, message: apiError.message,
              usedBytes: quotaNumber(details.used_bytes, quota.used_bytes),
              limitBytes: quotaNumber(details.limit_bytes, quota.limit_bytes),
              remainingBytes: quotaNumber(details.remaining_bytes, quota.remaining_bytes),
              diskFreeBytes: typeof details.disk_free_bytes === 'number' ? details.disk_free_bytes : quota.disk_free_bytes,
              diskFloorBytes: quotaNumber(details.disk_floor_bytes, quota.disk_floor_bytes),
            });
            return;
          }

          const missingSession = checkpoint !== null && apiError?.status === 404;
          if (missingSession) {
            checkpoint = null;
            updateTask(id, { checkpoint });
            // A completion may have committed before its response was lost.
            if (completing) {
              try {
                const items = await listAllFilesInFolder(parentFolderPath(targetPath), signal);
                const item = items.find((entry) => entry.path === targetPath && entry.sha256 === sha256 && entry.size_bytes === sizeBytes);
                if (item) {
                  const { quota } = await probeHealth(undefined, signal);
                  finish({ item, quota });
                  return;
                }
              } catch (lookupError) {
                if (!isRetryable(lookupError)) throw lookupError;
                optionsRef.current.onApiError(lookupError);
              }
            }
          }

          const delay = BACKOFF_MS[retry];
          if (delay === undefined || (!missingSession && !isRetryable(error))) throw error;
          updateTask(id, { phase: 'retrying', errorMessage: `Retrying (${retry + 1}/${BACKOFF_MS.length})…` });
          await retryDelay(delay, signal);
        }
      }
    } catch (error) {
      if (signal.aborted) {
        try {
          if (checkpoint) await discardCheckpoint(checkpoint);
          replaceTasks(tasksRef.current.filter((entry) => entry.id !== id));
          return;
        } catch (cleanupError) {
          error = cleanupError;
          optionsRef.current.onApiError(cleanupError);
        }
      }
      updateTask(id, {
        phase: 'failed', checkpoint,
        errorMessage: error instanceof Error ? error.message : 'Upload failed',
      });
    } finally {
      controllers.current.delete(id);
    }
  }

  function enqueueFiles(files: FileList, folder: string, existing: FileItem[]) {
    const { quota, onQuotaBlocked, onQueueConflict } = optionsRef.current;
    const toStart: UploadTask[] = [];
    const pending = tasksRef.current.filter(isActive);
    const reservedPaths = new Set(tasksRef.current.filter((task) => task.phase !== 'done').map((task) => task.targetPath));
    let pendingGrowth = pending.reduce((total, task) => total + task.growthBytes, 0);
    let pendingDisk = pending.reduce((total, task) => total + Math.max(0, task.sizeBytes - (task.checkpoint?.confirmedBytes ?? 0)), 0);

    for (const file of files) {
      const targetPath = joinDrivePath(folder, file.name);
      if (reservedPaths.has(targetPath)) {
        onQueueConflict(targetPath);
        continue;
      }
      const oldSize = existing.find((item) => item.path === targetPath)?.size_bytes ?? 0;
      const growthBytes = Math.max(0, file.size - oldSize);
      const diskFull = quota.disk_free_bytes !== null && quota.disk_free_bytes - pendingDisk - file.size < quota.disk_floor_bytes;
      if (diskFull || growthBytes > Math.max(0, quota.remaining_bytes - pendingGrowth)) {
        onQuotaBlocked({
          fileName: file.name, fileSize: file.size,
          code: diskFull ? 'ERR_DISK_FLOOR' : 'ERR_QUOTA_EXCEEDED',
          message: diskFull ? 'This upload would exceed the disk free-space floor.' : 'This upload exceeds the remaining storage quota.',
          usedBytes: quota.used_bytes, limitBytes: quota.limit_bytes,
          remainingBytes: quota.remaining_bytes, diskFreeBytes: quota.disk_free_bytes, diskFloorBytes: quota.disk_floor_bytes,
        });
        continue;
      }

      reservedPaths.add(targetPath);
      pendingGrowth += growthBytes;
      pendingDisk += file.size;
      toStart.push({
        id: String(nextTaskId++), file, targetPath, growthBytes,
        sizeBytes: file.size, phase: 'queued', progress: 0, uploadedBytes: 0,
        sha256: null, checkpoint: null, errorMessage: null,
      });
    }

    if (toStart.length === 0) return;
    replaceTasks([...toStart, ...tasksRef.current]);
    for (const task of toStart) void runTask(task);
  }

  async function dismissTask(id: string) {
    const task = tasksRef.current.find((entry) => entry.id === id);
    if (!task || controllers.current.has(id)) return;
    if (task.checkpoint) {
      const controller = new AbortController();
      controllers.current.set(id, controller);
      try {
        await discardCheckpoint(task.checkpoint);
      } catch (error) {
        optionsRef.current.onApiError(error);
        updateTask(id, { errorMessage: error instanceof Error ? error.message : 'Unable to discard upload' });
        return;
      } finally {
        controllers.current.delete(id);
      }
    }
    replaceTasks(tasksRef.current.filter((entry) => entry.id !== id));
  }

  return {
    tasks,
    enqueueFiles,
    retryTask: (id: string, rehash: boolean) => {
      const task = tasksRef.current.find((entry) => entry.id === id);
      if (task?.phase === 'failed' || task?.phase === 'mismatch') void runTask(task, rehash);
    },
    cancelTask: (id: string) => controllers.current.get(id)?.abort(),
    dismissTask: (id: string) => { void dismissTask(id); },
    clearFinished: () => replaceTasks(tasksRef.current.filter((entry) => entry.phase !== 'done')),
  };
}

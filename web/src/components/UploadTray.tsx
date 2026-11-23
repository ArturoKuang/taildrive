import { useState } from 'react';
import { ChevronDown, ChevronUp, RotateCcw, Upload, X } from 'lucide-react';
import type { UploadPhase, UploadTask } from '../hooks/useUploadQueue';
import { formatBytes } from '../lib/format';

const phaseLabel: Record<UploadPhase, string> = {
  queued: 'Queued',
  hashing: 'Hashing SHA-256…',
  uploading: 'Uploading…',
  completing: 'Verifying upload…',
  retrying: 'Retrying…',
  done: 'Uploaded',
  mismatch: 'Checksum mismatch',
  failed: 'Failed',
};

interface UploadTrayProps {
  tasks: UploadTask[];
  writable: boolean;
  onRetry: (id: string, rehash: boolean) => void;
  onCancel: (id: string) => void;
  onDismiss: (id: string) => void;
  onClear: () => void;
}

export function UploadTray({ tasks, writable, onRetry, onCancel, onDismiss, onClear }: UploadTrayProps) {
  const [expanded, setExpanded] = useState(true);
  if (!tasks.length) return null;

  return (
    <section className="upload-tray" aria-label="Upload Tray">
      <div className="upload-tray-header">
        <div className="upload-tray-title"><Upload size={14} /> Uploads ({tasks.length})</div>
        <div className="row-actions">
          {tasks.some((task) => task.phase === 'done') && <button className="btn btn-ghost btn-sm" onClick={onClear}>Clear completed</button>}
          <button
            className="icon-btn"
            aria-label={expanded ? 'Collapse upload tray' : 'Expand upload tray'}
            aria-expanded={expanded}
            onClick={() => setExpanded(!expanded)}
          >
            {expanded ? <ChevronDown size={15} /> : <ChevronUp size={15} />}
          </button>
        </div>
      </div>
      {expanded && (
        <div className="upload-tray-list">
          {tasks.map((task) => {
            const failed = task.phase === 'failed' || task.phase === 'mismatch';
            const done = task.phase === 'done';
            const retrying = task.phase === 'retrying';
            const percent = Math.round(task.progress * 100);
            return (
              <div className="upload-item" key={task.id}>
                <div className="upload-item-top">
                  <span className="upload-item-name" title={task.targetPath}>{task.file.name}</span>
                  <span className={`upload-badge ${failed ? 'danger' : retrying ? 'warn' : done ? 'ok' : ''}`} role="status">
                    {retrying ? task.errorMessage : phaseLabel[task.phase]}
                  </span>
                </div>
                <div className="upload-progress-track" role="progressbar" aria-label={`Upload ${task.file.name}`} aria-valuenow={percent}>
                  <div className={`upload-progress-bar ${failed ? 'danger' : retrying ? 'warn' : done ? 'done' : ''}`} style={{ width: `${percent}%` }} />
                </div>
                <div className="upload-item-meta tabular">
                  <span>{formatBytes(task.uploadedBytes)} / {formatBytes(task.sizeBytes)}</span>
                  <span>{percent}%</span>
                </div>
                {failed && <div className="upload-item-meta">{task.errorMessage}</div>}
                <div className="upload-item-actions">
                  {failed && (
                    <button className="btn btn-sm" disabled={!writable} onClick={() => onRetry(task.id, task.phase === 'mismatch')}>
                      <RotateCcw size={12} />{task.phase === 'mismatch' ? 'Re-hash & Retry' : 'Retry'}
                    </button>
                  )}
                  {done || failed ? (
                    <button className="btn btn-ghost btn-sm" onClick={() => onDismiss(task.id)}><X size={12} /> Dismiss</button>
                  ) : (
                    <button className="btn btn-ghost btn-sm" disabled={task.phase === 'completing'} onClick={() => onCancel(task.id)}><X size={12} /> Cancel</button>
                  )}
                </div>
              </div>
            );
          })}
        </div>
      )}
    </section>
  );
}

import { useEffect, useState } from 'react';
import { Copy, Download, File, FolderOpen, Link2, LoaderCircle, Trash2, X } from 'lucide-react';
import { fetchFileContent } from '../lib/api';
import { formatAbsoluteTime, formatExactBytes, getPreviewKind } from '../lib/format';
import type { FileItem } from '../types/api';

const TEXT_PREVIEW_BYTES = 128 * 1024;
const MEDIA_PREVIEW_BYTES = 32 * 1024 * 1024;

type Preview =
  | { kind: 'loading' }
  | { kind: 'message'; text: string }
  | { kind: 'text'; text: string; truncated: boolean }
  | { kind: 'image' | 'pdf'; url: string };

function FilePreview({ item, onApiError, online }: {
  item: FileItem;
  onApiError: (error: unknown) => void;
  online: boolean;
}) {
  const [preview, setPreview] = useState<Preview>({ kind: 'loading' });

  useEffect(() => {
    const kind = getPreviewKind(item.name, item.mime);
    if (!online || kind === 'none' || (kind !== 'text' && item.size_bytes > MEDIA_PREVIEW_BYTES)) {
      setPreview({ kind: 'message', text: !online ? 'Preview unavailable offline' : 'Preview unavailable for this file' });
      return;
    }
    if (item.size_bytes === 0) {
      setPreview({ kind: 'message', text: 'This file is empty' });
      return;
    }

    const controller = new AbortController();
    let objectUrl: string | undefined;
    const truncated = kind === 'text' && item.size_bytes > TEXT_PREVIEW_BYTES;
    setPreview({ kind: 'loading' });
    void (async () => {
      try {
        const { blob } = await fetchFileContent(item.path, {
          signal: controller.signal,
          range: truncated ? `bytes=0-${TEXT_PREVIEW_BYTES - 1}` : undefined,
        });
        if (controller.signal.aborted) return;
        if (kind === 'text') {
          const text = await blob.slice(0, TEXT_PREVIEW_BYTES).text();
          if (!controller.signal.aborted) setPreview({ kind, text, truncated });
        } else {
          const media = kind === 'pdf' ? blob.slice(0, blob.size, 'application/pdf') : blob;
          objectUrl = URL.createObjectURL(media);
          setPreview({ kind, url: objectUrl });
        }
      } catch (error) {
        if (controller.signal.aborted) return;
        onApiError(error);
        setPreview({ kind: 'message', text: error instanceof Error ? error.message : 'Unable to load preview' });
      }
    })();

    return () => {
      controller.abort();
      if (objectUrl) URL.revokeObjectURL(objectUrl);
    };
  }, [item, onApiError, online]);

  return (
    <div className="preview-box">
      {preview.kind === 'loading' && <div className="preview-placeholder"><LoaderCircle className="spin" size={18} /> Loading preview…</div>}
      {preview.kind === 'message' && <div className="preview-placeholder"><File size={20} />{preview.text}</div>}
      {preview.kind === 'image' && (
        <div className="preview-image-wrap">
          <img src={preview.url} alt={item.name} onError={() => setPreview({ kind: 'message', text: 'Unable to display this image' })} />
        </div>
      )}
      {preview.kind === 'pdf' && (
        <object className="preview-pdf" data={preview.url} type="application/pdf" aria-label={`Preview of ${item.name}`}>
          <div className="preview-placeholder">Download this PDF to view it.</div>
        </object>
      )}
      {preview.kind === 'text' && (
        <>
          <pre className="preview-code">{preview.text}</pre>
          {preview.truncated && <div className="preview-truncated-bar">Showing the first 128 KB</div>}
        </>
      )}
    </div>
  );
}

interface InspectorProps {
  item: FileItem;
  online: boolean;
  writable: boolean;
  onClose: () => void;
  onOpen: (item: FileItem) => void;
  onDownload: (item: FileItem) => void;
  onShare: (item: FileItem) => void;
  onDelete: (item: FileItem) => void;
  onCopy: (value: string, label: string) => void;
  onApiError: (error: unknown) => void;
}

export function Inspector({
  item, online, writable, onClose, onOpen, onDownload, onShare, onDelete, onCopy, onApiError,
}: InspectorProps) {
  const folder = item.type === 'folder';
  const fingerprint = item.sha256;

  return (
    <aside className="inspector" aria-label="Inspector">
      <div className="inspector-header">
        <div className="inspector-title-wrap">
          <h2 className="inspector-title">{item.name}</h2>
          <div className="inspector-subtitle" title={item.path}>{item.path}</div>
        </div>
        <button className="icon-btn" aria-label="Close inspector" onClick={onClose}><X size={16} /></button>
      </div>
      <div className="inspector-body">
        {!folder && <FilePreview key={item.version} item={item} online={online} onApiError={onApiError} />}
        <div className="inspector-actions">
          {folder ? (
            <button className="btn full-span" onClick={() => onOpen(item)}><FolderOpen size={14} /> Open Folder</button>
          ) : (
            <>
              <button className="btn" disabled={!online} onClick={() => onDownload(item)}><Download size={14} /> Download</button>
              <button className="btn" disabled={!writable} onClick={() => onShare(item)}><Link2 size={14} /> Copy Share Link</button>
            </>
          )}
          <button className="btn btn-danger-ghost full-span" disabled={!writable} onClick={() => onDelete(item)}><Trash2 size={14} /> Delete</button>
        </div>
        <section className="meta-section" aria-label="File details">
          <div className="meta-section-title">Details</div>
          <dl className="meta-list inspector-details">
            <div className="meta-row"><dt className="meta-key">Type</dt><dd className="meta-val">{folder ? 'Folder' : item.mime ?? 'File'}</dd></div>
            {!folder && <div className="meta-row"><dt className="meta-key">Size</dt><dd className="meta-val tabular">{formatExactBytes(item.size_bytes)}</dd></div>}
            <div className="meta-row"><dt className="meta-key">Modified</dt><dd className="meta-val tabular">{formatAbsoluteTime(item.updated_at)}</dd></div>
            {!folder && <div className="meta-row"><dt className="meta-key">Version</dt><dd className="meta-val tabular">{item.version}</dd></div>}
            {fingerprint && (
              <div className="meta-row">
                <dt className="meta-key">
                  SHA-256
                  <button className="icon-btn" aria-label="Copy SHA-256" onClick={() => onCopy(fingerprint, 'SHA-256 copied')}><Copy size={13} /></button>
                </dt>
                <dd className="sha-box">{fingerprint}</dd>
              </div>
            )}
          </dl>
        </section>
      </div>
    </aside>
  );
}

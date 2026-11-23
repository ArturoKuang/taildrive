import { useEffect, useId, useRef, useState, type FormEvent, type ReactNode } from 'react';
import { Copy, X } from 'lucide-react';

export interface DialogResult {
  url: string;
}

export interface DialogDetails {
  title: string;
  submitLabel?: string;
  danger?: boolean;
  content: (disabled: boolean) => ReactNode;
  submit?: (data: FormData) => Promise<DialogResult | void>;
}

interface WorkspaceDialogProps {
  details: DialogDetails;
  writable: boolean;
  onClose: () => void;
  onApiError: (error: unknown) => void;
  onCopy: (value: string, label: string) => Promise<boolean>;
}

export function WorkspaceDialog({ details, writable, onClose, onApiError, onCopy }: WorkspaceDialogProps) {
  const ref = useRef<HTMLDialogElement>(null);
  const titleId = useId();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [result, setResult] = useState<DialogResult | null>(null);
  const [copied, setCopied] = useState(false);
  const disabled = busy || !writable;

  useEffect(() => {
    ref.current?.showModal();
  }, []);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!details.submit || disabled) return;
    const data = new FormData(event.currentTarget);
    setBusy(true);
    setError(null);
    try {
      const response = await details.submit(data);
      if (response) setResult(response);
      else onClose();
    } catch (error) {
      setError(error instanceof Error ? error.message : 'Request failed');
      onApiError(error);
    } finally {
      setBusy(false);
    }
  }

  return (
    <dialog
      ref={ref}
      className="modal-card workspace-dialog"
      aria-labelledby={titleId}
      onCancel={(event) => { event.preventDefault(); if (!busy) onClose(); }}
    >
      <form onSubmit={(event) => { void submit(event); }}>
        <div className="modal-header">
          <h2 className="modal-title" id={titleId}>{result ? 'Share link created' : details.title}</h2>
          <button className="icon-btn" type="button" aria-label="Close dialog" disabled={busy} onClick={onClose}><X size={16} /></button>
        </div>
        <div className="modal-body">
          {result ? (
            <div className="share-result-box">
              <input className="text-input share-result-url" aria-label="Share URL" readOnly value={result.url} onFocus={(event) => event.target.select()} />
              <button className="btn" type="button" onClick={async () => {
                const success = await onCopy(result.url, 'Share link copied');
                setCopied(success);
                setError(success ? null : 'Select the URL to copy it manually.');
              }}><Copy size={13} /> {copied ? 'Copied' : 'Copy Link'}</button>
            </div>
          ) : details.content(disabled)}
          {error && <div className="form-error" role="alert">{error}</div>}
        </div>
        <div className="modal-footer">
          <button className="btn" type="button" disabled={busy} onClick={onClose}>{result || !details.submit ? 'Close' : 'Cancel'}</button>
          {!result && details.submit && (
            <button className={`btn ${details.danger ? 'btn-danger' : 'btn-primary'}`} type="submit" disabled={disabled}>
              {busy ? 'Working…' : details.submitLabel}
            </button>
          )}
        </div>
      </form>
    </dialog>
  );
}

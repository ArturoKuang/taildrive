import { Copy, Link2, RefreshCw, Trash2 } from 'lucide-react';
import { formatAbsoluteTime, formatExpiryRemaining } from '../lib/format';
import type { ShareItem } from '../types/api';

interface SharedLinksProps {
  items: ShareItem[] | null;
  loading: boolean;
  error: string | null;
  writable: boolean;
  onCreate: () => void;
  onCopy: (share: ShareItem) => void;
  onRevoke: (share: ShareItem) => void;
  onRefresh: () => void;
}

export function SharedLinks({
  items, loading, error, writable, onCreate, onCopy, onRevoke, onRefresh,
}: SharedLinksProps) {
  return (
    <div className="page-scroll">
      <div className="page-container">
        <div className="page-head">
          <p>Create and manage expiring links to files in My Drive.</p>
          <div className="row-actions">
            <button className="btn" disabled={!writable || loading} onClick={onRefresh} aria-label="Refresh shared links"><RefreshCw size={14} /></button>
            <button className="btn btn-primary" disabled={!writable} onClick={onCreate}><Link2 size={14} /> Create Link</button>
          </div>
        </div>
        {error && <div className="form-error" role="alert">{error}</div>}
        {!items?.length ? (
          <div className="empty-state" role="status">
            <div className="empty-state-icon"><Link2 size={19} /></div>
            <h3>{loading ? 'Loading links…' : items ? 'No active share links' : 'Shared links unavailable'}</h3>
          </div>
        ) : (
          <div className="panel-card shares-scroll" aria-busy={loading}>
            <table className="shares-table" aria-label="Shared links">
              <thead><tr><th scope="col">Path</th><th scope="col">Created</th><th scope="col">Expires</th><th scope="col">Actions</th></tr></thead>
              <tbody>
                {items.map((share) => {
                  const expiry = formatExpiryRemaining(share.expires_at);
                  return (
                    <tr key={share.id}>
                      <td className="mono share-path">{share.path}</td>
                      <td className="tabular">{formatAbsoluteTime(share.created_at)}</td>
                      <td className="tabular" title={formatAbsoluteTime(share.expires_at)}><span className={`pill ${expiry.expired ? 'neutral' : 'ok'}`}>{expiry.label}</span></td>
                      <td>
                        <div className="row-actions">
                          <button className="btn btn-sm" disabled={expiry.expired} onClick={() => onCopy(share)}><Copy size={12} /> Copy Link</button>
                          <button className="btn btn-sm btn-danger-ghost" disabled={!writable} onClick={() => onRevoke(share)}><Trash2 size={12} /> Revoke</button>
                        </div>
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </div>
    </div>
  );
}

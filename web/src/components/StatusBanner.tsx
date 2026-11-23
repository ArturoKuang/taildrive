import { RefreshCw, WifiOff } from 'lucide-react';
import type { ConnectionState } from '../types/api';

interface StatusBannerProps {
  connection: ConnectionState;
  nodeName: string;
  retrying: boolean;
  onRetry: () => void;
}

export function StatusBanner({
  connection,
  nodeName,
  retrying,
  onRetry,
}: StatusBannerProps) {
  if (connection === 'connected') return null;

  const isOffline = connection === 'offline';

  return (
    <div className={`status-banner ${connection}`} role="status">
      <div className="status-banner-left">
        <WifiOff size={14} style={{ flexShrink: 0 }} />
        <span>
          {isOffline ? (
            <>
              <strong>Offline</strong> · Browsing cached files
            </>
          ) : (
            <>
              <strong>Tailscale disconnected</strong> · Connect to reach{' '}
              <span className="mono">{nodeName}</span>.
            </>
          )}
        </span>
      </div>
      <button
        type="button"
        className="btn btn-sm"
        onClick={onRetry}
        disabled={retrying}
      >
        <RefreshCw size={12} className={retrying ? 'spin' : undefined} />
        <span>{retrying ? 'Probing…' : 'Retry'}</span>
      </button>
    </div>
  );
}

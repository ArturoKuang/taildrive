import { RefreshCw } from 'lucide-react';
import { formatBytes } from '../lib/format';
import type { HealthResponse } from '../types/api';

export function StorageStatus({ health, probing, onRefresh }: {
  health: HealthResponse;
  probing: boolean;
  onRefresh: () => void;
}) {
  const { quota } = health;
  return (
    <div className="page-scroll">
      <div className="page-container">
        <div className="page-head">
          <p>{health.node} · {health.storage_driver}</p>
          <button className="btn" disabled={probing} onClick={onRefresh}><RefreshCw size={14} className={probing ? 'spin' : undefined} /> Refresh</button>
        </div>
        <div className="stat-grid">
          <div className="stat-card">
            <div className="stat-label">Drive storage used</div>
            <div className="stat-value">{formatBytes(quota.used_bytes)}</div>
            <div className="stat-sub">{formatBytes(quota.limit_bytes)} limit</div>
          </div>
          <div className="stat-card">
            <div className="stat-label">Quota remaining</div>
            <div className="stat-value">{formatBytes(quota.remaining_bytes)}</div>
          </div>
          {quota.disk_free_bytes !== null && (
            <div className="stat-card">
              <div className="stat-label">Free disk space</div>
              <div className="stat-value">{formatBytes(quota.disk_free_bytes)}</div>
              <div className="stat-sub">{formatBytes(quota.disk_floor_bytes)} free-space floor</div>
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

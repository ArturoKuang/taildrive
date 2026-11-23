import {
  Activity,
  FolderClosed,
  HardDrive,
  KeyRound,
  Link2,
  LogOut,
  Moon,
  Sun,
  User,
} from 'lucide-react';
import { formatBytes, formatGigabytes } from '../lib/format';
import type { ConnectionState, HealthResponse, NavTab } from '../types/api';

interface SidebarProps {
  health: HealthResponse | null;
  connection: ConnectionState;
  activeTab: NavTab;
  shareCount: number | null;
  theme: 'light' | 'dark';
  hasBearerToken: boolean;
  logoutDisabled: boolean;
  mobileOpen: boolean;
  onCloseMobile: () => void;
  onSelectTab: (tab: NavTab) => void;
  onToggleTheme: () => void;
  onLogout: () => void;
}

export function Sidebar({
  health,
  connection,
  activeTab,
  shareCount,
  theme,
  hasBearerToken,
  logoutDisabled,
  mobileOpen,
  onCloseMobile,
  onSelectTab,
  onToggleTheme,
  onLogout,
}: SidebarProps) {
  const quota = health?.quota ?? null;
  const usedPct = quota && quota.limit_bytes > 0
    ? Math.min(100, (quota.used_bytes / quota.limit_bytes) * 100)
    : 0;
  const quotaStatus = quota?.status ?? 'ok';

  const nodeLabel = health?.node ?? 'macmini-ci';
  const driverLabel = health?.storage_driver ?? 'local-fs';

  const statusDotClass =
    connection === 'connected'
      ? 'live'
      : connection === 'offline'
        ? 'offline'
        : 'disconnected';

  const handleNav = (tab: NavTab) => {
    onSelectTab(tab);
    onCloseMobile();
  };

  return (
    <>
      {mobileOpen && (
        <div
          className="rail-mobile-backdrop"
          onClick={onCloseMobile}
          aria-hidden="true"
        />
      )}
      <aside className={`rail${mobileOpen ? ' open' : ''}`} aria-label="Primary navigation">
        <div className="rail-header">
          <div className="brand">
            <span className="brand-mark">
              <HardDrive size={13} strokeWidth={2} />
            </span>
            <span>TailDrive</span>
          </div>
          <button
            type="button"
            className="icon-btn"
            onClick={onToggleTheme}
            title={theme === 'dark' ? 'Switch to light mode' : 'Switch to dark mode'}
            aria-label="Toggle color theme"
          >
            {theme === 'dark' ? <Sun size={15} /> : <Moon size={15} />}
          </button>
        </div>

        <div className="node-block">
          <div className="node-row">
            <span
              className="node-pill"
              title={`Node: ${nodeLabel} (${connection})`}
            >
              <span className={`status-dot ${statusDotClass}`} />
              <span style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>{nodeLabel}</span>
            </span>
            <span className="driver-badge" title={`Storage driver: ${driverLabel}`}>
              {driverLabel}
            </span>
          </div>
          <div className="identity-line" title={health?.tailnet_user ?? 'Bearer token session'}>
            {health?.tailnet_user ? (
              <>
                <User size={12} style={{ flexShrink: 0 }} />
                <span className="mono" style={{ overflow: 'hidden', textOverflow: 'ellipsis' }}>
                  {health.tailnet_user}
                </span>
              </>
            ) : (
              <>
                <KeyRound size={12} style={{ flexShrink: 0 }} />
                <span>Bearer Token Session</span>
              </>
            )}
          </div>
        </div>

        <nav className="rail-nav">
          <button
            type="button"
            className={`nav-item${activeTab === 'drive' ? ' active' : ''}`}
            onClick={() => handleNav('drive')}
          >
            <span className="nav-item-left">
              <FolderClosed size={15} />
              <span>My Drive</span>
            </span>
          </button>

          <button
            type="button"
            className={`nav-item${activeTab === 'shares' ? ' active' : ''}`}
            onClick={() => handleNav('shares')}
          >
            <span className="nav-item-left">
              <Link2 size={15} />
              <span>Shared Links</span>
            </span>
            {shareCount !== null && shareCount > 0 && (
              <span className="nav-count tabular">{shareCount}</span>
            )}
          </button>

          <button
            type="button"
            className={`nav-item${activeTab === 'status' ? ' active' : ''}`}
            onClick={() => handleNav('status')}
          >
            <span className="nav-item-left">
              <Activity size={15} />
              <span>Storage Status</span>
            </span>
          </button>
        </nav>

        <div className="rail-footer">
          <div className="quota-card">
            <div className="quota-header">
              <span className="quota-label">Quota</span>
              <span className="quota-numbers tabular">
                {quota
                  ? `${formatGigabytes(quota.used_bytes)} / ${formatGigabytes(quota.limit_bytes)} GB`
                  : '— / 5.00 GB'}
              </span>
            </div>

            <div className="quota-track" role="progressbar" aria-label="Storage quota used" aria-valuenow={Math.round(usedPct)} aria-valuemin={0} aria-valuemax={100}>
              <div
                className={`quota-fill ${quotaStatus}`}
                style={{ width: `${Math.max(usedPct, usedPct > 0 ? 2 : 0)}%` }}
              />
            </div>

            <div className={`quota-sub tabular${quotaStatus === 'full' ? ' danger' : ''}`}>
              <span>
                {quota ? `${formatBytes(quota.remaining_bytes)} free` : 'Checking…'}
              </span>
              <span>{quota ? `${usedPct.toFixed(0)}%` : '—'}</span>
            </div>

            {quota && (
              <div className="quota-sub tabular" style={{ fontSize: 10 }}>
                <span>Disk floor: {formatGigabytes(quota.disk_floor_bytes)} GB</span>
                <span>
                  {quota.disk_free_bytes !== null
                    ? `${formatBytes(quota.disk_free_bytes)} SSD`
                    : driverLabel}
                </span>
              </div>
            )}
          </div>

          <div className="rail-actions">
            <span className="mono" style={{ fontSize: 10, color: 'var(--text-muted)' }}>
              {connection === 'connected' ? 'tailnet live' : connection}
            </span>
            {hasBearerToken && (
              <button
                type="button"
                className="btn btn-ghost btn-sm"
                onClick={onLogout}
                disabled={logoutDisabled}
                title={logoutDisabled ? 'Wait for active uploads to finish before signing out' : 'Sign out'}
              >
                <LogOut size={12} />
                <span>Log out</span>
              </button>
            )}
          </div>
        </div>
      </aside>
    </>
  );
}

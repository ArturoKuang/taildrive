import { type RefObject } from 'react';
import {
  ChevronRight,
  FolderPlus,
  HardDrive,
  Menu,
  Search,
  Upload,
  X,
} from 'lucide-react';
import { splitBreadcrumbs } from '../lib/format';
import type { NavTab } from '../types/api';

interface TopbarProps {
  activeTab: NavTab;
  currentPath: string;
  searchQuery: string;
  searchInputRef: RefObject<HTMLInputElement | null>;
  disabledMutations: boolean;
  onOpenMobileMenu: () => void;
  onNavigateFolder: (path: string) => void;
  onSearchChange: (q: string) => void;
  onOpenNewFolder: () => void;
  onTriggerUpload: () => void;
}

export function Topbar({
  activeTab,
  currentPath,
  searchQuery,
  searchInputRef,
  disabledMutations,
  onOpenMobileMenu,
  onNavigateFolder,
  onSearchChange,
  onOpenNewFolder,
  onTriggerUpload,
}: TopbarProps) {
  const crumbs = splitBreadcrumbs(currentPath);

  return (
    <header className="topbar">
      <div className="topbar-left">
        <button
          type="button"
          className="icon-btn mobile-menu-btn"
          onClick={onOpenMobileMenu}
          aria-label="Open navigation menu"
        >
          <Menu size={16} />
        </button>

        {activeTab === 'drive' ? (
          <nav className="breadcrumbs" aria-label="Folder breadcrumbs">
            <button
              type="button"
              className={`crumb-btn${crumbs.length === 0 ? ' current' : ''}`}
              onClick={() => onNavigateFolder('')}
            >
              <HardDrive size={13} />
              <span>My Drive</span>
            </button>
            {crumbs.map((crumb, index) => {
              const isLast = index === crumbs.length - 1;
              return (
                <span
                  key={crumb.path}
                  style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}
                >
                  <ChevronRight size={13} className="crumb-sep" />
                  <button
                    type="button"
                    className={`crumb-btn mono${isLast ? ' current' : ''}`}
                    onClick={() => onNavigateFolder(crumb.path)}
                  >
                    {crumb.label}
                  </button>
                </span>
              );
            })}
          </nav>
        ) : activeTab === 'shares' ? (
          <h1 className="view-title">Shared Links</h1>
        ) : (
          <h1 className="view-title">Storage Status</h1>
        )}
      </div>

      {activeTab === 'drive' && (
        <div className="topbar-right">
          <div className="search-wrap">
            <Search size={13} className="search-icon" />
            <input
              ref={searchInputRef}
              type="text"
              className="search-input"
              placeholder="Filter files…"
              value={searchQuery}
              onChange={(e) => onSearchChange(e.target.value)}
              aria-label="Search current folder"
            />
            {searchQuery ? (
              <button
                type="button"
                className="search-clear"
                onClick={() => onSearchChange('')}
                aria-label="Clear filter"
              >
                <X size={12} />
              </button>
            ) : (
              <span className="search-kbd">⌘K</span>
            )}
          </div>

          <button
            type="button"
            className="btn"
            onClick={onOpenNewFolder}
            disabled={disabledMutations}
            title="New Folder (⌘⇧N)"
          >
            <FolderPlus size={14} />
            <span>New Folder</span>
          </button>

          <button
            type="button"
            className="btn btn-primary"
            onClick={onTriggerUpload}
            disabled={disabledMutations}
          >
            <Upload size={14} />
            <span>Upload</span>
          </button>
        </div>
      )}
    </header>
  );
}

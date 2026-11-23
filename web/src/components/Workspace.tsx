import { useEffect, useMemo, useRef, useState, type DragEvent } from 'react';
import { Upload, X } from 'lucide-react';
import { useUploadQueue, type QuotaBlockInfo } from '../hooks/useUploadQueue';
import {
  createFolder, createShare, deleteFile, fetchFileContent, fileContentUrl,
  listAllFilesInFolder, listShares, revokeShare,
} from '../lib/api';
import { copyTextToClipboard, formatBytes, formatGigabytes, joinDrivePath, parentFolderPath } from '../lib/format';
import type { ConnectionState, FileItem, HealthResponse, NavTab, QuotaInfo, ShareItem, SortField } from '../types/api';
import { FileTable, type Sort } from './FileTable';
import { Inspector } from './Inspector';
import { SharedLinks } from './SharedLinks';
import { Sidebar } from './Sidebar';
import { StatusBanner } from './StatusBanner';
import { StorageStatus } from './StorageStatus';
import { Topbar } from './Topbar';
import { UploadTray } from './UploadTray';
import { WorkspaceDialog, type DialogDetails } from './WorkspaceDialog';

const nameOrder = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

interface WorkspaceProps {
  health: HealthResponse;
  hasToken: boolean;
  connection: ConnectionState;
  probing: boolean;
  onReconnect: () => void;
  onApiError: (error: unknown) => void;
  onLogout: () => void;
  onQuota: (quota: QuotaInfo) => void;
  theme: 'light' | 'dark';
  onToggleTheme: () => void;
}

export function Workspace({
  health, hasToken, connection, probing, onReconnect, onApiError, onLogout, onQuota,
  theme, onToggleTheme,
}: WorkspaceProps) {
  const [tab, setTab] = useState<NavTab>('drive');
  const [path, setPath] = useState('');
  const [search, setSearch] = useState('');
  const [sort, setSort] = useState<Sort>({ field: 'name', direction: 'asc' });
  const [selection, setSelection] = useState<string | null>(null);
  const [folders, setFolders] = useState<Map<string, FileItem[]>>(() => new Map());
  const [folderError, setFolderError] = useState<string | null>(null);
  const [loadingFolder, setLoadingFolder] = useState(true);
  const [folderRefresh, setFolderRefresh] = useState(0);
  const [shares, setShares] = useState<ShareItem[] | null>(null);
  const [sharesError, setSharesError] = useState<string | null>(null);
  const [loadingShares, setLoadingShares] = useState(false);
  const [sharesRefresh, setSharesRefresh] = useState(0);
  const [mobileOpen, setMobileOpen] = useState(false);
  const [dragging, setDragging] = useState(false);
  const [dialog, setDialog] = useState<DialogDetails | null>(null);
  const [notice, setNotice] = useState<{ text: string; tone: 'ok' | 'danger' } | null>(null);
  const searchInput = useRef<HTMLInputElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const tablePane = useRef<HTMLElement>(null);
  const dragDepth = useRef(0);

  const online = connection === 'connected';
  const writable = online;
  const cachedItems = folders.get(path);
  const items = useMemo(() => {
    const query = search.trim().toLocaleLowerCase();
    return (cachedItems ?? []).filter((item) => item.name.toLocaleLowerCase().includes(query)).sort((a, b) => {
      if (sort.field === 'name' && a.type !== b.type) return a.type === 'folder' ? -1 : 1;
      const order = sort.field === 'size' ? a.size_bytes - b.size_bytes
        : sort.field === 'modified' ? Date.parse(a.updated_at) - Date.parse(b.updated_at)
        : nameOrder.compare(a.name, b.name);
      return (sort.direction === 'asc' ? order : -order) || nameOrder.compare(a.name, b.name);
    });
  }, [cachedItems, search, sort]);
  const selected = items.find((item) => item.path === selection);

  const refreshFolder = () => setFolderRefresh((value) => value + 1);
  const refreshShares = () => setSharesRefresh((value) => value + 1);

  useEffect(() => {
    if (!online || tab !== 'drive') return;
    const controller = new AbortController();
    setLoadingFolder(true);
    setFolderError(null);

    void listAllFilesInFolder(path, controller.signal).then((result) => {
      if (!controller.signal.aborted) setFolders((current) => new Map(current).set(path, result));
    }).catch((error: unknown) => {
      if (controller.signal.aborted) return;
      setFolderError(error instanceof Error ? error.message : 'Unable to load files');
      onApiError(error);
    }).finally(() => {
      if (!controller.signal.aborted) setLoadingFolder(false);
    });
    return () => controller.abort();
  }, [path, tab, online, folderRefresh, onApiError]);

  useEffect(() => {
    if (!online) return;
    const controller = new AbortController();
    setLoadingShares(true);
    setSharesError(null);
    void listShares(controller.signal).then(({ items: result }) => {
      if (!controller.signal.aborted) setShares(result);
    }).catch((error: unknown) => {
      if (controller.signal.aborted) return;
      setSharesError(error instanceof Error ? error.message : 'Unable to load links');
      onApiError(error);
    }).finally(() => {
      if (!controller.signal.aborted) setLoadingShares(false);
    });
    return () => controller.abort();
  }, [tab, online, sharesRefresh, onApiError]);

  useEffect(() => {
    if (!notice) return;
    const timer = window.setTimeout(() => setNotice(null), 5000);
    return () => window.clearTimeout(timer);
  }, [notice]);

  useEffect(() => {
    const preventBrowserDrop = (event: globalThis.DragEvent) => {
      if (event.dataTransfer?.types.includes('Files')) event.preventDefault();
    };
    window.addEventListener('dragover', preventBrowserDrop);
    window.addEventListener('drop', preventBrowserDrop);
    return () => {
      window.removeEventListener('dragover', preventBrowserDrop);
      window.removeEventListener('drop', preventBrowserDrop);
    };
  }, []);

  function reportError(error: unknown) {
    onApiError(error);
    setNotice({ text: error instanceof Error ? error.message : 'Request failed', tone: 'danger' });
  }

  async function copy(value: string, label: string) {
    try {
      await copyTextToClipboard(value);
      setNotice({ text: label, tone: 'ok' });
      return true;
    } catch {
      setNotice({ text: 'Unable to copy to the clipboard.', tone: 'danger' });
      return false;
    }
  }

  function upsertItem(item: FileItem) {
    const parent = parentFolderPath(item.path);
    setFolders((current) => {
      const before = current.get(parent);
      return before ? new Map(current).set(parent, [...before.filter(({ path }) => path !== item.path), item]) : current;
    });
    refreshFolder();
  }

  function showQuotaBlock(info: QuotaBlockInfo) {
    const diskFloor = info.code === 'ERR_DISK_FLOOR';
    const details: DialogDetails = {
      title: diskFloor ? 'Disk free-space limit reached' : 'Storage limit reached',
      content: () => (
        <>
          <div>{info.message}</div>
          <div><strong>{info.fileName}</strong> · {formatBytes(info.fileSize)}</div>
          <div className="tabular">{formatGigabytes(info.usedBytes)} / {formatGigabytes(info.limitBytes)} GB used · {formatBytes(info.remainingBytes)} available</div>
          {diskFloor && info.diskFreeBytes !== null && (
            <div className="tabular">{formatBytes(info.diskFreeBytes)} disk free · {formatBytes(info.diskFloorBytes)} floor</div>
          )}
          <button className="btn" type="button" onClick={() => {
            setSort({ field: 'size', direction: 'desc' });
            setTab('drive');
            setDialog(null);
          }}>Sort by Largest</button>
        </>
      ),
    };
    setDialog((current) => current ?? details);
  }

  const queue = useUploadQueue({
    quota: health.quota,
    onUploaded: (item, quota) => {
      onQuota(quota);
      upsertItem(item);
    },
    onQuotaBlocked: showQuotaBlock,
    onQueueConflict: (path) => setNotice({ text: `An upload for ${path} is already in the tray.`, tone: 'danger' }),
    onChecksumMismatch: (name) => setNotice({ text: `Integrity mismatch for ${name}. The upload was discarded.`, tone: 'danger' }),
    onApiError,
  });
  const transfersPending = queue.tasks.some(({ phase }) => phase !== 'done' && phase !== 'failed' && phase !== 'mismatch');

  function navigate(folder: string) {
    if (folder === path) return;
    setPath(folder);
    setSearch('');
    setSelection(null);
    setFolderError(null);
    setLoadingFolder(online);
  }

  function openItem(item: FileItem) {
    if (item.type === 'folder') navigate(item.path);
    else setSelection(item.path);
  }

  function openNewFolder() {
    if (!writable) return;
    setDialog({
      title: 'New Folder',
      submitLabel: 'Create Folder',
      content: (disabled) => (
        <label className="field">
          <span className="field-label">Folder name</span>
          <input className="text-input" name="name" placeholder="Untitled folder" disabled={disabled} required autoFocus />
        </label>
      ),
      submit: async (data) => {
        const name = String(data.get('name') ?? '').trim();
        if (!name || name === '.' || name === '..' || /[/\\]/.test(name)) {
          throw new Error('Enter a single folder name.');
        }
        const item = await createFolder(joinDrivePath(path, name));
        upsertItem(item);
        setNotice({ text: `Created ${item.name}`, tone: 'ok' });
      },
    });
  }

  function openDelete(item: FileItem) {
    if (!writable) return;
    const folder = item.type === 'folder';
    setDialog({
      title: folder ? 'Delete folder?' : 'Delete file?',
      submitLabel: 'Delete',
      danger: true,
      content: () => <div><strong>{item.name}</strong> {folder ? 'and all its contents will be permanently deleted.' : 'will be permanently deleted.'}</div>,
      submit: async () => {
        const { quota } = await deleteFile(item.path, folder);
        onQuota(quota);
        const parent = parentFolderPath(item.path);
        setFolders((current) => {
          const next = new Map(current);
          const siblings = next.get(parent);
          if (siblings) next.set(parent, siblings.filter((entry) => entry.path !== item.path));
          for (const key of next.keys()) {
            if (key === item.path || key.startsWith(`${item.path}/`)) next.delete(key);
          }
          return next;
        });
        setShares((current) => current?.filter((share) =>
          share.path !== item.path && !share.path.startsWith(`${item.path}/`)) ?? null);
        setSelection(null);
        refreshFolder();
        setNotice({ text: `Deleted ${item.name}`, tone: 'ok' });
      },
    });
  }

  function openCreateLink(item?: FileItem) {
    if (!writable) return;
    setDialog({
      title: 'Create Share Link',
      submitLabel: 'Create Link',
      content: (disabled) => (
        <>
          <label className="field">
            <span className="field-label">File path</span>
            <input className="text-input mono" name="path" defaultValue={item?.path} placeholder="folder/file.pdf" readOnly={Boolean(item)} disabled={disabled} required autoFocus={!item} />
          </label>
          <label className="field">
            <span className="field-label">Link expires in</span>
            <select className="text-input" name="expiry" defaultValue="86400" disabled={disabled} autoFocus={Boolean(item)}>
              <option value="3600">1 hour</option>
              <option value="86400">1 day</option>
              <option value="604800">7 days</option>
              <option value="2592000">30 days</option>
            </select>
          </label>
        </>
      ),
      submit: async (data) => {
        const path = String(data.get('path') ?? '');
        const share = await createShare({ path, expires_in_seconds: Number(data.get('expiry')) });
        setShares((current) => [share, ...(current ?? [])]);
        refreshShares();
        return { url: new URL(share.url, window.location.origin).href };
      },
    });
  }

  function openRevoke(share: ShareItem) {
    if (!writable) return;
    setDialog({
      title: 'Revoke share link?',
      submitLabel: 'Revoke Link',
      danger: true,
      content: () => <div>Anyone with this link to <strong>{share.path}</strong> will lose access immediately.</div>,
      submit: async () => {
        await revokeShare(share.id);
        setShares((current) => current?.filter(({ id }) => id !== share.id) ?? null);
        refreshShares();
        setNotice({ text: 'Share link revoked', tone: 'ok' });
      },
    });
  }

  async function download(item: FileItem) {
    if (!online) return;
    try {
      const url = hasToken
        ? URL.createObjectURL((await fetchFileContent(item.path)).blob)
        : fileContentUrl(item.path);
      const link = document.createElement('a');
      link.href = url;
      link.download = item.name;
      document.body.append(link);
      link.click();
      link.remove();
      if (hasToken) window.setTimeout(() => URL.revokeObjectURL(url), 30_000);
    } catch (error) {
      reportError(error);
    }
  }

  function startUpload(files: FileList) {
    if (!writable || !files.length) return;
    queue.enqueueFiles(files, path, cachedItems ?? []);
  }

  function changeSort(field: SortField) {
    setSort((current) => ({
      field,
      direction: current.field === field
        ? (current.direction === 'asc' ? 'desc' : 'asc')
        : (field === 'name' ? 'asc' : 'desc'),
    }));
  }

  function enterDrag(event: DragEvent) {
    if (!writable || !event.dataTransfer.types.includes('Files')) return;
    event.preventDefault();
    dragDepth.current++;
    setDragging(true);
  }

  function leaveDrag(event: DragEvent) {
    if (!event.dataTransfer.types.includes('Files')) return;
    dragDepth.current = Math.max(0, dragDepth.current - 1);
    if (!dragDepth.current) setDragging(false);
  }

  function drop(event: DragEvent) {
    event.preventDefault();
    dragDepth.current = 0;
    setDragging(false);
    startUpload(event.dataTransfer.files);
  }

  useEffect(() => {
    function keydown(event: KeyboardEvent) {
      if (dialog || event.isComposing) return;
      if (event.key === 'Escape') {
        setMobileOpen(false);
        if (document.activeElement === searchInput.current) {
          setSearch('');
          searchInput.current?.blur();
        } else {
          setSelection(null);
        }
        return;
      }
      const key = event.key.toLowerCase();
      if ((event.metaKey || event.ctrlKey) && key === 'k') {
        event.preventDefault();
        if (tab === 'drive') searchInput.current?.focus();
        else {
          setTab('drive');
          requestAnimationFrame(() => searchInput.current?.focus());
        }
        return;
      }
      if (tab !== 'drive') return;
      if ((event.metaKey || event.ctrlKey) && event.shiftKey && key === 'n' && writable) {
        event.preventDefault();
        openNewFolder();
        return;
      }
      if (event.target instanceof HTMLElement && event.target.closest('input, select, textarea, [contenteditable], a, button:not([data-file-row])')) return;
      if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
        if (!items.length) return;
        event.preventDefault();
        const index = items.findIndex((item) => item.path === selection);
        const next = index === -1 ? (event.key === 'ArrowDown' ? 0 : items.length - 1)
          : Math.max(0, Math.min(items.length - 1, index + (event.key === 'ArrowDown' ? 1 : -1)));
        setSelection(items[next].path);
        tablePane.current?.querySelectorAll<HTMLButtonElement>('[data-file-row]')[next]?.focus();
      } else if (!event.defaultPrevented && event.key === 'Enter' && selected) {
        event.preventDefault();
        openItem(selected);
      }
    }
    window.addEventListener('keydown', keydown);
    return () => window.removeEventListener('keydown', keydown);
  });

  return (
    <div className="app-shell">
      <Sidebar
        health={health}
        connection={connection}
        activeTab={tab}
        shareCount={shares?.length ?? null}
        theme={theme}
        hasBearerToken={hasToken}
        logoutDisabled={transfersPending}
        mobileOpen={mobileOpen}
        onCloseMobile={() => setMobileOpen(false)}
        onSelectTab={setTab}
        onToggleTheme={onToggleTheme}
        onLogout={onLogout}
      />
      <div className="workspace-column">
        <Topbar
          activeTab={tab}
          currentPath={path}
          searchQuery={search}
          searchInputRef={searchInput}
          disabledMutations={!writable}
          onOpenMobileMenu={() => setMobileOpen(true)}
          onNavigateFolder={navigate}
          onSearchChange={setSearch}
          onOpenNewFolder={openNewFolder}
          onTriggerUpload={() => fileInput.current?.click()}
        />
        <StatusBanner connection={connection} nodeName={health.node} retrying={probing} onRetry={onReconnect} />
        <div className="workspace-body">
          {tab === 'drive' && (
            <>
              <main
                className="content-pane"
                ref={tablePane}
                onDragEnter={enterDrag}
                onDragLeave={leaveDrag}
                onDragOver={(event) => {
                  if (event.dataTransfer.types.includes('Files')) {
                    event.preventDefault();
                    event.dataTransfer.dropEffect = writable ? 'copy' : 'none';
                  }
                }}
                onDrop={drop}
              >
                <FileTable
                  items={items}
                  selectedPath={selected?.path ?? null}
                  sort={sort}
                  loading={online && loadingFolder}
                  error={online ? folderError : null}
                  cached={cachedItems !== undefined}
                  filtering={Boolean(search.trim())}
                  writable={writable}
                  online={online}
                  onSort={changeSort}
                  onSelect={(item) => setSelection(item.path)}
                  onOpen={openItem}
                  onDownload={(item) => { void download(item); }}
                  onDelete={openDelete}
                  onUpload={() => fileInput.current?.click()}
                  onRefresh={refreshFolder}
                />
                {dragging && writable && (
                  <div className="dropzone-overlay"><Upload size={24} /><span className="dropzone-overlay-title">Drop files to upload</span><span className="dropzone-overlay-sub">{path || 'My Drive'}</span></div>
                )}
              </main>
              {selected && (
                <Inspector
                  key={selected.id}
                  item={selected}
                  online={online}
                  writable={writable}
                  onClose={() => setSelection(null)}
                  onOpen={openItem}
                  onDownload={(item) => { void download(item); }}
                  onShare={openCreateLink}
                  onDelete={openDelete}
                  onCopy={(value, label) => { void copy(value, label); }}
                  onApiError={onApiError}
                />
              )}
            </>
          )}
          {tab === 'shares' && (
            <SharedLinks
              items={shares}
              loading={online && loadingShares}
              error={online ? sharesError : null}
              writable={writable}
              onCreate={() => openCreateLink()}
              onCopy={(share) => { void copy(new URL(share.url, window.location.origin).href, 'Share link copied'); }}
              onRevoke={openRevoke}
              onRefresh={refreshShares}
            />
          )}
          {tab === 'status' && <StorageStatus health={health} probing={probing} onRefresh={onReconnect} />}
        </div>
      </div>
      <input hidden ref={fileInput} type="file" multiple onChange={(event) => {
        if (event.currentTarget.files) startUpload(event.currentTarget.files);
        event.currentTarget.value = '';
      }} />
      <UploadTray
        tasks={queue.tasks}
        writable={writable}
        onRetry={queue.retryTask}
        onCancel={queue.cancelTask}
        onDismiss={queue.dismissTask}
        onClear={queue.clearFinished}
      />
      {dialog && (
        <WorkspaceDialog
          details={dialog}
          writable={writable}
          onClose={() => setDialog(null)}
          onApiError={onApiError}
          onCopy={copy}
        />
      )}
      {notice && (
        <div className="toast-stack">
          <div className={`toast ${notice.tone}`} role="status">
            <span className="toast-msg">{notice.text}</span>
            <button className="icon-btn" aria-label="Dismiss notification" onClick={() => setNotice(null)}><X size={14} /></button>
          </div>
        </div>
      )}
    </div>
  );
}

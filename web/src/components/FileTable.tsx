import { ArrowDown, ArrowUp, Download, File, FolderClosed, Trash2, Upload } from 'lucide-react';
import { formatAbsoluteTime, formatBytes, formatTimestamp } from '../lib/format';
import type { FileItem, SortDirection, SortField } from '../types/api';

export interface Sort {
  field: SortField;
  direction: SortDirection;
}

interface FileTableProps {
  items: FileItem[];
  selectedPath: string | null;
  sort: Sort;
  loading: boolean;
  error: string | null;
  cached: boolean;
  filtering: boolean;
  writable: boolean;
  online: boolean;
  onSort: (field: SortField) => void;
  onSelect: (item: FileItem) => void;
  onOpen: (item: FileItem) => void;
  onDownload: (item: FileItem) => void;
  onDelete: (item: FileItem) => void;
  onUpload: () => void;
  onRefresh: () => void;
}

const columns = [
  { field: 'name', label: 'Name' },
  { field: 'size', label: 'Size' },
  { field: 'modified', label: 'Modified' },
] satisfies { field: SortField; label: string }[];

export function FileTable({
  items, selectedPath, sort, loading, error, cached, filtering, writable, online,
  onSort, onSelect, onOpen, onDownload, onDelete, onUpload, onRefresh,
}: FileTableProps) {
  const SortIcon = sort.direction === 'asc' ? ArrowUp : ArrowDown;

  return (
    <>
      <div className="table-scroll" aria-busy={loading}>
        <table className="file-table" aria-label="Files in this folder">
          <thead>
            <tr>
              {columns.map(({ field, label }) => (
                <th
                  key={field}
                  className={`col-${field}`}
                  scope="col"
                  aria-sort={field === sort.field ? (sort.direction === 'asc' ? 'ascending' : 'descending') : undefined}
                >
                  <button className={`sort-btn${field === sort.field ? ' active' : ''}`} onClick={() => onSort(field)}>
                    {label}{field === sort.field && <SortIcon size={12} />}
                  </button>
                </th>
              ))}
              <th className="col-sha" scope="col">SHA-256</th>
              <th className="col-actions" scope="col">Actions</th>
            </tr>
          </thead>
          <tbody>
            {loading && !cached && Array.from({ length: 6 }, (_, row) => (
              <tr className="file-row" key={row} aria-hidden="true">
                {['55%', '60%', '75%', '75%', '60%'].map((width, index) => (
                  <td key={index}><span className="shimmer-bar" style={{ width }} /></td>
                ))}
              </tr>
            ))}
            {items.map((item) => {
              const folder = item.type === 'folder';
              const ItemIcon = folder ? FolderClosed : File;
              const selected = item.path === selectedPath;
              return (
                <tr
                  className={`file-row${selected ? ' selected' : ''}`}
                  key={item.id}
                  onClick={() => onSelect(item)}
                  onDoubleClick={() => onOpen(item)}
                >
                  <td>
                    <button
                      className="file-name-cell file-name-button"
                      data-file-row
                      title={item.name}
                      aria-pressed={selected}
                      onClick={(event) => {
                        event.stopPropagation();
                        onSelect(item);
                      }}
                      onKeyDown={(event) => {
                        if (event.key === 'Enter') {
                          event.preventDefault();
                          onOpen(item);
                        }
                      }}
                    >
                      <ItemIcon size={16} className={`file-icon${folder ? ' folder' : ''}`} />
                      <span className="file-name-text">{item.name}</span>
                    </button>
                  </td>
                  <td className="cell-meta mono tabular">{folder ? '—' : formatBytes(item.size_bytes)}</td>
                  <td className="cell-meta tabular" title={formatAbsoluteTime(item.updated_at)}>{formatTimestamp(item.updated_at)}</td>
                  <td className="cell-sha" title={item.sha256 ?? undefined}>{item.sha256?.slice(0, 10) ?? '—'}</td>
                  <td>
                    <div className="row-actions" onClick={(event) => event.stopPropagation()} onDoubleClick={(event) => event.stopPropagation()}>
                      {!folder && (
                        <button className="icon-btn" aria-label={`Download ${item.name}`} title="Download" disabled={!online} onClick={() => onDownload(item)}>
                          <Download size={14} />
                        </button>
                      )}
                      <button className="icon-btn danger" aria-label={`Delete ${item.name}`} title="Delete" disabled={!writable} onClick={() => onDelete(item)}>
                        <Trash2 size={14} />
                      </button>
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
        {error && (
          <div className="empty-state">
            <h3>Unable to load this folder</h3>
            <p role="alert">{error}</p>
            {online && <button className="btn" onClick={onRefresh}>Retry</button>}
          </div>
        )}
        {!loading && !error && items.length === 0 && (
          <div className="empty-state">
            <div className="empty-state-icon"><FolderClosed size={19} /></div>
            <h3>{filtering ? 'No matching files' : cached ? 'This folder is empty' : 'Folder unavailable offline'}</h3>
            {!filtering && writable && (
              <>
                <p>Drop files here or choose files to upload.</p>
                <button className="btn btn-primary" onClick={onUpload}><Upload size={14} /> Upload First File</button>
              </>
            )}
          </div>
        )}
      </div>
      <div className="table-footer tabular">
        <span>{items.length} {items.length === 1 ? 'item' : 'items'}</span>
        <span>↑ ↓ Select · Enter Open</span>
      </div>
    </>
  );
}

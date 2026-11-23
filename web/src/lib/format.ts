export function formatBytes(bytes: number, decimals?: number): string {
  if (!Number.isFinite(bytes) || bytes < 0) return '0 B';
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const k = 1024;
  const i = Math.min(Math.floor(Math.log(bytes) / Math.log(k)), units.length - 1);
  const value = bytes / Math.pow(k, i);
  const digits = decimals ?? (i === 0 ? 0 : i >= 3 ? 2 : value >= 100 ? 0 : 1);
  return `${value.toFixed(digits)} ${units[i]}`;
}

export function formatGigabytes(bytes: number): string {
  const gb = bytes / (1024 * 1024 * 1024);
  return gb.toFixed(2);
}

export function formatExactBytes(bytes: number): string {
  return `${bytes.toLocaleString('en-US')} bytes`;
}

export function formatTimestamp(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;

  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);

  if (diffSec >= 0 && diffSec < 60) return 'Just now';
  if (diffSec >= 60 && diffSec < 3600) {
    const mins = Math.floor(diffSec / 60);
    return `${mins}m ago`;
  }
  if (diffSec >= 3600 && diffSec < 86400) {
    const hrs = Math.floor(diffSec / 3600);
    return `${hrs}h ago`;
  }

  const sameYear = date.getFullYear() === now.getFullYear();
  return date.toLocaleString('en-US', {
    month: 'short',
    day: '2-digit',
    ...(sameYear ? {} : { year: 'numeric' }),
    hour: '2-digit',
    minute: '2-digit',
    hour12: false,
  });
}

export function formatAbsoluteTime(iso: string): string {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return iso;
  return date.toLocaleString('en-US', {
    year: 'numeric',
    month: 'short',
    day: '2-digit',
    hour: '2-digit',
    minute: '2-digit',
    second: '2-digit',
    hour12: false,
  });
}

export function formatExpiryRemaining(iso: string): { label: string; expired: boolean } {
  const date = new Date(iso);
  if (Number.isNaN(date.getTime())) return { label: iso, expired: false };
  const diffMs = date.getTime() - Date.now();
  if (diffMs <= 0) {
    return { label: 'Expired', expired: true };
  }
  const mins = Math.floor(diffMs / 60_000);
  if (mins < 60) return { label: `${Math.max(1, mins)}m left`, expired: false };
  const hours = Math.floor(mins / 60);
  if (hours < 48) return { label: `${hours}h ${mins % 60}m left`, expired: false };
  const days = Math.floor(hours / 24);
  return { label: `${days}d ${hours % 24}h left`, expired: false };
}

export function joinDrivePath(folder: string, name: string): string {
  const cleanFolder = folder.replace(/^\/+|\/+$/g, '');
  const cleanName = name.replace(/^\/+|\/+$/g, '');
  return cleanFolder ? `${cleanFolder}/${cleanName}` : cleanName;
}

export function splitBreadcrumbs(path: string): Array<{ label: string; path: string }> {
  const clean = path.replace(/^\/+|\/+$/g, '');
  if (!clean) return [];
  const parts = clean.split('/');
  const result: Array<{ label: string; path: string }> = [];
  let acc = '';
  for (const part of parts) {
    acc = acc ? `${acc}/${part}` : part;
    result.push({ label: part, path: acc });
  }
  return result;
}

export function parentFolderPath(path: string): string {
  const clean = path.replace(/^\/+|\/+$/g, '');
  const idx = clean.lastIndexOf('/');
  return idx === -1 ? '' : clean.slice(0, idx);
}

export type PreviewKind = 'image' | 'pdf' | 'text' | 'none';

const TEXT_EXTENSIONS = new Set([
  'txt', 'md', 'markdown', 'json', 'yaml', 'yml', 'toml', 'xml', 'csv', 'tsv',
  'log', 'ini', 'cfg', 'conf', 'env', 'rs', 'ts', 'tsx', 'js', 'jsx', 'mjs', 'cjs',
  'py', 'go', 'java', 'kt', 'swift', 'c', 'h', 'cpp', 'hpp', 'cs', 'rb', 'php',
  'sh', 'bash', 'zsh', 'fish', 'sql', 'html', 'htm', 'css', 'scss', 'less',
  'dockerfile', 'makefile', 'gitignore', 'lock', 'svg',
]);

export function getPreviewKind(name: string, mime: string | null): PreviewKind {
  const m = (mime || '').toLowerCase();
  if (m.startsWith('image/') && m !== 'image/svg+xml') {
    return 'image';
  }
  if (m === 'application/pdf' || name.toLowerCase().endsWith('.pdf')) {
    return 'pdf';
  }
  if (
    m.startsWith('text/') ||
    m === 'application/json' ||
    m === 'application/xml' ||
    m === 'application/javascript' ||
    m === 'application/x-javascript' ||
    m === 'application/typescript' ||
    m === 'application/x-yaml' ||
    m === 'application/toml' ||
    m === 'image/svg+xml'
  ) {
    return 'text';
  }

  const lower = name.toLowerCase();
  const base = lower.split('/').pop() || lower;
  if (base === 'dockerfile' || base === 'makefile' || base === 'readme' || base === 'license') {
    return 'text';
  }
  const dot = base.lastIndexOf('.');
  if (dot !== -1) {
    const ext = base.slice(dot + 1);
    if (TEXT_EXTENSIONS.has(ext)) return 'text';
    if (['png', 'jpg', 'jpeg', 'gif', 'webp', 'avif', 'bmp', 'ico'].includes(ext)) {
      return 'image';
    }
  }
  return 'none';
}

export function copyTextToClipboard(text: string): Promise<void> {
  if (navigator.clipboard && navigator.clipboard.writeText) {
    return navigator.clipboard.writeText(text).catch(() => fallbackCopy(text));
  }
  return fallbackCopy(text);
}

function fallbackCopy(text: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const previousFocus = document.activeElement;
    const ta = document.createElement('textarea');
    ta.value = text;
    ta.readOnly = true;
    ta.style.position = 'fixed';
    ta.style.opacity = '0';
    const container = previousFocus?.closest('dialog[open]') ?? document.body;
    container.appendChild(ta);
    ta.focus({ preventScroll: true });
    ta.select();
    try {
      if (document.execCommand('copy')) resolve();
      else reject(new Error('Clipboard access was denied'));
    } catch (err) {
      reject(err);
    } finally {
      ta.remove();
      if (previousFocus instanceof HTMLElement) previousFocus.focus({ preventScroll: true });
    }
  });
}

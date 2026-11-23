# TailDrive v1 — Design & UX Specification

> Anchor: `macmini-ci` over Tailscale (`100.x` / MagicDNS) · Dev Quota: **5 GB** cap + **5 GB** SSD free-space floor · Auth: `tailscale serve` WhoIs headers + scoped Bearer tokens.

## 1. v1 User Flows (Over Tailscale)

### Web UI (`https://taildrive.<tailnet>.ts.net`)
1. **Entry & Auth:** Auto-authenticates via `Tailscale-User-Login` header from `tailscale serve`; falls back to a minimal Bearer token prompt if accessed directly via IP:port.
2. **Browse & Organize:** Root `My Drive` list with breadcrumb path bar, keyboard-first navigation (`↑/↓`, `Enter` open, `⌘⇧N` new folder, `⌘K` quick search), and sort by Name/Size/Modified.
3. **Upload & Verify:** Drag-and-drop or `Upload` CTA -> pre-flight quota check -> browser computes SHA-256 (`SubtleCrypto` / streaming worker) -> `PUT /v1/files` with live progress in bottom-right Upload Tray -> row inserts on `201`.
4. **Preview, Download, Share & Delete:** Single-click opens right Inspector/Preview drawer (image/PDF/code/text + SHA-256 fingerprint + `Download` / `Copy Share Link`); `Delete` removes immediately with a 5s undo toast (or immediate hard delete in Slice 1 with confirmation for folders).

### Native macOS App (Menu-Bar + Synced Mount)
1. **Onboarding:** Probe `GET /v1/health` over MagicDNS -> authenticate via Tailnet identity or pasted API key -> select local sync folder (`~/TailDrive`; v1 mirrored directory, keeping `etag`/`sha256`/`version` ready for v2 File Provider).
2. **Menu-Bar Popover (`320×420px`):** Header (Tailscale status dot + `macmini-ci` + Pause/Resume toggle) -> Quota meter -> Active transfer list (filename, progress bar, speed, retry/cancel) -> 5 recent files (click reveals in Finder) -> Footer (`Open ~/TailDrive`, `Web UI`, `Preferences`).
3. **Sync Loop:** Local `FSEvents` watcher computes SHA-256 and pushes deltas via `PUT /v1/files` (with `If-Match`); remote poll/SSE updates local mirror. Conflicts in v1 save `<name> (conflict <host>-<ts>).<ext>` (never silent overwrite).

### Android App (Over Tailscale)
1. **Setup:** Verify Tailscale VPN active -> enter/discover MagicDNS host + scoped Bearer token -> validate via `GET /v1/health`.
2. **Navigation (3 Bottom Tabs):** `Files` (folder tree, pull-to-refresh, tap preview/download, overflow menu: Share/Delete, FAB: Upload / New Folder), `Backup` (toggle background camera-roll upload via `WorkManager` constrained to Tailscale + Wi-Fi/unmetered), `Transfers` (active/failed queue with one-tap retry).

## 2. Design System & Layout Hierarchy

**Direction:** High-density archival utility (inspired by macOS Finder + Linear) — crisp 1px hairlines, monochrome zinc surfaces, tabular numerals, single cobalt accent, zero decorative gradients.

### Design Tokens (Light / Dark)
| Token | Light | Dark | Usage |
| :--- | :--- | :--- | :--- |
| `bg.canvas` / `bg.surface` | `#FAFAFA` / `#FFFFFF` | `#0B0C0E` / `#131518` | App backdrop / cards, drawers, tables |
| `bg.subtle` / `border.hairline` | `#F4F4F5` / `#E4E4E7` | `#1B1E23` / `#272A30` | Row hover, inputs / 1px dividers |
| `text.primary` / `text.secondary` | `#111214` / `#52525B` | `#F4F4F6` / `#9CA3AF` | Filenames, headings / sizes, timestamps |
| `accent.primary` | `#2563EB` | `#3B82F6` | Primary CTA, focus ring (`2px`), active nav |
| `status.ok` / `warn` / `danger` | `#16A34A` / `#D97706` / `#DC2626` | `#22C55E` / `#F59E0B` / `#EF4444` | Tailnet live / quota `>=80%` / quota `>=95%` & errors |

- **Typography:** Sans `Inter` (Web) / `SF Pro` (macOS) / `Roboto` (Android); Mono `JetBrains Mono` / `SF Mono` for paths, byte sizes, SHA-256 prefixes, and `100.x` IPs. Apply `font-variant-numeric: tabular-nums` to all sizes, quotas, and timestamps. Scale: `11/16px` meta, `13/18px` table rows, `14/20px` controls, `18/24px` (600) view title.
- **Spacing & Radii:** `4px` base grid (`4/8/12/16/24px`). Row height `40px` (Web/macOS) / `56dp` (Android, min `48dp` touch target). Radii: `6px` inputs/buttons, `10px` drawers/popovers, `9999px` status pills.

### Layout Hierarchy
- **Web Desktop (`>=1024px`):** `240px` left rail (node pill `macmini-ci`, nav `Files / Shared / Trash`, pinned bottom **Quota Meter**: `X.XX / 5.00 GB` bar) | `52px` sticky topbar (breadcrumbs, `⌘K` search, `New Folder`, primary `Upload`) | fluid table (`Name 46%`, `Size 14%`, `Modified 18%`, `SHA-256 12%`, `Actions 10%`) | collapsible `360px` right Preview/Inspector drawer | floating `340px` bottom-right Upload Tray. Below `768px`, left rail collapses to slide-over drawer.
- **macOS Popover:** Fixed `320px` width, `12px` outer padding, `8px` section dividers, native vibrancy disabled in high-contrast mode.
- **Android:** `56dp` top bar (title + Tailscale status dot + search) -> `16dp` horizontal list padding -> sticky Quota warning strip when usage `>=80%` -> `64dp` bottom bar + `56dp` FAB.

## 3. Non-Happy States & Recovery Contracts

| State | Trigger / Code | UI Treatment (Web / macOS / Android) | Recovery & Guardrails |
| :--- | :--- | :--- | :--- |
| **Loading / Empty** | Initial fetch / 0 items | 6 shimmer rows (`40px`); empty folder shows dropzone + `Upload First File` CTA. | Keep left rail & quota interactive; never block nav. |
| **Offline** | No OS network / `ECONNABORTED` | Top amber banner: `Offline — read-only cache`. Upload/Delete CTAs disabled; macOS icon muted. | Persist pending mobile/macOS queue; auto-resume on network restore. |
| **Tailscale Disconnected** | Internet up, `100.x`/MagicDNS unreachable | Distinct red-amber banner + gray Tailnet dot: `Tailscale disconnected — connect to reach macmini-ci`. | Show `Retry` + deep link (`tailscale://`) on macOS/Android; never conflate with generic offline. |
| **5 GB Dev Quota Hit** | Pre-flight `size > remaining` or `413`/`507` `ERR_QUOTA_EXCEEDED` / `ERR_DISK_FLOOR` | Quota bar turns red (`100%`); modal: `Storage limit reached (X.XX / 5.00 GB)` with remaining bytes. | Block upload pre-flight if size known; provide `Sort by Largest` shortcut to free space. |
| **Upload Retry** | `5xx`, `429`, socket drop mid-PUT | Transfer row enters amber `Retrying (n/3)…`; after 3 tries shows red `Failed` + `Retry` / `Dismiss`. | Backoff `1s -> 2s -> 4s`; retain file handle/URI in queue so user never re-picks file. |
| **Checksum Mismatch** | `422 ERR_CHECKSUM_MISMATCH` | Red toast + row badge: `Integrity mismatch — upload discarded (file not saved)`. | Server deletes temp blob; client offers 1-click `Re-hash & Retry` recomputing SHA-256 from source. |

## Handoff to @coder

### Security & Invariants (Slice 1)
- **Auth:** Accept `Tailscale-User-Login` **only** when request originates from loopback (`tailscale serve`), or `Authorization: Bearer <token>` (constant-time compare). Return `401 ERR_UNAUTHORIZED` otherwise.
- **Path Safety:** Normalize relative paths; reject empty, leading `/`, `..`, `\`, NUL bytes, or symlink escapes with `400 ERR_INVALID_PATH`.
- **Quota & Floor:** Default `TAILDRIVE_QUOTA_BYTES=5368709120` (5 GB) and `TAILDRIVE_FREE_FLOOR_BYTES=5368709120` (5 GB min free on `macmini-ci`). Check before accepting stream AND atomically before commit; reject with `507 ERR_QUOTA_EXCEEDED` or `507 ERR_DISK_FLOOR`.
- **Error Envelope (all 4xx/5xx):** `{ "error": { "code": "ERR_*", "message": "human-readable", "retryable": false, "details": {} } }`.

### Slice 1 REST API Endpoints (`/v1`)
1. **`GET /v1/health`** -> `200 OK`
```json
{
  "status": "ok", "node": "macmini-ci", "tailnet_user": "alice@ts.net", "storage_driver": "local-fs",
  "quota": { "used_bytes": 104857600, "limit_bytes": 5368709120, "remaining_bytes": 5263831552, "disk_free_bytes": 32212254720, "disk_floor_bytes": 5368709120, "status": "ok" }
}
```
(`quota.status`: `"ok"` `<80%`, `"warn"` `80–94%`, `"full"` `>=95%` or disk below floor.)

2. **`GET /v1/files?path=<rel_dir>&cursor=<tok>&limit=100`** -> `200 OK`
```json
{
  "path": "docs",
  "items": [{ "id": "f_01J...", "name": "spec.pdf", "path": "docs/spec.pdf", "type": "file", "size_bytes": 204800, "mime": "application/pdf", "sha256": "<64-hex>", "version": 1, "etag": "\"<64-hex>\"", "updated_at": "2026-11-22T19:00:00Z" }],
  "next_cursor": null
}
```
(`POST /v1/folders` body `{"path":"docs/q4"}` -> `201` folder item; idempotent `409 ERR_ALREADY_EXISTS` if exists.)

3. **`PUT /v1/files?path=<rel_path>`** — Headers: `Content-Type`, `Content-Length`, `X-Content-SHA256: <64-hex>` (required), optional `If-Match: <etag>`.
   - Streams to temp blob, computes SHA-256 on the fly, verifies exact byte length + hash, commits atomically to SQLite + storage.
   - Returns `201 Created` (new) or `200 OK` (overwrite): `{ "item": <FileItem>, "quota": <Quota> }`.
   - Errors: `400 ERR_INVALID_PATH`, `409 ERR_CONFLICT` (`If-Match` mismatch), `422 ERR_CHECKSUM_MISMATCH`, `507 ERR_QUOTA_EXCEEDED` / `ERR_DISK_FLOOR`.

4. **`GET /v1/files/content?path=<rel_path>`** -> `200 OK` (or `206 Partial Content` if `Range` header set).
   - Headers: `Content-Type`, `Content-Length`, `ETag: "<sha256>"`, `X-Content-SHA256: <sha256>`, `Content-Disposition`. `404 ERR_NOT_FOUND` if missing.

5. **`DELETE /v1/files?path=<rel_path>`** (optional `&recursive=true` for non-empty dirs) -> `200 OK`
```json
{ "deleted": true, "path": "docs/spec.pdf", "freed_bytes": 204800, "quota": { "used_bytes": 104652800, "limit_bytes": 5368709120, "remaining_bytes": 5264036352, "disk_free_bytes": 32212459520, "disk_floor_bytes": 5368709120, "status": "ok" } }
```

### Storage Adapter Interface (Local FS in Slice 1 -> S3/SeaweedFS/Garage Drop-In)
```ts
interface StorageAdapter {
  readonly driverName: "local-fs" | "s3";
  put(key: string, stream: Readable, expectedSize: number, expectedSha256: string): Promise<{ key: string; sizeBytes: number; sha256: string; etag: string }>; // atomic stage + hash verify + commit; unlink temp on error
  get(key: string, range?: { start: number; end?: number }): Promise<{ stream: Readable; sizeBytes: number; sha256: string }>;
  stat(key: string): Promise<{ sizeBytes: number; sha256: string } | null>;
  delete(key: string): Promise<{ freedBytes: number }>;
  usage(): Promise<{ usedBytes: number; diskFreeBytes: number }>; // LocalFS reads statfs(dataDir) + tracked blob bytes
}
```
- **Blob Layout (`local-fs`):** `$TAILDRIVE_DATA_DIR/blobs/<sha256[0..2]>/<id>` + `$TAILDRIVE_DATA_DIR/tmp/<uuid>` (same filesystem for atomic `rename`).
- **Metadata DB (SQLite Slice 1):** `files(id PK, path UNIQUE, parent_path INDEX, type, object_key, size_bytes, mime, sha256, version, updated_at)` — swapping to S3/SeaweedFS/Garage or Postgres requires zero API or client changes.

## Contract Alignment Addendum (Rust Workspace + QA Slice 1 Sync)
- **Auth:** `/v1/health` requires auth (unauthenticated probe returns `401`). Loopback verification MUST inspect true TCP socket peer (`ConnectInfo<SocketAddr>`), never `X-Forwarded-For`.
- **Storage Trait:** Omit `stat()` and `usage()` from `StorageAdapter`; SQLite metadata tracks `used_bytes` (O(1) on S3/Garage), and storage exposes `disk_free_bytes()` (returning `None` on remote S3).
- **Additional Error Codes:** `411 ERR_LENGTH_REQUIRED` (missing `Content-Length` on single-shot `PUT`), `409 ERR_NOT_EMPTY` (folder delete without `recursive=true`), `416 ERR_RANGE_NOT_SATISFIABLE` (invalid `Range` or 0-byte range).
- **Phase 2 Extensions:**
  - Resumable chunked uploads: `POST /v1/uploads` -> `{ upload_id, part_size }`, `PUT /v1/uploads/{id}/parts/{part_num}` (with `X-Content-SHA256`), `POST /v1/uploads/{id}/complete` (with final `X-Content-SHA256` + optional `If-Match`), `DELETE /v1/uploads/{id}`.
  - Expiring read-only share links: `POST /v1/shares` `{ path, expires_in_seconds }`, `GET /v1/shares`, `DELETE /v1/shares/{id}`, and public `GET /s/{token}` (stream download without auth, honoring expiry & revocation).

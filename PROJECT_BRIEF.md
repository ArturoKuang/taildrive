# TailDrive — Private Self-Hosted Cloud Storage Data Center

## Vision
Build our own custom, private Dropbox-style cloud storage data center anchored on `macmini-ci` and reachable over Tailscale, engineered to scale horizontally to additional machines/drives later.

## Locked Decisions (v1)
1. **Architecture (Hybrid Open-Source + Custom):**
   - **Storage engine:** Open-source distributed S3-compatible object storage (e.g., **SeaweedFS** or **Garage** — pluggable S3 driver behind a clean storage interface so adding extra nodes/drives later requires zero client changes).
   - **Control plane / Sync API:** Custom lightweight API + metadata DB (SQLite for single-node dev, swappable to Postgres for multi-node) handling auth (Tailscale identity / WhoIs + bearer tokens), folder hierarchy, chunked/resumable uploads, file versions/hashes, share links, and per-node storage quotas.
   - **Clients:** Custom **Web UI**, **Android app**, and **native macOS app** (Finder/menu-bar sync & mount).
2. **v1 Feature Bar (Cloud Drive + Desktop Mount first):**
   - Web file browser (upload, download, folders, preview, search, share links, storage quota meter).
   - Android app (browse, upload/download, background photo/file upload over Tailscale).
   - Native macOS app (menu-bar app + synced folder / mount).
   - **v2 (deferred, keep contract ready):** macOS File Provider on-demand virtual files & full bidirectional offline conflict resolution.
3. **Storage Plan (`macmini-ci` constraint):**
   - `macmini-ci` currently has ~33 GB free on its internal 228 GB SSD (no external volumes mounted).
   - v1 runs on the internal SSD with a **strict configurable dev storage quota** (e.g., 5 GB default cap + hard free-space floor) and a clean swappable storage root/endpoint so we can point it at an external SSD/DAS (`/Volumes/...`) or multi-node SeaweedFS/Garage cluster later without rewriting the app.
4. **Networking & Security:**
   - Private by default over **Tailscale** (`100.x.y.z` / MagicDNS).
   - Leverage Tailscale identity headers (`tailscale serve` / WhoIs) + scoped API keys for mobile/desktop clients.
   - Enforce path-traversal protection, checksum verification (SHA-256), and strict quota checks before accepting uploads.

## Team Roster & Routing
- **`@lead`** (`claude` / `claude-lead`): Architecture, contracts, decomposition, sign-off.
- **`@design`** (`agy` / `agy-design`): Cross-platform UX flows, design tokens, state specs (Web, macOS, Android).
- **`@coder`** (`claude` / `claude-coder` + `codex` / `codex-coder`): Backend, storage adapter, clients, tests.
- **`@qa`** (`codex` / `codex-qa`): Adversarial review, automated tests, edge-case & security gates (`pstack`).

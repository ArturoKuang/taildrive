# TailDrive — Private Self-Hosted Cloud Storage Data Center (Rust)

## Vision
Build our own custom, private Dropbox-style cloud storage data center anchored on `macmini-ci` (`arturos-mac-mini`, `100.97.87.111`) over Tailscale, engineered to scale horizontally to additional machines/drives later.

## Locked Stack & Architecture
- **Workspace Language:** **Rust** Cargo workspace (`rustc 1.100+`):
  - `crates/taildrive-core`: shared domain types, NFC path safety & traversal guards, streaming SHA-256, `/v1` API client, retry backoff, and `If-Match` conflict filename resolver.
  - `crates/taildrive-server`: `axum` + `tokio` + `sqlx` (SQLite, swappable to Postgres) + `opendal` (pluggable `local-fs` and `s3` for **Garage** / SeaweedFS) + `rust-embed` (serves `web/dist` directly from the single binary).
  - `crates/taildrive-ffi`: **UniFFI** bindings exporting `taildrive-core` to **Swift** (macOS) and **Kotlin** (Android).
- **Distributed Storage Engine:** **Garage (`2.5.0`)** S3-compatible object store (Rust-native, multi-node ready), with `local-fs` fallback and a configurable storage root so swapping from internal SSD to `/Volumes/<external-ssd>` or adding extra nodes requires zero client changes.
- **Clients:**
  - `web/`: React + TypeScript + Vite Web UI (embedded in `taildrive-server`), matching `docs/design-v1.md`.
  - `clients/macos/`: Native SwiftUI menu-bar app + `~/TailDrive` synced folder powered by `taildrive-ffi` (v1 mirror; contract ready for v2 File Provider).
  - `clients/android/`: Native Kotlin + Jetpack Compose + WorkManager app powered by `taildrive-ffi` (3 tabs: Files, Backup, Transfers).
- **Storage & Security Invariants (incorporating Slice 1 QA audit):**
  - Default `TAILDRIVE_QUOTA_BYTES=5368709120` (5 GB) and `TAILDRIVE_FREE_FLOOR_BYTES=5368709120` (5 GB SSD floor on `macmini-ci`).
  - Atomic `tokio::sync::Mutex` capacity reservation + commit-time quota & free-space floor re-check + shielded rollback cleanup.
  - Loopback-only `Tailscale-User-Login` verified via true TCP peer `ConnectInfo<SocketAddr>` (never trusting `X-Forwarded-For`), plus constant-time scoped Bearer tokens.
  - `/v1/health` requires auth (`401` unauthenticated probe). Standard error envelope `{ error: { code, message, retryable, details } }` on all 4xx/5xx including `411 ERR_LENGTH_REQUIRED`, `409 ERR_NOT_EMPTY`, `416 ERR_RANGE_NOT_SATISFIABLE`.
  - Resumable chunked uploads (`/v1/uploads`) and expiring read-only share links (`/v1/shares`, `/s/{token}`).

# TailDrive Phase 1 (Rust Workspace) — Independent QA Audit

**Scope:** `crates/taildrive-core`, `crates/taildrive-server` vs `PROJECT_BRIEF.md`, `docs/design-v1.md`, `docs/qa-slice1.md`
**Toolchain:** `rustc 1.100.0`, `cargo 1.100.0` (`unsafe_code = "forbid"`)

## 1. Automated Verification
- `cargo test --workspace` → **24 passed, 0 failed, 0 ignored** (0 doc-tests):
  - `taildrive-core` unit (`src/lib.rs`): **12 passed** (`path`, `hash`, `client` backoff)
  - `taildrive-server` unit (`src/lib.rs`): **2 passed** (`auth`, `content::range_edges`); `src/main.rs`: **0 tests**
  - `taildrive-server` live TCP integration (`tests/live_tcp.rs`): **10 passed** in `1.07s`
- `cargo clippy --workspace --all-targets -- -D warnings` → **0 warnings, exit 0**.

## 2. Area-by-Area Audit

### Authentication (`crates/taildrive-server/src/auth.rs`, `lib.rs`)
- **Pass:** `App::serve` wires `into_make_service_with_connect_info::<SocketAddr>()`; `require_auth` reads true TCP peer `ConnectInfo`, canonicalizes IPv4-mapped IPv6 (`peer.ip().to_canonical().is_loopback()`), rejects missing `ConnectInfo`, and ignores `X-Forwarded-For`.
- **Pass:** Bearer tokens are SHA-256 hashed and compared across all configured hashes via `subtle::ConstantTimeEq` with no early exit. `/v1/health` sits behind `require_auth` (`401 ERR_UNAUTHORIZED`). Live TCP test covers both loopback + `X-Forwarded-For` and non-loopback spoofing.

### Capacity, Concurrency & Rollback (`crates/taildrive-server/src/drive.rs`, `db.rs`)
- **Pass (Slice 1 P1 resolved):** `reserve_file`, `reserve_part`, and `init_upload` hold `tokio::sync::Mutex` (`reservations`) across SQLite checks, async `disk().await`, and slot registration; `Weak<Reservation>` auto-releases on client disconnect/drop.
- **Pass:** `publish` runs in a `TaskTracker`-spawned task (shielded from request cancellation, drained on shutdown), re-checks disk floor, quota, and `If-Match` at commit time inside the mutex + SQLite transaction, and rolls back failed commits via `BlobRollback` drop-guard with SQLite `garbage` retry fallback.

### Storage Backends (`crates/taildrive-server/src/storage/{mod,local,s3}.rs`)
- **Pass:** `Spool` + `StagedFile` drop-guard unlinks temp files on mismatch, error, or disconnect (verified live on TCP FIN/RST), aborts streaming immediately if body exceeds `expected_size`, and flushes before crediting `disk_remaining`.
- **Pass:** `LocalFsStorage` uses off-loop `tokio::fs` + `spawn_blocking` `nix::sys::statvfs::statvfs`, atomic `rename`, and pre-header blob open/size checks. `S3Storage` uses OpenDAL `0.59.5` (path-style default for Garage, 8 MiB streaming multipart writer with `writer.abort()` on error, `disk_free_bytes() -> Ok(None)`, first-chunk/stat verification before `200 OK`).

### Routes, Resumables, Shares & Static UI (`api.rs`, `lib.rs`, `content.rs`)
- **Pass:** All `/v1` errors emit `{ error: { code, message, retryable, details } }` including `411 ERR_LENGTH_REQUIRED`, `409 ERR_NOT_EMPTY`, `416 ERR_RANGE_NOT_SATISFIABLE` (`Content-Range: bytes */<size>`).
- **Pass (Resumable Uploads):** `/v1/uploads` persists sessions/parts in SQLite, reserves pending growth in `pending_bytes`, serializes per-upload ops with striped `RwLock`, and cleans retired/aborted part blobs via `garbage`.
- **Pass (Shares):** `/v1/shares` issues 256-bit CSPRNG URL-safe tokens; `GET /s/{token}` is read-only (`405` on writes), sets `Cache-Control: private, no-store`, `Content-Disposition: attachment`, `X-Content-Type-Options: nosniff`, and enforces ms expiry, revocation, and `ON DELETE CASCADE` on file deletion.
- **Pass (Static `web_dir` Traversal):** `lib.rs` mounts `tower_http::services::ServeDir::new(dir)` as outer fallback while `/v1` retains its authenticated `not_found` fallback. Audited `tower-http 0.7.1` (`ServeVariant::build_and_validate_path`): percent-decodes paths and rejects `..`, `/`, and prefix components.

## 3. Non-Blocking P2 Observations (Follow-Up)
1. **Crash orphan sweep (`drive.rs:87-109`):** `Drive::new` runs DB `collect_garbage()` on startup but does not sweep stale `data_dir/tmp/` files left by a hard `SIGKILL`/power loss.
2. **S3 download lock contention (`drive.rs:580`):** `Drive::open` holds `reservations` mutex across `storage.open()`, which awaits S3 first-byte RTT on remote backends.
3. **Minor gaps to track for Phase 2:** Add a live test asserting `ServeDir` rejects `/../` and `/%2e%2e/`; note `complete()` on `local-fs` briefly needs `2x file.size` above the 5 GB floor before part GC; note `rusqlite` + `ServeDir` are used now vs `sqlx` + `rust-embed` noted in `PROJECT_BRIEF.md`.

## 4. Blockers
- **P0:** None.
- **P1:** None.

**Verdict: PASS**

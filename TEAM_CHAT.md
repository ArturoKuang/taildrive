# TailDrive Team Thread (`@lead`, `@design`, `@coder`, `@qa`)

## [1] `@design` (via `agy-design` / `antigravity_cli`)
- Created `docs/design-v1.md`.
- `@coder`: Implement the `/v1` Slice 1 endpoints (`health`, files list/PUT/content/DELETE, `folders`) and `StorageAdapter` (local-fs atomic stage-and-hash + SQLite metadata) per `## Handoff to @coder`, enforcing the 5 GB dev quota + 5 GB SSD free-space floor.
- `@coder`: Return the standard `{ error: { code, message, retryable, details } }` envelope on all failures and require `X-Content-SHA256` on `PUT /v1/files` so Web/macOS/Android can drive deterministic retry and mismatch UX.
- `@qa`: Please build Slice 1 adversarial tests against the non-happy states in Section 3 — specifically path traversal (400), loopback-only `Tailscale-User-Login` enforcement (401), 5 GB quota & disk-floor ceilings (507), checksum mismatch cleanup (422), and `If-Match` conflict (409).
- Flag any contract friction before coding UI shells; I'm standing by to refine tokens or edge states as Slice 1 lands.

## [2] `@coder` (via `claude-coder` / `claude_code`)
- `@qa` `@design`: Slice 1 backend is built in `server/` (Python 3.11+, `uv` + FastAPI + stdlib SQLite) with pluggable `StorageAdapter` (`local_fs` + `s3` stub) and standard error envelope on all `/v1` endpoints; `uv run taildrive-server` binds `127.0.0.1:8787` for `tailscale serve`.
- Files: `server/{pyproject.toml,uv.lock,.gitignore}`, `server/src/taildrive/{__init__,__main__,app,auth,config,drive,errors,metadata,paths}.py`, `server/src/taildrive/storage/{__init__,base,local_fs,s3}.py`, `server/tests/{conftest,test_api,test_local_fs}.py`.
- Verify: `cd server && uv run pytest` → `55 passed in 0.41s`, plus live `curl` checks (20 MB upload/download SHA-256, 422 temp cleanup, 400 traversal, 401 auth, 5 GB quota + 5 GB floor) and auth mutation check.
- Contract notes: `/v1/health` requires auth (unauthenticated probe gets 401); missing `Content-Length` is `411 ERR_LENGTH_REQUIRED`; length mismatch reuses `422 ERR_CHECKSUM_MISMATCH`; paths are NFC-normalized; stale `If-Match` returns `409`.
- `@qa`: please attack concurrent uploads near the quota limit and client disconnects mid-PUT. `@design`: new codes for UI states are `ERR_NOT_EMPTY` (409, folder delete needs `recursive=true`) and `ERR_RANGE_NOT_SATISFIABLE` (416).

## [3] `@qa` (via `agy-qa` / `antigravity_cli` — cross-model audit of `@coder`'s Claude build)
- **Verdict: `CHANGES REQUESTED`** (full report in `docs/qa-slice1.md`; `cd server && uv run pytest -vv` → `55 passed in 0.39s`).
- **P0 Blocker (`@coder`):** `uvicorn.run()` defaults to `proxy_headers=True`, so real `tailscale serve` requests carrying `X-Forwarded-For` from `127.0.0.1` get rewritten to the remote Tailscale IP and rejected with `401`. Fix with `proxy_headers=False` + live `uvicorn` test.
- **P1 Required (`@coder`):** Async quota TOCTOU race in `drive.py` when `disk_free_bytes()` awaits (6x400 B staged concurrently under 1000 B quota), missing commit-time `free_floor_bytes` re-check, and unshielded rollback `storage.delete(key)`.
- **P2 Polish (`@coder`):** Catch `ClientDisconnect` cleanly in `put_file`, check blob existence before `StreamingResponse` sends `200 OK`, and add `lifespan`/`aclose()` + off-loop file I/O.
- **Contract sync (`@design` `@lead`):** Align `docs/design-v1.md` on `StorageAdapter` (SQLite tracks usage instead of `stat()`/`usage()`), authenticated `/v1/health`, and error codes `411`/`409 ERR_NOT_EMPTY`/`416`.

## [4] `@coder` (via `codex` / `gpt-6.2-sol`) → `@qa` (via `agy`) — Phase 1 Rust Workspace
- Replaced Python prototype with Rust Cargo workspace (`crates/taildrive-core`, `crates/taildrive-server`) implementing all `/v1` file/folder/resumable-upload/share-link endpoints, `ConnectInfo<SocketAddr>` loopback auth, atomic async quota+floor reservations, `LocalFsStorage`, and OpenDAL `S3Storage` (verified against Garage 2.5.0).
- `@qa` independent audit (`docs/qa-phase1-rust.md`): **`PASS`** (`24/24` tests, `0` clippy warnings, `unsafe_code = "forbid"`, 0 P0/P1 blockers).

## [5] `@design` (`agy`) → `@coder` (`codex`) — Phase 2 & 3 (Garage S3 + Web UI)
- Deployed **Garage 2.5.0** (`com.local.garage`) and release `taildrive-server` (`com.local.taildrive-server`, `storage_driver=s3`) as persistent LaunchAgents on `macmini-ci`; verified live S3 upload/download/delete roundtrip.
- Built and verified the React + TypeScript Web UI (`web/dist`, served directly at `http://127.0.0.1:8787/`) with 0 TS/Vite errors and passing live browser smoke tests covering auto-auth, Bearer login, 422 re-hash, 503 multipart resume, previews, share links, and offline cache.

## [6] `@coder` (`codex`) — Phase 4A (UniFFI Rust Core + Native macOS SwiftUI Client)
- Added `crates/taildrive-ffi` (`staticlib` + `cdylib` + workspace `uniffi-bindgen`) generating both Swift and Kotlin bindings from one Rust sync/hash/API core.
- Built native macOS 14+ SwiftUI menu-bar client (`clients/macos/`) linked statically to `libtaildrive_ffi.a`.
- Verified: `cargo test --workspace` (24/24), strict `cargo clippy -- -D warnings` (0 warnings), `swift build && swift test` (6/6 tests exercising live Rust FFI transfers, cancellation, and `If-Match` 409 conflict preservation).

## [7] `@coder` (`codex`) — Phase 4B (Native Android Kotlin + Compose + WorkManager Client)
- Built `clients/android/` (Kotlin + Jetpack Compose + Material 3 + WorkManager + Tailscale VPN detector + Keystore Bearer auth + UniFFI adapter & OkHttp client + `scripts/build-android-rust.sh`).
- Verified: `gradle testDebugUnitTest assembleDebug` passed and built `clients/android/app/build/outputs/apk/debug/app-debug.apk` (17 MB); Rust workspace tests + strict Clippy + Swift FFI tests all remain green.

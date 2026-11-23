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

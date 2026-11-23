# TailDrive Slice 1 — Independent QA Audit

**Verdict:** `CHANGES REQUESTED`
**Scope:** `server/` against [design-v1.md](file:///Users/arturokuang/Projects/taildrive/docs/design-v1.md) and [PROJECT_BRIEF.md](file:///Users/arturokuang/Projects/taildrive/PROJECT_BRIEF.md)

## 1. Automated Verification Run
- `cd /Users/arturokuang/Projects/taildrive/server && uv run pytest -vv` → **`55 passed in 0.39s`** (Python 3.11.15, pytest 9.2.0).
- Live adversarial harness run against real `uvicorn` (raw TCP sockets) + async `Drive` concurrency harness; findings below are empirically reproduced, not inferred.

## 2. Findings

### [P0 — Blocker] `tailscale serve` Auth Breaks Under Default Uvicorn `proxy_headers=True` (and Risks Spoofing)
- **Where:** `server/src/taildrive/__main__.py:12-16`, `server/src/taildrive/auth.py:25-30,42-48`.
- **Evidence (live `uvicorn` repro):** `uvicorn.run()` defaults to `proxy_headers=True` with `forwarded_allow_ips='127.0.0.1,::1'`. When `tailscale serve` proxies from `127.0.0.1`, it injects `X-Forwarded-For: 100.64.0.7` + `Tailscale-User-Login: alice@ts.net`. Uvicorn's `ProxyHeadersMiddleware` rewrites `request.client.host` to `100.64.0.7` before `authenticate()` runs, returning **`401 ERR_UNAUTHORIZED`** on every real `tailscale serve` request. Conversely, if `FORWARDED_ALLOW_IPS` is widened behind a proxy, a remote peer sending `X-Forwarded-For: 127.0.0.1` can spoof loopback and forge `Tailscale-User-Login`.
- **Fix:** Pass `proxy_headers=False` to `uvicorn.run(...)` so `request.client.host` always reflects the true TCP peer. Add a live `uvicorn` test covering `127.0.0.1` + `X-Forwarded-For: 100.64.0.7` + `Tailscale-User-Login`. (Also note: `::ffff:127.0.0.1` is rejected by `ipaddress.is_loopback` if ever dual-stack bound.)

### [P1 — Required] Async Quota TOCTOU Race & Missing Commit-Time Disk Floor Re-check
- **Where:** `server/src/taildrive/drive.py:99-105,122-135,182-197`.
- **Evidence (live async repro):** `_check_capacity()` checks `quota_bytes` *before* `await self.storage.disk_free_bytes()`, while `_reserved_quota` and `_reserved_disk` are incremented *after* that `await`. `LocalFsStorageAdapter` masks this today only because `disk_free_bytes()` has no internal `await`. With an async `disk_free_bytes()` (required for S3/SeaweedFS/Garage or threaded `statfs`), 6 concurrent 400 B uploads against a 1000 B quota all passed pre-flight and staged **2400 B** concurrently before 4 failed at commit.
- **Related gaps:** (a) `free_floor_bytes` is never re-checked before commit (external disk pressure on 33 GB `macmini-ci` during a long upload can commit below the 5 GB floor); (b) rollback `await self.storage.delete(key)` at `drive.py:133` is unshielded against task cancellation (leaks orphan S3 objects on disconnect/shutdown).
- **Fix:** Hold an `asyncio.Lock` around `_check_capacity()` + reservation (or re-check quota & floor atomically after `await` before incrementing), re-check `free_floor_bytes` before commit, and shield rollback `storage.delete(key)`. Add an async concurrent quota test.

### [P2 — Polish / Follow-up]
- **Mid-PUT disconnect logging (`app.py:89`):** Live TCP RST and FIN mid-PUT cleanly removed `tmp/` staging files, left 0 orphan blobs, and released `_reserved_quota`/`_reserved_disk` (verified by an immediate full-quota `201 Created` upload). However, `starlette.requests.ClientDisconnect` escapes to Uvicorn as an unhandled `ERROR: Exception in ASGI application` traceback. Catch `ClientDisconnect` cleanly in `put_file`.
- **Missing blob returns `200 OK` (`app.py:113-119`, `local_fs.py:72-82`):** `StreamingResponse` starts sending `200 OK` before `storage.get()` opens the file; a missing blob fails mid-stream with an empty `200` instead of a JSON error envelope.
- **Event loop blocking & lifecycle (`local_fs.py:42-56`, `app.py:33`, `metadata.py:60`):** `LocalFsStorageAdapter.put()` runs synchronous `f.write()` + `os.fsync()` on the event loop thread (stalls concurrent health/list/downloads on large files). `StorageAdapter` / `create_app()` also lack a FastAPI `lifespan` / `aclose()` hook needed for S3 HTTP client pools and closing SQLite cleanly.

## 3. Verified Passing Surfaces
- **Path Traversal & Unicode NFC:** `parse_path()` blocks `..`, leading `/`, `\`, NUL/control chars, empty segments, `>1024 B`; blobs use server UUID keys (zero user-path filesystem exposure). Verified NFC/NFD parity, 0-byte `PUT`/`GET`/`Range` (`416`), and multi-byte/emoji recursive `DELETE` (`café`, `🎉`, `🎉_keep.txt`).
- **Mid-PUT Disconnect Cleanup:** Verified live over TCP RST + FIN against `uvicorn`: `tmp/` empty, `blobs/` empty, quota reservation fully released.
- **LocalFS Concurrent Quota (sync `disk_free_bytes`):** 8x400 B concurrent uploads under 1000 B quota → exactly 2 `ok` (800 B used), 6 `ERR_QUOTA_EXCEEDED`, 0 temp/orphan blobs.
- **Error Envelope & Integrity:** Constant-time token compare, `411 ERR_LENGTH_REQUIRED` on chunked PUT without `Content-Length`, `422 ERR_CHECKSUM_MISMATCH` temp cleanup, `409 ERR_CONFLICT` on `If-Match`.

## 4. Contract Sync for `@design`
- `StorageAdapter` omits `stat()` and `usage()` from `design-v1.md` (SQLite tracks `used_bytes` and stats — better for S3 O(1) vs O(N) listing); align `design-v1.md`.
- Confirm `/v1/health` requiring auth (`401` unauthenticated) fits macOS onboarding probe, and record `411 ERR_LENGTH_REQUIRED`, `409 ERR_NOT_EMPTY`, `416 ERR_RANGE_NOT_SATISFIABLE` in `design-v1.md`.
- Note for `s3.py` plan: S3 `CopyObject` caps at 5 GB and uncommitted multipart uploads are already invisible at `key` (staging-key copy unnecessary).

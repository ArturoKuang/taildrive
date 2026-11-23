# TailDrive Team Thread (`@lead`, `@design`, `@coder`, `@qa`)

## [1] `@design` (via `agy-design` / `antigravity_cli`)
- Created `docs/design-v1.md`.
- `@coder`: Implement the `/v1` Slice 1 endpoints (`health`, files list/PUT/content/DELETE, `folders`) and `StorageAdapter` (local-fs atomic stage-and-hash + SQLite metadata) per `## Handoff to @coder`, enforcing the 5 GB dev quota + 5 GB SSD free-space floor.
- `@coder`: Return the standard `{ error: { code, message, retryable, details } }` envelope on all failures and require `X-Content-SHA256` on `PUT /v1/files` so Web/macOS/Android can drive deterministic retry and mismatch UX.
- `@qa`: Please build Slice 1 adversarial tests against the non-happy states in Section 3 — specifically path traversal (400), loopback-only `Tailscale-User-Login` enforcement (401), 5 GB quota & disk-floor ceilings (507), checksum mismatch cleanup (422), and `If-Match` conflict (409).
- Flag any contract friction before coding UI shells; I'm standing by to refine tokens or edge states as Slice 1 lands.

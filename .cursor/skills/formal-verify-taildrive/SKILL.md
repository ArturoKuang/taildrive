---
name: formal-verify-taildrive
description: Run and maintain TailDrive's formal verification suite — TLA+/TLC models with negative canary kill proofs (ReaderSyncProtocol, TailDriveSync3Way, TailDriveQuotaStorage), Rust property/refinement harnesses, static analysis, and spec-to-code drift checks. Use whenever changing Reader sync rules/store/blobs, 3-way folder sync planning or macOS SyncWorker, or TailDrive server quota/reservations/resumable uploads/GC.
---

# Formal Verification Skill: `taildrive`

Synchronized to commit **`fc03a50`** (`fc03a502963361a4b31cf9e0aaa0e559da942be2`).
Manifest source of truth: `formal/manifest.json` (pinned `tla2tools.jar` SHA-256 `18e0d9caa517a6553ef50c7105191b1bfcfe5cc630bdbceefc087093e0520ce3`).

---

## 1. Coverage Map (`sync_commit: fc03a50`)

### TLA+ Protocol & Concurrency Models (Gate G1)

| Model | Spec & Configs | Covered Source Files | Covered Symbols | Canary (`BUG_MODE`) -> Expected Invariant |
| --- | --- | --- | --- | --- |
| `ReaderSyncProtocol` | `formal/tla/ReaderSyncProtocol.{tla,cfg,_Deep.cfg,_Canary.cfg}` | `docs/reader-sync/SYNC_SPEC.md`, `crates/taildrive-server/src/reader/wire.rs`, `crates/taildrive-server/src/reader/store.rs`, `crates/taildrive-server/src/reader/blobs.rs` | `wire::{same, decide, position_rejection, decide_position}`, `store::{push, apply, apply_position, apply_record, write_head, write_new, open_conflict, append_side, pull, resolve, register_book}`, `blobs::Blobs::put` | `skip_conflict_check` -> `NoSilentOverwrite` |
| `TailDriveSync3Way` | `formal/tla/TailDriveSync3Way.{tla,cfg,_Deep.cfg,_Canary.cfg}` | `crates/taildrive-ffi/src/sync.rs`, `clients/macos/Sources/TailDriveSyncKit/SyncWorker.swift`, `clients/macos/Sources/TailDriveSyncKit/LocalMirror.swift` | `plan_folder_sync`, `SyncEntry`, `SyncAction`, `SyncWorker::{sync, apply, saveConflict}`, `Checkpoint`, `LocalMirror::{preserveConflict, delete, validate}` | `drop_keep_folder_guard` -> `NonEmptyFolderProtected` |
| `TailDriveQuotaStorage` | `formal/tla/TailDriveQuotaStorage.{tla,cfg,_Deep.cfg,_Canary.cfg}` | `crates/taildrive-server/src/drive.rs`, `crates/taildrive-server/src/db.rs`, `crates/taildrive-server/src/storage/mod.rs` | `Drive::{put, reserve_file, reserve_part, init_upload, put_part, complete, commit_file, publish, abort, delete, open_file, collect_garbage}`, `Reserved::collect`, `BlobRollback`, `db::{check_target, write_file, delete_tree, used_bytes, pending_bytes, record_part, remove_upload, enqueue_garbage, forget_garbage}`, `Spool::stage` | `skip_commit_quota_recheck` -> `QuotaNeverExceeded` |

### Rust Code-Level Formal Harnesses (Gate G2)

| Harness | Covered Source Files | Scope |
| --- | --- | --- |
| `crates/taildrive-core/tests/formal_properties.rs` | `crates/taildrive-core/src/path.rs`, `crates/taildrive-core/src/hash.rs` | Path canonicalization (NFC/NFD equivalence, traversal/control-char rejection, conflict-path round-trip bounds) and SHA-256 determinism. |
| `crates/taildrive-ffi/tests/formal_sync_3way.rs` | `crates/taildrive-ffi/src/sync.rs` | Stateful refinement & property tests of `plan_folder_sync` across randomized nested trees, deletes, kind conflicts, and convergence. |
| `crates/taildrive-server/tests/formal_reader_sync.rs` | `crates/taildrive-server/src/reader/{wire.rs,store.rs,blobs.rs}` | Exhaustive §6 merge matrix, word-id fixture, idempotency, crash-before-commit rollback, register rules 1–3, and `disk_full` isolation against real SQLite `reader.db`. |
| `crates/taildrive-server/tests/formal_drive_quota.rs` | `crates/taildrive-server/src/{drive.rs,db.rs,storage/mod.rs}` | Concurrent `put`, resumable uploads, `delete`, `open_file`, `If-Match` CAS, `BlobRollback`, disk-floor, and GC invariants against real `Drive` + SQLite `taildrive.db`. |

---

## 2. Exact Commands

### Full Unified Verification Harness (`G0 -> G1 -> G2 -> G3`)

`./scripts/formal-verify.sh` runs all four gates (G0 static analysis + code-quality ratchet + pinned `tla2tools.jar` SHA-256 check, G1 TLC standard/deep + mandatory canary kill proofs, G2 Rust code harnesses + `docs/reader-sync/tools/validate.py`, G3 anti-cheat & spec-to-code drift guard) and emits `.formal-reports/formal-report.{md,json}`:

```bash
# Standard verification (PR gate)
./scripts/formal-verify.sh

# Deep verification (nightly / pre-release gate)
./scripts/formal-verify.sh --mode deep

# Single TLA+ model + its mandatory canary only
./scripts/formal-verify.sh --model ReaderSyncProtocol --skip-code --skip-static
./scripts/formal-verify.sh --model TailDriveSync3Way --skip-code --skip-static
./scripts/formal-verify.sh --model TailDriveQuotaStorage --skip-code --skip-static

# Install local git hooks (pre-commit / pre-push drift & static guards)
./scripts/install-hooks.sh
```

### Direct TLC Invocation (Single Model or Canary Debugging)

```bash
JAVA=/opt/homebrew/opt/openjdk@21/bin/java
JAR="$HOME/ci/tools/tla2tools.jar"
META=$(mktemp -d /tmp/tlc-td-XXXXXX)

# Example: standard run (MUST exit 0, zero errors, deadlock checking enabled)
"$JAVA" -XX:+UseParallelGC -Xmx2g -cp "$JAR" tlc2.TLC \
  -noGenerateSpecTE -workers 2 -cleanup -metadir "$META" \
  -config formal/tla/ReaderSyncProtocol.cfg formal/tla/ReaderSyncProtocol.tla

# Example: canary run (MUST exit non-zero and report the expected violated invariant)
"$JAVA" -XX:+UseParallelGC -Xmx2g -cp "$JAR" tlc2.TLC \
  -noGenerateSpecTE -workers 2 -cleanup -metadir "$META" \
  -config formal/tla/ReaderSyncProtocol_Canary.cfg formal/tla/ReaderSyncProtocol.tla

rm -rf "$META"
```

### Drift & Anti-Cheat Guards

```bash
./scripts/check-formal-drift.sh            # Advisory drift check vs sync_commit
./scripts/check-formal-drift.sh --strict   # Exit 2 on drift
./scripts/verify-harness-guard.sh          # Manifest integrity & anti-cheat scan
```

---

## 3. Properties Verified & Harms Prevented

### `ReaderSyncProtocol`
- **`SeqStrictlyMonotonic` & `SeqNeverDecreases`**: Global `meta.seq` never regresses, `record_versions` seqs strictly increase with version, and every live record holds a unique `seq`. *Prevents:* offline devices skipping edits or looping pull feeds forever.
- **`IdempotentReplaySound`**: Replaying a committed `batchId`/`changeId` with an identical body returns the stored result without advancing `seq`; reusing an ID with a different body rejects `change_id_reused`; crash-before-commit rolls back atomically so retries apply cleanly once. *Prevents:* duplicate highlights/notes or phantom conflicts under mobile network retries.
- **`PositionNeverConflicts`**: `kind: position` enforces owner-only writes, rejects deletes (`bad_request`), ignores `baseVersion`, fast-forwards on new payloads, and never opens a conflict. *Prevents:* spurious conflict badges on reading progress or cross-device position overwrites.
- **`IdenticalContentNeverConflicts`**: Pushing content canonically equal to head or an open side is always a `noop`; every open conflict retains at least two canonically distinct sides. *Prevents:* redundant "2 versions" conflicts when two devices make identical edits or deletes.
- **`NoSilentOverwrite`**: Every concurrent divergent edit (`baseVersion < head.version`) stays visible as either `head` or a side in `conflict_sides` until explicitly fast-forwarded or resolved. *Prevents:* silent loss of offline annotations.
- **`BookRegisterDeterministic`**: `register_book` Rules 1 (`existing`), 2 (`attached` on matching `(format, docId)`), and 3 (`created`) execute atomically and never merge distinct formats. *Prevents:* split highlight histories after re-saving a PDF or cross-format corruption.
- **`DiskFullNeverBlocksRecords`**: Free-disk floor enforcement (`507 disk_full`) gates only book blob uploads (`Blobs::put`); record pushes, positions, resolves, and registers continue unimpeded. *Prevents:* note/highlight sync outages when large book uploads pause near the disk floor.

### `TailDriveSync3Way`
- **`TreeWellFormed`**: Every file/folder in `local` and `remote` always has a present folder parent (guaranteed by top-down forward emission and bottom-up delete emission). *Prevents:* orphaned paths or failed writes into missing directories.
- **`NoSilentEditLoss`**: Every active local or remote user edit survives at its path, its conflict sibling, or under a moved kind-conflict folder (`a_c/*`), including across mid-sync `If-Match` 409 upload races. *Prevents:* silent loss of edits during concurrent modification or folder deletion.
- **`NoResurrectionOfCleanDelete`**: Files cleanly deleted on one side while unchanged on the other are never re-uploaded/re-downloaded and stay absent once converged. *Prevents:* deleted files reappearing as zombies.
- **`NonEmptyFolderProtected`**: `plan_folder_sync` never schedules `DeleteRemote` or `DeleteLocal` on a folder that retains any non-deleted descendant (`keep_remote_folders` / `keep_local_folders`). *Prevents:* wiping out newly added/edited files inside a folder deleted on the other side.
- **`EventualConvergence` (liveness)**: Under weak fairness of sync steps, once bounded user edits quiesce, `local`, `remote`, and `baseline` converge permanently. *Prevents:* infinite rescan loops or stuck divergence.

### `TailDriveQuotaStorage`
- **`QuotaNeverExceeded`**: Committed live bytes (`usage.used_bytes`) never exceed `quota_limit`, even under concurrent overwrite-vs-delete-vs-upload interleavings. *Prevents:* quota overruns bypassing initial admission checks.
- **`UsageMatchesLiveFiles`**: SQLite trigger-maintained `usage.used_bytes` always equals `SUM(files.size_bytes)`. *Prevents:* permanent quota accounting drift.
- **`DiskFloorRespected`**: Actual free disk (counting both published blobs and staged spool files) never dips below `free_floor_bytes`. *Prevents:* disk exhaustion breaking macOS or SQLite WAL durability.
- **`NoDanglingLiveBlob` & `NoOrphanedBlob`**: Every live file and live upload part references a present non-garbage blob (`open_file` never sees a deleted blob), and every stored blob is accounted for by a live file, live part, active `BlobRollback`, or `garbage`. *Prevents:* broken downloads (`404`/`500`) and silent disk leaks.
- **`GarbageEventuallyReclaimed` (liveness)**: Under weak fairness of `CollectGarbage` and `BlobRollback`, every retired/rolled-back blob in `garbage` is eventually deleted from storage and removed from SQLite. *Prevents:* unbounded retired-blob accumulation starving disk headroom.

---

## 4. Bounds, Assumptions, and Non-Goals

- **Finite bounds**: TLC checks all interleavings exhaustively within the bounded constants in `formal/manifest.json` (2 devices / 2 workers / 2–4 paths / 3–5 seqs or blob IDs). Safety invariants hold for every reachable state within bounds; larger combinatorics are complemented by G2 randomized Rust property/refinement tests (`PROPTEST_CASES`, `FORMAL_CASES`).
- **Atomic boundary assumptions**: SQLite transactions (`Store::conn`, `Db`) are assumed ACID/serializable (`WAL`, `synchronous=FULL`); `LocalFsStorage::publish` rename is assumed atomic with net zero additional disk bytes; external non-TailDrive host processes are assumed not to consume disk space concurrently inside `TailDriveQuotaStorage`.
- **Explicit non-goals**:
  - TLS / Tailscale identity middleware, bearer token cryptography, pairing QR expiry, and redeem IP rate limiting.
  - Byte-level HTTP `Range` framing, S3 backend eventual consistency, and APFS symlink/inode caching.
  - Reader Markdown projection (`markdown-mapping.md`) and Swift/JS Unicode word-id normalization (verified separately via `normalize.json` fixture tests).

---

## 5. Triaging Failures: Real Bug vs. Spec Drift

When TLC or a Rust formal harness fails:

1. **Do NOT weaken invariants, drop canaries, or add `#[ignore]`** (Gate G3 `verify-harness-guard.sh` explicitly blocks anti-cheat shortcuts).
2. **Read the counterexample trace in code terms**: map every TLA+ action in the shortest counterexample back to its exact Rust/Swift function and mutex/transaction boundary listed in the Coverage Map above.
3. **Classify the root cause**:
   - **Real product bug**: the counterexample trace is reproducible in the actual Rust/Swift code (for example, an unguarded TOCTOU window, a missing commit-time re-check, or an unconditional `DELETE` racing with a concurrent edit). Fix the product code and add a regression test in the matching G2 harness.
   - **Spec drift**: the product code intentionally changed its contract or atomic boundary since `sync_commit` (check `./scripts/check-formal-drift.sh`), so the TLA+ model no longer matches the real implementation.
4. **Known real-code edge cases audited at `fc03a50`** (do not mask these in models):
   - `SyncWorker.swift:260-265` + `api.rs:310-316` + `db.rs:317`: `DeleteRemote` sends an unconditional `DELETE /v1/files` without an `If-Match` precondition; a concurrent remote edit racing between `remoteSnapshot` and `deletePath` can be silently deleted (reproducible in `TailDriveSync3Way.tla` via `BUG_MODE = "unconditional_delete_race"`).
   - `SyncWorker.swift:287-296` + `api.rs:281`: New-file `Upload` (`current == nil`) checks `remoteItem(path) != nil` then uploads with `ifMatch: nil` (no `If-None-Match: *`), leaving a TOCTOU overwrite window if another client creates the same path concurrently.
   - `reader/blobs.rs:245-274`: `Blobs::put` locks only per-SHA and checks `free_bytes` / `reader_book_quota_bytes` without global in-flight reservations or a commit-time re-check in `finish()`, so two concurrent uploads of distinct SHAs can both pass admission and exceed the optional book quota or dip below `free_floor_bytes`.
   - `reader/store.rs:673-677`: Fast-forwarding `head` while a conflict is open leaves `conflict_sides` unchanged, so the new `head` is absent from `conflict.sides` while the superseded head remains in side 0 (clients resolving conflicts must inspect `envelope.payload` alongside `conflict.sides`).

---

## 6. Maintenance Loop

Whenever a PR modifies any file listed in `formal/manifest.json` (`covered_files`):

1. Run `./scripts/check-formal-drift.sh` to see which models are affected.
2. Audit each affected model's actions and atomic boundaries against the real implementation diff (or invoke `/maintain-formal-verification-skill` to guide the sync update).
3. Update the TLA+ spec, configs, Rust formal harnesses, and this `SKILL.md` if protocol semantics or boundaries changed.
4. Run `./scripts/formal-verify.sh` (standard) and `./scripts/formal-verify.sh --mode deep` — verify every model passes, every canary is killed on its expected invariant, and G0/G2/G3 are green.
5. Bump `sync_commit` in `formal/manifest.json` and the header of this `SKILL.md` to the new verified commit SHA.

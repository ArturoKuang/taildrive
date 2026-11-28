# TailDrive Formal Verification Suite (`formal/`)

This directory holds TailDrive's TLA+/TLC formal models, finite model-checking configurations (standard, deep, and negative canary kill proofs), and the machine-readable verification manifest (`manifest.json`) synchronized to commit `fc03a50` (`fc03a502963361a4b31cf9e0aaa0e559da942be2`).

## Models

| Model | Spec & Configs | Covered Implementation | Key Verified Properties | Canary Defect (`BUG_MODE`) -> Expected Violation |
| --- | --- | --- | --- | --- |
| `ReaderSyncProtocol` | `formal/tla/ReaderSyncProtocol.{tla,cfg,_Deep.cfg,_Canary.cfg}` | `crates/taildrive-server/src/reader/{wire.rs,store.rs,blobs.rs}`, `docs/reader-sync/SYNC_SPEC.md` | `SeqStrictlyMonotonic`, `IdempotentReplaySound`, `PositionNeverConflicts`, `IdenticalContentNeverConflicts`, `NoSilentOverwrite`, `BookRegisterDeterministic`, `DiskFullNeverBlocksRecords`, `SeqNeverDecreases` | `skip_conflict_check` -> `NoSilentOverwrite` |
| `TailDriveSync3Way` | `formal/tla/TailDriveSync3Way.{tla,cfg,_Deep.cfg,_Canary.cfg}` | `crates/taildrive-ffi/src/sync.rs`, `clients/macos/Sources/TailDriveSyncKit/{SyncWorker.swift,LocalMirror.swift}` | `TreeWellFormed`, `NoSilentEditLoss`, `NoResurrectionOfCleanDelete`, `NonEmptyFolderProtected`, `EventualConvergence` | `drop_keep_folder_guard` -> `NonEmptyFolderProtected` |
| `TailDriveQuotaStorage` | `formal/tla/TailDriveQuotaStorage.{tla,cfg,_Deep.cfg,_Canary.cfg}` | `crates/taildrive-server/src/{drive.rs,db.rs,storage/mod.rs}` | `QuotaNeverExceeded`, `UsageMatchesLiveFiles`, `DiskFloorRespected`, `NoDanglingLiveBlob`, `NoOrphanedBlob`, `GarbageEventuallyReclaimed` | `skip_commit_quota_recheck` -> `QuotaNeverExceeded` |

## Bounds Summary

- **`ReaderSyncProtocol`**:
  - Standard: 2 devices, 2 payloads (+ tombstone), 3 book SHAs (`pdf+doc1`, `pdf+doc1`, `epub+doc1`), `MaxSeq=4`, `MaxChanges=3`.
  - Deep: 2 devices, 3 payloads (+ tombstone), 4 book SHAs (adds `pdf+none`), `MaxSeq=5`, `MaxChanges=3`.
- **`TailDriveSync3Way`**:
  - Standard: base paths `{"a", "a/f", "b"}` + conflict sibling slots, 2 content values, `MaxEdits=2` (3 synced initial trees).
  - Deep: base paths `{"a", "a/f", "a/g", "b"}` + conflict sibling slots, 3 content values, `MaxEdits=2`.
- **`TailDriveQuotaStorage`**:
  - Standard: 2 paths, 2 concurrent workers, sizes `{2, 3}`, `QuotaLimit=4`, `DiskTotal=12`, `DiskFloor=4`, `MaxBlobId=3`.
  - Deep: 2 paths, 2 concurrent workers, sizes `{1, 2, 3}`, `QuotaLimit=4`, `DiskTotal=10`, `DiskFloor=4`, `MaxBlobId=3`.

## Running Verification

Run the unified verification harness (G0 static analysis & code-quality ratchet -> G1 TLA+/TLC + mandatory canary kill proofs -> G2 Rust property/refinement harnesses + Reader sync OpenAPI/schema validation -> G3 anti-cheat & spec-to-code drift guard):

```bash
# Standard PR gate (fast, ~10-20s for all TLC models + canaries)
./scripts/formal-verify.sh

# Deep nightly gate (wider bounds + 1000 proptest/formal cases)
./scripts/formal-verify.sh --mode deep

# Single TLA+ model + its canary only (skip Rust tests & static analysis)
./scripts/formal-verify.sh --model ReaderSyncProtocol --skip-code --skip-static

# Check spec-to-code drift vs manifest sync_commit
./scripts/check-formal-drift.sh

# Install local git pre-commit/pre-push hooks
./scripts/install-hooks.sh
```

Reports are written to `.formal-reports/formal-report.md` and `.formal-reports/formal-report.json`.

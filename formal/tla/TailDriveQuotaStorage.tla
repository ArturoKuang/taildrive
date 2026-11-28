------------------------- MODULE TailDriveQuotaStorage -------------------------
(*
 * Formal TLA+ specification of TailDrive's concurrent quota reservation,
 * free-disk floor enforcement, resumable upload lifecycle, If-Match CAS,
 * BlobRollback, and asynchronous garbage collection.
 *
 * Faithful to implementation files (HEAD fc03a50):
 *   - crates/taildrive-server/src/drive.rs:
 *       Drive::put, Drive::reserve_file, Drive::reserve_part,
 *       Drive::init_upload, Drive::put_part, Drive::complete,
 *       Drive::commit_file, Drive::publish, Drive::abort, Drive::delete,
 *       Drive::open_file (Drive::open), Drive::collect_garbage,
 *       Reserved::collect, BlobRollback
 *   - crates/taildrive-server/src/db.rs:
 *       check_target, write_file, delete_tree, used_bytes, pending_bytes,
 *       insert_upload, record_part, remove_upload, enqueue_garbage,
 *       garbage, forget_garbage, and SQLite usage triggers
 *   - crates/taildrive-server/src/storage/mod.rs:
 *       Spool::stage (write_verified decrementing disk_remaining)
 *
 * Atomic boundaries modeled:
 *   - Drive::reservations (tokio::sync::Mutex) serializes reservation checks,
 *     metadata commits, deletes, aborts, and open_file lookups.
 *   - Drive::upload_locks (RwLock per upload) allows concurrent put_part
 *     readers while giving complete and abort exclusive writer access.
 *   - SQLite transactions atomically update files, uploads, garbage, and
 *     usage.used_bytes (via AFTER INSERT/UPDATE/DELETE triggers).
 *   - Drive::garbage_lock serializes collect_garbage independently of
 *     Drive::reservations.
 *)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Paths,          \* Set of file paths, e.g. {"p1", "p2"}
    Workers,        \* Set of concurrent request workers, e.g. {"w1", "w2"}
    Sizes,          \* Set of file sizes in abstract blocks, e.g. {2, 3}
    QuotaLimit,     \* Server storage quota (config.quota_bytes)
    DiskTotal,      \* Total backing disk capacity
    DiskFloor,      \* Free-disk floor (config.free_floor_bytes)
    MaxBlobId,      \* Finite bound on server-generated unique blob keys (1..MaxBlobId)
    BUG_MODE        \* "none" | "skip_commit_quota_recheck" | "skip_disk_floor_check"

ASSUME BUG_MODE \in {"none", "skip_commit_quota_recheck", "skip_disk_floor_check"}
ASSUME QuotaLimit > 0 /\ DiskTotal > DiskFloor /\ MaxBlobId >= 1

BlobIds == 1..MaxBlobId
MaxSize == CHOOSE s \in Sizes : \A t \in Sizes : s >= t

(* ------------------------------------------------------------------------- *)
(* Helper operators                                                          *)
(* ------------------------------------------------------------------------- *)

Max(a, b) == IF a >= b THEN a ELSE b

RECURSIVE SumOver(_, _)
SumOver(S, F) ==
    IF S = {} THEN 0
    ELSE LET x == CHOOSE y \in S : TRUE
         IN F[x] + SumOver(S \ {x}, F)

\* Uniform sentinel records (avoids comparing records with non-records in TLC)
NoFile == [present |-> FALSE, key |-> 0, size |-> 0, version |-> 0]
NoUpload == [active |-> FALSE, size |-> 0, partKey |-> 0]
IdleWorker == [
    pc          |-> "idle",
    path        |-> "NONE",
    size        |-> 0,
    ifMatch     |-> 0,      \* 0 means unconditional (no If-Match); >0 means expected version
    blobKey     |-> 0,      \* Freshly allocated unique key for this operation
    resQuota    |-> 0,      \* reservation.quota
    resDisk     |-> 0,      \* reservation.disk_remaining
    stagedBytes |-> 0,      \* Bytes currently spooled on disk in tmp/
    rollbackKey |-> 0       \* BlobRollback.key (0 = disarmed)
]

WorkerPCs == {
    "idle",
    "put_reserved", "put_staged", "put_published",
    "part_staged",
    "complete_staged", "complete_published",
    "rollback"
}

(* ------------------------------------------------------------------------- *)
(* State variables                                                           *)
(* ------------------------------------------------------------------------- *)

VARIABLES
    files,          \* [Paths -> [present, key, size, version]] (SQLite `files` table)
    usedBytes,      \* Nat: SQLite `usage.used_bytes` maintained by triggers
    uploads,        \* [Paths -> [active, size, partKey]] (SQLite `uploads` + `upload_parts`)
    uploadReaders,  \* [Paths -> 0..Cardinality(Workers)]: active put_part read locks
    uploadWriter,   \* [Paths -> Workers \cup {"NONE"}]: active complete write lock
    storage,        \* [BlobIds -> 0..MaxSize]: 0 = absent, >0 = stored blob size
    garbage,        \* SUBSET BlobIds (SQLite `garbage` table)
    nextBlobId,     \* 1..(MaxBlobId + 1): unique blob key allocator (db::id("b"))
    worker,         \* [Workers -> WorkerRecord]
    openError       \* Ghost flag: TRUE iff open_file ever observed a missing/garbage live blob

vars == <<
    files, usedBytes, uploads, uploadReaders, uploadWriter,
    storage, garbage, nextBlobId, worker, openError
>>

(* ------------------------------------------------------------------------- *)
(* Derived quantities matching drive.rs & db.rs                              *)
(* ------------------------------------------------------------------------- *)

CurrentFileSize(F, p) == IF F[p].present THEN F[p].size ELSE 0

SumLiveFileSizes(F) ==
    SumOver(Paths, [p \in Paths |-> CurrentFileSize(F, p)])

\* db::pending_bytes(conn, exclude) (db.rs:365-372)
PendingBytes(F, U, excludePath) ==
    SumOver(Paths, [p \in Paths |->
        IF p = excludePath \/ ~U[p].active THEN 0
        ELSE Max(0, U[p].size - CurrentFileSize(F, p))])

\* Reserved::collect(&mut slots, exclude) (drive.rs:59-77)
ReservedQuota(W, excludeW) ==
    SumOver(Workers, [w \in Workers |-> IF w = excludeW THEN 0 ELSE W[w].resQuota])

ReservedDisk(W, excludeW) ==
    SumOver(Workers, [w \in Workers |-> IF w = excludeW THEN 0 ELSE W[w].resDisk])

TotalStaged(W) ==
    SumOver(Workers, [w \in Workers |-> W[w].stagedBytes])

TotalStored(St) ==
    SumOver(BlobIds, St)

DiskUsed(St, W) == TotalStored(St) + TotalStaged(W)
DiskFree(St, W) == DiskTotal - DiskUsed(St, W)

\* db::check_target If-Match check (db.rs:197-213)
IfMatchOk(fileRec, ifMatch) ==
    ifMatch = 0 \/ (fileRec.present /\ fileRec.version = ifMatch)

\* drive::check_disk (drive.rs:153-166)
CheckDisk(free, reserved, requested) ==
    BUG_MODE = "skip_disk_floor_check" \/ (DiskFloor + reserved + requested <= free)

(* ------------------------------------------------------------------------- *)
(* Initial state                                                             *)
(* ------------------------------------------------------------------------- *)

Init ==
    /\ files = [p \in Paths |-> NoFile]
    /\ usedBytes = 0
    /\ uploads = [p \in Paths |-> NoUpload]
    /\ uploadReaders = [p \in Paths |-> 0]
    /\ uploadWriter = [p \in Paths |-> "NONE"]
    /\ storage = [k \in BlobIds |-> 0]
    /\ garbage = {}
    /\ nextBlobId = 1
    /\ worker = [w \in Workers |-> IdleWorker]
    /\ openError = FALSE

(* ------------------------------------------------------------------------- *)
(* Direct PUT lifecycle: PutReserve -> PutStage -> PutPublish -> PutCommit   *)
(* (drive.rs:179-238, 388-488; storage/mod.rs:93-193)                        *)
(* ------------------------------------------------------------------------- *)

\* Step 1: Drive::reserve_file (under reservations mutex + db check_target)
PutReserve(w, p, sz, useIfMatch) ==
    /\ worker[w].pc = "idle"
    /\ nextBlobId <= MaxBlobId
    /\ LET ifM == IF useIfMatch /\ files[p].present THEN files[p].version ELSE 0
           prev == CurrentFileSize(files, p)
           growth == Max(0, sz - prev)
           pending == PendingBytes(files, uploads, "NONE")
           resQ == ReservedQuota(worker, "NONE")
           resD == ReservedDisk(worker, "NONE")
           free == DiskFree(storage, worker)
       IN
       /\ IfMatchOk(files[p], ifM)
       /\ usedBytes + pending + resQ + growth <= QuotaLimit
       /\ CheckDisk(free, resD, sz)
       /\ nextBlobId' = nextBlobId + 1
       /\ worker' = [worker EXCEPT ![w] = [
              pc          |-> "put_reserved",
              path        |-> p,
              size        |-> sz,
              ifMatch     |-> ifM,
              blobKey     |-> nextBlobId,
              resQuota    |-> growth,
              resDisk     |-> sz,
              stagedBytes |-> 0,
              rollbackKey |-> 0
          ]]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      storage, garbage, openError>>

\* Step 2a: Spool::stage succeeds (writes sz bytes to tmp/, drops disk_remaining to 0)
PutStageOk(w) ==
    /\ worker[w].pc = "put_reserved"
    /\ LET sz == worker[w].size
       IN
       /\ worker' = [worker EXCEPT ![w] = [@ EXCEPT
              !.pc = "put_staged",
              !.resDisk = 0,
              !.stagedBytes = sz]]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      storage, garbage, nextBlobId, openError>>

\* Step 2b: Spool::stage fails (checksum mismatch / interrupted body) -> drops temp & reservation
PutStageAbort(w) ==
    /\ worker[w].pc = "put_reserved"
    /\ worker' = [worker EXCEPT ![w] = IdleWorker]
    /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                   storage, garbage, nextBlobId, openError>>

\* Step 3: storage.publish + staged.remove (rename on LocalFs: net disk delta = 0, arms BlobRollback)
PutPublish(w) ==
    /\ worker[w].pc = "put_staged"
    /\ LET k == worker[w].blobKey
           sz == worker[w].size
       IN
       /\ storage' = [storage EXCEPT ![k] = sz]
       /\ worker' = [worker EXCEPT ![w] = [@ EXCEPT
              !.pc = "put_published",
              !.stagedBytes = 0,
              !.rollbackKey = k]]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      garbage, nextBlobId, openError>>

\* Step 4: commit_file under reservations mutex + SQLite transaction (drive.rs:388-475)
PutCommit(w) ==
    /\ worker[w].pc = "put_published"
    /\ LET p == worker[w].path
           sz == worker[w].size
           k == worker[w].blobKey
           ifM == worker[w].ifMatch
           resQ == ReservedQuota(worker, w)
           resD == ReservedDisk(worker, w)
           free == DiskFree(storage, worker)
           cur == files[p]
           oldSz == CurrentFileSize(files, p)
           projected == usedBytes - oldSz + sz
           newFile == [present |-> TRUE, key |-> k, size |-> sz, version |-> cur.version + 1]
           nextFiles == [files EXCEPT ![p] = newFile]
           pendingAfter == PendingBytes(nextFiles, uploads, "NONE")
           diskOk == CheckDisk(free, resD, 0)
           casOk == IfMatchOk(cur, ifM)
           quotaOk == (BUG_MODE = "skip_commit_quota_recheck")
                      \/ (projected <= QuotaLimit /\ projected + pendingAfter + resQ <= QuotaLimit)
       IN
       IF diskOk /\ casOk /\ quotaOk THEN
           /\ files' = nextFiles
           /\ usedBytes' = projected
           /\ garbage' = IF cur.present THEN garbage \cup {cur.key} ELSE garbage
           \* Disarm BlobRollback & drop reservation
           /\ worker' = [worker EXCEPT ![w] = IdleWorker]
           /\ UNCHANGED <<uploads, uploadReaders, uploadWriter, storage, nextBlobId, openError>>
       ELSE
           \* SQLite tx rolled back; drop reservation and keep BlobRollback armed
           /\ worker' = [worker EXCEPT ![w] = [@ EXCEPT
                  !.pc = "rollback",
                  !.resQuota = 0,
                  !.resDisk = 0]]
           /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                          storage, garbage, nextBlobId, openError>>

(* ------------------------------------------------------------------------- *)
(* Resumable upload lifecycle: InitUpload -> PutPart -> Complete | Abort     *)
(* (drive.rs:240-386, 490-513; db.rs:365-505)                                *)
(* ------------------------------------------------------------------------- *)

\* Drive::init_upload (atomic under reservations mutex + SQLite tx)
InitUpload(w, p, sz) ==
    /\ worker[w].pc = "idle"
    /\ ~uploads[p].active
    /\ LET prev == CurrentFileSize(files, p)
           growth == Max(0, sz - prev)
           pending == PendingBytes(files, uploads, "NONE")
           resQ == ReservedQuota(worker, "NONE")
           resD == ReservedDisk(worker, "NONE")
           free == DiskFree(storage, worker)
       IN
       /\ CheckDisk(free, resD, 0)
       /\ usedBytes + pending + resQ + growth <= QuotaLimit
       /\ uploads' = [uploads EXCEPT ![p] = [active |-> TRUE, size |-> sz, partKey |-> 0]]
       /\ UNCHANGED <<files, usedBytes, uploadReaders, uploadWriter,
                      storage, garbage, nextBlobId, worker, openError>>

\* Drive::put_part Step 1: acquire upload_lock read guard + reserve_part + stage
PutPartStart(w, p) ==
    /\ worker[w].pc = "idle"
    /\ nextBlobId <= MaxBlobId
    /\ uploads[p].active
    /\ uploadWriter[p] = "NONE"
    /\ LET sz == uploads[p].size
           resD == ReservedDisk(worker, "NONE")
           free == DiskFree(storage, worker)
       IN
       /\ CheckDisk(free, resD, sz)
       /\ uploadReaders' = [uploadReaders EXCEPT ![p] = @ + 1]
       /\ nextBlobId' = nextBlobId + 1
       /\ worker' = [worker EXCEPT ![w] = [
              pc          |-> "part_staged",
              path        |-> p,
              size        |-> sz,
              ifMatch     |-> 0,
              blobKey     |-> nextBlobId,
              resQuota    |-> 0,
              resDisk     |-> 0,
              stagedBytes |-> sz,
              rollbackKey |-> 0
          ]]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadWriter, storage, garbage, openError>>

\* Drive::put_part Step 2: publish part blob + db::record_part (enqueuing old part if overwritten)
PutPartCommit(w) ==
    /\ worker[w].pc = "part_staged"
    /\ LET p == worker[w].path
           sz == worker[w].size
           k == worker[w].blobKey
           resD == ReservedDisk(worker, w)
           free == DiskFree(storage, worker)
           oldPart == uploads[p].partKey
       IN
       \* Publish part blob (rename staged -> storage, net disk delta 0)
       /\ storage' = [storage EXCEPT ![k] = sz]
       /\ uploadReaders' = [uploadReaders EXCEPT ![p] = @ - 1]
       /\ IF CheckDisk(free, resD, 0) /\ uploads[p].active THEN
              /\ uploads' = [uploads EXCEPT ![p].partKey = k]
              /\ garbage' = IF oldPart /= 0 THEN garbage \cup {oldPart} ELSE garbage
              /\ worker' = [worker EXCEPT ![w] = IdleWorker]
              /\ UNCHANGED <<files, usedBytes, uploadWriter, nextBlobId, openError>>
          ELSE
              \* Disk check failed -> rollback published part blob
              /\ worker' = [worker EXCEPT ![w] = [@ EXCEPT
                     !.pc = "rollback",
                     !.stagedBytes = 0,
                     !.rollbackKey = k]]
              /\ UNCHANGED <<files, usedBytes, uploads, uploadWriter, garbage, nextBlobId, openError>>

\* Drive::complete Step 1: acquire upload_lock write guard + reserve_file(Some(id)) + stage from parts
CompleteStart(w, p, useIfMatch) ==
    /\ worker[w].pc = "idle"
    /\ nextBlobId <= MaxBlobId
    /\ uploads[p].active
    /\ uploads[p].partKey /= 0
    /\ uploadReaders[p] = 0
    /\ uploadWriter[p] = "NONE"
    /\ LET sz == uploads[p].size
           ifM == IF useIfMatch /\ files[p].present THEN files[p].version ELSE 0
           prev == CurrentFileSize(files, p)
           growth == Max(0, sz - prev)
           pendingOther == PendingBytes(files, uploads, p)
           resQ == ReservedQuota(worker, "NONE")
           resD == ReservedDisk(worker, "NONE")
           free == DiskFree(storage, worker)
       IN
       /\ IfMatchOk(files[p], ifM)
       /\ usedBytes + pendingOther + resQ + growth <= QuotaLimit
       /\ CheckDisk(free, resD, sz)
       /\ uploadWriter' = [uploadWriter EXCEPT ![p] = w]
       /\ nextBlobId' = nextBlobId + 1
       \* Notice drive.rs:215: resumable reservation sets resQuota = 0 because uploads[p]
       \* already accounts for its growth in SQLite pending_bytes!
       /\ worker' = [worker EXCEPT ![w] = [
              pc          |-> "complete_staged",
              path        |-> p,
              size        |-> sz,
              ifMatch     |-> ifM,
              blobKey     |-> nextBlobId,
              resQuota    |-> 0,
              resDisk     |-> 0,
              stagedBytes |-> sz,
              rollbackKey |-> 0
          ]]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, storage, garbage, openError>>

\* Drive::complete Step 2: publish assembled blob (rename staged -> storage, arm BlobRollback)
CompletePublish(w) ==
    /\ worker[w].pc = "complete_staged"
    /\ LET k == worker[w].blobKey
           sz == worker[w].size
       IN
       /\ storage' = [storage EXCEPT ![k] = sz]
       /\ worker' = [worker EXCEPT ![w] = [@ EXCEPT
              !.pc = "complete_published",
              !.stagedBytes = 0,
              !.rollbackKey = k]]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      garbage, nextBlobId, openError>>

\* Drive::complete Step 3: commit_file (remove_upload + write_file + quota/disk/If-Match re-checks)
CompleteCommit(w) ==
    /\ worker[w].pc = "complete_published"
    /\ LET p == worker[w].path
           sz == worker[w].size
           k == worker[w].blobKey
           ifM == worker[w].ifMatch
           resQ == ReservedQuota(worker, w)
           resD == ReservedDisk(worker, w)
           free == DiskFree(storage, worker)
           cur == files[p]
           oldSz == CurrentFileSize(files, p)
           projected == usedBytes - oldSz + sz
           partK == uploads[p].partKey
           newFile == [present |-> TRUE, key |-> k, size |-> sz, version |-> cur.version + 1]
           nextFiles == [files EXCEPT ![p] = newFile]
           nextUploads == [uploads EXCEPT ![p] = NoUpload]
           pendingAfter == PendingBytes(nextFiles, nextUploads, "NONE")
           diskOk == CheckDisk(free, resD, 0)
           casOk == IfMatchOk(cur, ifM)
           quotaOk == (BUG_MODE = "skip_commit_quota_recheck")
                      \/ (projected <= QuotaLimit /\ projected + pendingAfter + resQ <= QuotaLimit)
       IN
       /\ uploadWriter' = [uploadWriter EXCEPT ![p] = "NONE"]
       /\ IF diskOk /\ casOk /\ quotaOk THEN
              /\ files' = nextFiles
              /\ usedBytes' = projected
              /\ uploads' = nextUploads
              /\ garbage' = garbage
                            \cup (IF partK /= 0 THEN {partK} ELSE {})
                            \cup (IF cur.present THEN {cur.key} ELSE {})
              /\ worker' = [worker EXCEPT ![w] = IdleWorker]
              /\ UNCHANGED <<uploadReaders, storage, nextBlobId, openError>>
          ELSE
              /\ worker' = [worker EXCEPT ![w] = [@ EXCEPT
                     !.pc = "rollback",
                     !.resQuota = 0,
                     !.resDisk = 0]]
              /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders,
                             storage, garbage, nextBlobId, openError>>

\* Drive::abort (drive.rs:490-513): acquires upload_lock write guard + reservations mutex
AbortUpload(p) ==
    /\ uploads[p].active
    /\ uploadReaders[p] = 0
    /\ uploadWriter[p] = "NONE"
    /\ LET partK == uploads[p].partKey
       IN
       /\ uploads' = [uploads EXCEPT ![p] = NoUpload]
       /\ garbage' = IF partK /= 0 THEN garbage \cup {partK} ELSE garbage
       /\ UNCHANGED <<files, usedBytes, uploadReaders, uploadWriter,
                      storage, nextBlobId, worker, openError>>

(* ------------------------------------------------------------------------- *)
(* Delete, OpenFile, BlobRollback, and CollectGarbage                        *)
(* ------------------------------------------------------------------------- *)

\* Drive::delete (drive.rs:515-552): under reservations mutex + SQLite tx
DeleteFile(p) ==
    /\ files[p].present
    /\ LET cur == files[p]
       IN
       /\ files' = [files EXCEPT ![p] = NoFile]
       /\ usedBytes' = usedBytes - cur.size
       /\ garbage' = garbage \cup {cur.key}
       /\ UNCHANGED <<uploads, uploadReaders, uploadWriter,
                      storage, nextBlobId, worker, openError>>

\* Drive::open_file (drive.rs:554-593): under reservations mutex, opens live blob
OpenFile(p) ==
    /\ files[p].present
    /\ LET k == files[p].key
       IN
       /\ openError' = (openError \/ storage[k] = 0 \/ k \in garbage)
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      storage, garbage, nextBlobId, worker>>

\* BlobRollback::rollback (drive.rs:643-657):
\*   - Direct delete succeeds: removes blob from storage immediately.
\*   - Transient delete error: enqueues key into SQLite `garbage` for later GC retry.
BlobRollbackDirectDelete(w) ==
    /\ worker[w].pc = "rollback"
    /\ LET k == worker[w].rollbackKey
       IN
       /\ storage' = [storage EXCEPT ![k] = 0]
       /\ worker' = [worker EXCEPT ![w] = IdleWorker]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      garbage, nextBlobId, openError>>

BlobRollbackEnqueueGarbage(w) ==
    /\ worker[w].pc = "rollback"
    /\ LET k == worker[w].rollbackKey
       IN
       /\ garbage' = garbage \cup {k}
       /\ worker' = [worker EXCEPT ![w] = IdleWorker]
       /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                      storage, nextBlobId, openError>>

\* Drive::collect_garbage (drive.rs:595-613): under garbage_lock (independent of reservations)
CollectGarbageKey(k) ==
    /\ k \in garbage
    /\ storage' = [storage EXCEPT ![k] = 0]
    /\ garbage' = garbage \ {k}
    /\ UNCHANGED <<files, usedBytes, uploads, uploadReaders, uploadWriter,
                   nextBlobId, worker, openError>>

\* Stutter step when all workers are idle and garbage is empty (deadlock freedom)
IdleDone ==
    /\ \A w \in Workers : worker[w].pc = "idle"
    /\ garbage = {}
    /\ UNCHANGED vars

(* ------------------------------------------------------------------------- *)
(* Next-state relation and fairness                                          *)
(* ------------------------------------------------------------------------- *)

RollbackProgress(w) ==
    BlobRollbackDirectDelete(w) \/ BlobRollbackEnqueueGarbage(w)

Next ==
    \/ \E w \in Workers, p \in Paths, sz \in Sizes, im \in BOOLEAN : PutReserve(w, p, sz, im)
    \/ \E w \in Workers : PutStageOk(w) \/ PutStageAbort(w) \/ PutPublish(w) \/ PutCommit(w)
    \/ \E w \in Workers, p \in Paths, sz \in Sizes : InitUpload(w, p, sz)
    \/ \E w \in Workers, p \in Paths : PutPartStart(w, p)
    \/ \E w \in Workers : PutPartCommit(w)
    \/ \E w \in Workers, p \in Paths, im \in BOOLEAN : CompleteStart(w, p, im)
    \/ \E w \in Workers : CompletePublish(w) \/ CompleteCommit(w)
    \/ \E p \in Paths : AbortUpload(p) \/ DeleteFile(p) \/ OpenFile(p)
    \/ \E w \in Workers : RollbackProgress(w)
    \/ \E k \in BlobIds : CollectGarbageKey(k)
    \/ IdleDone

\* Weak fairness on GC and BlobRollback guarantees all retired/rolled-back blobs are reclaimed.
CollectGarbageStep ==
    \E k \in BlobIds : CollectGarbageKey(k)

Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(CollectGarbageStep)
    /\ \A w \in Workers : WF_vars(RollbackProgress(w))

(* ------------------------------------------------------------------------- *)
(* Safety Invariants                                                         *)
(* ------------------------------------------------------------------------- *)

TypeOK ==
    /\ \A p \in Paths :
           /\ files[p].present \in BOOLEAN
           /\ files[p].key \in 0..MaxBlobId
           /\ files[p].size \in ({0} \cup Sizes)
           /\ files[p].version \in 0..MaxBlobId
    /\ usedBytes \in 0..(DiskTotal)
    /\ \A p \in Paths :
           /\ uploads[p].active \in BOOLEAN
           /\ uploads[p].size \in ({0} \cup Sizes)
           /\ uploads[p].partKey \in 0..MaxBlobId
    /\ uploadReaders \in [Paths -> 0..Cardinality(Workers)]
    /\ uploadWriter \in [Paths -> Workers \cup {"NONE"}]
    /\ storage \in [BlobIds -> ({0} \cup Sizes)]
    /\ garbage \subseteq BlobIds
    /\ nextBlobId \in 1..(MaxBlobId + 1)
    /\ \A w \in Workers : worker[w].pc \in WorkerPCs
    /\ openError \in BOOLEAN

\* Committed file bytes never exceed the configured storage quota.
QuotaNeverExceeded ==
    usedBytes <= QuotaLimit

\* SQLite `usage.used_bytes` (maintained by triggers) always equals SUM(files.size_bytes).
UsageMatchesLiveFiles ==
    usedBytes = SumLiveFileSizes(files)

\* Actual free disk space (accounting for both stored blobs and staged spool files)
\* never dips below the configured free-disk floor.
DiskFloorRespected ==
    DiskFree(storage, worker) >= DiskFloor

\* Every live file and every live upload part points to a present blob of exact
\* matching size in storage that has NOT been enqueued into garbage; open_file
\* never encounters a missing or retired blob.
NoDanglingLiveBlob ==
    /\ ~openError
    /\ \A p \in Paths :
           files[p].present =>
               /\ files[p].key \in BlobIds
               /\ storage[files[p].key] = files[p].size
               /\ files[p].key \notin garbage
    /\ \A p \in Paths :
           (uploads[p].active /\ uploads[p].partKey /= 0) =>
               /\ uploads[p].partKey \in BlobIds
               /\ storage[uploads[p].partKey] = uploads[p].size
               /\ uploads[p].partKey \notin garbage

\* Every stored blob in storage is accounted for: live file, live upload part,
\* in-flight publish/rollback, or pending garbage (never leaked/orphaned).
NoOrphanedBlob ==
    \A k \in BlobIds :
        storage[k] > 0 =>
            \/ (\E p \in Paths : files[p].present /\ files[p].key = k)
            \/ (\E p \in Paths : uploads[p].active /\ uploads[p].partKey = k)
            \/ (\E w \in Workers : worker[w].rollbackKey = k)
            \/ (k \in garbage)

(* ------------------------------------------------------------------------- *)
(* Temporal Liveness Property                                                *)
(* ------------------------------------------------------------------------- *)

\* Under weak fairness of CollectGarbage and BlobRollback, every retired blob
\* enqueued into `garbage` is eventually reclaimed (garbage drains to empty,
\* and CollectGarbageKey deletes each key from storage atomically on drain).
GarbageEventuallyReclaimed ==
    [] (garbage /= {} => <> (garbage = {}))

=============================================================================

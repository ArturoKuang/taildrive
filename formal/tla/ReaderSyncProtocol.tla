--------------------------- MODULE ReaderSyncProtocol ---------------------------
(*
 * Formal TLA+ specification of the Reader Sync Protocol (SYNC_SPEC rev 3.2)
 * hosted inside TailDrive under /v1/reader.
 *
 * Faithful to implementation files (HEAD fc03a50):
 *   - crates/taildrive-server/src/reader/wire.rs:
 *       same, decide, position_rejection, decide_position
 *   - crates/taildrive-server/src/reader/store.rs:
 *       push, apply, apply_position, apply_record, write_head, write_new,
 *       open_conflict, append_side, pull, resolve, register_book
 *   - crates/taildrive-server/src/reader/blobs.rs:
 *       Blobs::put (disk_full isolation from record operations)
 *   - docs/reader-sync/SYNC_SPEC.md (§0.1, §3.3, §3.4, §5.3–§5.7, §6, §8)
 *
 * Atomic boundaries modeled:
 *   - Each push batch, conflict resolve, and book register executes inside a
 *     single SQLite transaction serialized by Store::conn (Arc<Mutex<Connection>>).
 *   - Crash-before-commit (TAILDRIVE_READER_CRASH_AT=push_before_commit) aborts
 *     the provisional transaction before tx.commit(), rolling back all tables;
 *     client retry with the same (batchId, changeId) then applies cleanly once.
 *   - Blobs::put checks free_floor_bytes (507 disk_full) only for book blobs;
 *     record endpoints never consult disk free space.
 *)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    Devices,        \* Set of paired device IDs, e.g. {"d1", "d2"}
    Payloads,       \* Set of opaque canonical JSON payloads, e.g. {"p1", "p2"}
    Shas,           \* Subset of {"s1", "s2", "s3", "s4"} representing registerable files
    PrimarySha,     \* The SHA of the primary book under which note/position records sync
    MaxSeq,         \* Upper bound on global monotonic server seq
    MaxChanges,     \* Upper bound on distinct fresh changeIds/batchIds
    BUG_MODE        \* "none" (spec) | "skip_conflict_check" | "position_opens_conflict" | "disk_full_blocks_records"

ASSUME BUG_MODE \in {"none", "skip_conflict_check", "position_opens_conflict", "disk_full_blocks_records"}
ASSUME Shas \subseteq {"s1", "s2", "s3", "s4"}
ASSUME PrimarySha \in Shas

(* ------------------------------------------------------------------------- *)
(* Helper sets and pure functions (wire.rs)                                  *)
(* ------------------------------------------------------------------------- *)

\* Deterministic file catalog exercising all three register_book rules (§3.3):
\*   s1: (pdf,  doc1) — primary PDF
\*   s2: (pdf,  doc1) — re-saved PDF sharing (pdf, doc1) -> Rule 2 attaches to s1 (or vice versa)
\*   s3: (epub, doc1) — EPUB with same docId -> Rule 2 format isolation creates separate book
\*   s4: (pdf,  none) — PDF without docId -> Rule 3 always creates separate book
BookSpec(s) ==
    CASE s = "s1" -> [sha |-> "s1", fmt |-> "pdf",  docId |-> "doc1"]
      [] s = "s2" -> [sha |-> "s2", fmt |-> "pdf",  docId |-> "doc1"]
      [] s = "s3" -> [sha |-> "s3", fmt |-> "epub", docId |-> "doc1"]
      [] s = "s4" -> [sha |-> "s4", fmt |-> "pdf",  docId |-> "none"]

BookCatalog == {BookSpec(s) : s \in Shas}
Formats == {b.fmt : b \in BookCatalog}
RealDocIds == {b.docId : b \in BookCatalog} \ {"none"}

\* Canonical content representation for same(x, y) (wire.rs:86-91):
\* Two tombstones are ALWAYS same regardless of carried payload, so we
\* normalize tombstones to payload = "NULL".
LiveContents == [deleted : {FALSE}, payload : Payloads]
TombstoneContent == [deleted |-> TRUE, payload |-> "NULL"]
CanonContents == LiveContents \cup {TombstoneContent}

Canon(deleted, payload) ==
    IF deleted THEN TombstoneContent ELSE [deleted |-> FALSE, payload |-> payload]

\* Uniform sentinel records (avoids comparing records with strings in TLC)
NoBookMeta == [live |-> FALSE, version |-> 0, seq |-> 0]
NoHead == [present |-> FALSE, version |-> 0, deleted |-> FALSE,
           payload |-> "NULL", seq |-> 0, conflictOpen |-> FALSE]
NoConflict == [open |-> FALSE, baseVersion |-> 0, openedSeq |-> 0, sides |-> {}]
NoPos == [present |-> FALSE, version |-> 0, payload |-> "NULL",
          seq |-> 0, conflictOpen |-> FALSE]
NoApplied == [present |-> FALSE, base |-> 0, content |-> TombstoneContent,
              status |-> "none", seq |-> 0]

\* wire::same (wire.rs:86-91)
Same(a, b) ==
    a.deleted = b.deleted /\ (a.deleted \/ a.payload = b.payload)

\* wire::decide (wire.rs:126-155) — pure §6 merge matrix.
\* BUG_MODE = "skip_conflict_check" models a defect that fast-forwards stale
\* baseVersion writes instead of opening/appending a conflict.
Decide(h, openSides, base, inc) ==
    IF ~h.present THEN
        IF base = 0 THEN "Create" ELSE "RejectBadBase"
    ELSE IF Same(h, inc) THEN
        "NoopHead"
    ELSE IF base > h.version THEN
        "RejectBadBase"
    ELSE IF base = h.version THEN
        "FastForward"
    ELSE IF BUG_MODE = "skip_conflict_check" THEN
        "FastForward"
    ELSE IF \E s \in openSides : Same(s, inc) THEN
        "NoopConflict"
    ELSE IF openSides = {} THEN
        "OpenConflict"
    ELSE
        "AppendConflict"

\* wire::position_rejection (wire.rs:162-182)
PositionRejection(caller, owner, deleted) ==
    IF deleted THEN "bad_request"
    ELSE IF caller /= owner THEN "not_owner"
    ELSE "OK"

\* wire::decide_position (wire.rs:185-196)
DecidePosition(pHead, deleted, payload) ==
    IF deleted THEN "RejectBadRequest"
    ELSE IF ~pHead.present THEN "Create"
    ELSE IF pHead.payload = payload THEN "NoopHead"
    ELSE "FastForward"

(* ------------------------------------------------------------------------- *)
(* State variables                                                           *)
(* ------------------------------------------------------------------------- *)

VARIABLES
    seq,                    \* Global monotonic server counter (meta.seq)
    \* --- Book registry (store.rs:1070-1143) ---
    bookFiles,              \* [Shas -> Shas \cup {"NONE"}] (book_files.sha256 -> book_id)
    bookDocIds,             \* [Formats \X RealDocIds -> Shas \cup {"NONE"}] (book_doc_ids)
    bookMeta,               \* [Shas -> [live, version, seq]]
    bookRegOk,              \* Deterministic Rule 1/2/3 consistency check
    \* --- Normal record ("note1" under PrimaryBook) (records, record_versions, conflicts) ---
    head,                   \* [present, version, deleted, payload, seq, conflictOpen]
    recVersions,            \* Seq([deleted, payload, seq]) — immutable record_versions log
    conflict,               \* [open, baseVersion, openedSeq, sides]
    unmergedWrites,         \* Ghost set of canonical contents written concurrently without being superseded/resolved
    \* --- Position records (one per device under PrimaryBook) (§3.4) ---
    posRec,                 \* [Devices -> [present, version, payload, seq, conflictOpen]]
    \* --- Idempotency & crash-before-commit (§5.3, §8.4) ---
    nextCid,                \* Canonical allocator 1..(MaxChanges+1) for fresh batch/change IDs
    appliedChanges,         \* [1..MaxChanges -> [present, base, content, status, seq]] (models batches + applied_changes)
    pendingCrash,           \* BOOLEAN: TRUE while a push tx is provisionally open before commit/abort
    replayViolation,        \* Ghost flag: TRUE iff replay mutated state or mismatched reuse was accepted
    \* --- Blob store & disk_full isolation (§0.1, §5.7, blobs.rs) ---
    diskFull,               \* BOOLEAN: TRUE when server free disk is below free_floor_bytes
    primaryBlobStored,      \* BOOLEAN: primary book blob complete in blobs table
    diskFullBlockedRecord   \* Ghost flag: TRUE iff disk_full blocked a record/book/resolve endpoint

vars == <<
    seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
    head, recVersions, conflict, unmergedWrites,
    posRec, nextCid, appliedChanges, pendingCrash, replayViolation,
    diskFull, primaryBlobStored, diskFullBlockedRecord
>>

BookMetaDomain ==
    [live : BOOLEAN, version : 0..MaxSeq, seq : 0..MaxSeq]

HeadDomain ==
    [present : BOOLEAN, version : 0..MaxSeq, deleted : BOOLEAN,
     payload : Payloads \cup {"NULL"}, seq : 0..MaxSeq, conflictOpen : BOOLEAN]

ConflictDomain ==
    [open : BOOLEAN, baseVersion : 0..MaxSeq, openedSeq : 0..MaxSeq,
     sides : SUBSET CanonContents]

PosDomain ==
    [present : BOOLEAN, version : 0..MaxSeq, payload : Payloads \cup {"NULL"},
     seq : 0..MaxSeq, conflictOpen : BOOLEAN]

AppliedStatuses ==
    {"none", "applied", "noop", "conflict", "rejected"}

(* ------------------------------------------------------------------------- *)
(* Initial state                                                             *)
(* ------------------------------------------------------------------------- *)

Init ==
    /\ seq = 0
    /\ bookFiles = [s \in Shas |-> "NONE"]
    /\ bookDocIds = [k \in Formats \X RealDocIds |-> "NONE"]
    /\ bookMeta = [s \in Shas |-> NoBookMeta]
    /\ bookRegOk = TRUE
    /\ head = NoHead
    /\ recVersions = << >>
    /\ conflict = NoConflict
    /\ unmergedWrites = {}
    /\ posRec = [d \in Devices |-> NoPos]
    /\ nextCid = 1
    /\ appliedChanges = [i \in 1..MaxChanges |-> NoApplied]
    /\ pendingCrash = FALSE
    /\ replayViolation = FALSE
    /\ diskFull = FALSE
    /\ primaryBlobStored = FALSE
    /\ diskFullBlockedRecord = FALSE

(* ------------------------------------------------------------------------- *)
(* Helper: primary book live check                                           *)
(* ------------------------------------------------------------------------- *)

PrimaryBookId == bookFiles[PrimarySha]
PrimaryBookLive == PrimaryBookId /= "NONE" /\ bookMeta[PrimaryBookId].live

(* ------------------------------------------------------------------------- *)
(* Action 1: RegisterBook (§3.3, store.rs:1070-1143)                         *)
(* Rules 1 -> 2 -> 3 applied in order inside one SQLite transaction.         *)
(* ------------------------------------------------------------------------- *)

RegisterBook(entry) ==
    /\ ~pendingCrash
    /\ LET s == entry.sha
           f == entry.fmt
           d == entry.docId
       IN
       \* Rule 1: sha256 already in book_files -> status "existing", seq unchanged
       IF bookFiles[s] /= "NONE" THEN
           /\ bookRegOk' = (bookRegOk /\ bookMeta[bookFiles[s]].live)
           /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta,
                          head, recVersions, conflict, unmergedWrites,
                          posRec, nextCid, appliedChanges, pendingCrash,
                          replayViolation, diskFull, primaryBlobStored,
                          diskFullBlockedRecord>>
       \* Rule 2: non-empty docId matching a live book of the SAME format -> "attached"
       ELSE IF d /= "none" /\ bookDocIds[<<f, d>>] /= "NONE" THEN
           /\ seq < MaxSeq
           /\ LET bid == bookDocIds[<<f, d>>]
                  nseq == seq + 1
              IN
              /\ seq' = nseq
              /\ bookFiles' = [bookFiles EXCEPT ![s] = bid]
              /\ bookMeta' = [bookMeta EXCEPT ![bid] =
                    [live |-> TRUE, version |-> @.version + 1, seq |-> nseq]]
              /\ UNCHANGED <<bookDocIds, bookRegOk,
                             head, recVersions, conflict, unmergedWrites,
                             posRec, nextCid, appliedChanges, pendingCrash,
                             replayViolation, diskFull, primaryBlobStored,
                             diskFullBlockedRecord>>
       \* Rule 3: create new book with bookId = sha, version = 1
       ELSE
           /\ seq < MaxSeq
           /\ LET nseq == seq + 1
              IN
              /\ seq' = nseq
              /\ bookFiles' = [bookFiles EXCEPT ![s] = s]
              /\ bookDocIds' = IF d /= "none"
                               THEN [bookDocIds EXCEPT ![<<f, d>>] = s]
                               ELSE bookDocIds
              /\ bookMeta' = [bookMeta EXCEPT ![s] = [live |-> TRUE, version |-> 1, seq |-> nseq]]
              /\ UNCHANGED <<bookRegOk,
                             head, recVersions, conflict, unmergedWrites,
                             posRec, nextCid, appliedChanges, pendingCrash,
                             replayViolation, diskFull, primaryBlobStored,
                             diskFullBlockedRecord>>

(* ------------------------------------------------------------------------- *)
(* Action 2: PushFreshNote (normal record, §5.3 & §6, store.rs:504-801)      *)
(* Single-change batch with fresh (batchId, changeId) = nextCid.             *)
(* ------------------------------------------------------------------------- *)

PushFreshNote(base, inc) ==
    /\ ~pendingCrash
    /\ nextCid <= MaxChanges
    /\ LET cid == nextCid
       IN
       \* Canary mode: defect where diskFull wrongly blocks record pushes
       IF BUG_MODE = "disk_full_blocks_records" /\ diskFull THEN
           /\ diskFullBlockedRecord' = TRUE
           /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
                          head, recVersions, conflict, unmergedWrites,
                          posRec, nextCid, appliedChanges, pendingCrash,
                          replayViolation, diskFull, primaryBlobStored>>
       \* Unregistered book -> per-change rejection unknown_book (no record/seq mutation)
       ELSE IF ~PrimaryBookLive THEN
           /\ UNCHANGED vars
       ELSE
           LET openSides == IF conflict.open THEN conflict.sides ELSE {}
               dec == Decide(head, openSides, base, inc)
           IN
           CASE dec = "Create" ->
               /\ seq < MaxSeq
               /\ LET nseq == seq + 1
                      newHead == [present |-> TRUE, version |-> 1, deleted |-> inc.deleted,
                                  payload |-> inc.payload, seq |-> nseq,
                                  conflictOpen |-> FALSE]
                  IN
                  /\ seq' = nseq
                  /\ head' = newHead
                  /\ recVersions' = Append(recVersions,
                        [deleted |-> inc.deleted, payload |-> inc.payload, seq |-> nseq])
                  /\ unmergedWrites' = {inc}
                  /\ nextCid' = cid + 1
                  /\ appliedChanges' = [appliedChanges EXCEPT ![cid] =
                        [present |-> TRUE, base |-> base, content |-> inc,
                         status |-> "applied", seq |-> nseq]]
                  /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                                 conflict, posRec, pendingCrash, replayViolation,
                                 diskFull, primaryBlobStored, diskFullBlockedRecord>>
             [] dec = "FastForward" ->
               /\ seq < MaxSeq
               /\ LET nseq == seq + 1
                      nver == head.version + 1
                      newHead == [present |-> TRUE, version |-> nver, deleted |-> inc.deleted,
                                  payload |-> inc.payload, seq |-> nseq,
                                  conflictOpen |-> head.conflictOpen]
                      \* Spec semantics: a valid fast-forward (base = head.version)
                      \* supersedes the prior head content; a stale write (base < head.version)
                      \* is a concurrent branch whose prior head must NOT be lost.
                      nextUnmerged ==
                          IF base = head.version
                          THEN (unmergedWrites \ {Canon(head.deleted, head.payload)}) \cup {inc}
                          ELSE unmergedWrites \cup {inc}
                  IN
                  /\ seq' = nseq
                  /\ head' = newHead
                  /\ recVersions' = Append(recVersions,
                        [deleted |-> inc.deleted, payload |-> inc.payload, seq |-> nseq])
                  /\ unmergedWrites' = nextUnmerged
                  /\ nextCid' = cid + 1
                  /\ appliedChanges' = [appliedChanges EXCEPT ![cid] =
                        [present |-> TRUE, base |-> base, content |-> inc,
                         status |-> "applied", seq |-> nseq]]
                  /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                                 conflict, posRec, pendingCrash, replayViolation,
                                 diskFull, primaryBlobStored, diskFullBlockedRecord>>
             [] dec = "NoopHead" \/ dec = "NoopConflict" ->
               /\ UNCHANGED vars
             [] dec = "OpenConflict" ->
               /\ seq < MaxSeq
               /\ LET nseq == seq + 1
                      headCanon == Canon(head.deleted, head.payload)
                  IN
                  /\ seq' = nseq
                  /\ head' = [head EXCEPT !.seq = nseq, !.conflictOpen = TRUE]
                  /\ conflict' = [open |-> TRUE, baseVersion |-> base, openedSeq |-> nseq,
                                  sides |-> {headCanon, inc}]
                  /\ unmergedWrites' = unmergedWrites \cup {inc}
                  /\ nextCid' = cid + 1
                  /\ appliedChanges' = [appliedChanges EXCEPT ![cid] =
                        [present |-> TRUE, base |-> base, content |-> inc,
                         status |-> "conflict", seq |-> nseq]]
                  /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                                 recVersions, posRec, pendingCrash, replayViolation,
                                 diskFull, primaryBlobStored, diskFullBlockedRecord>>
             [] dec = "AppendConflict" ->
               /\ seq < MaxSeq
               /\ LET nseq == seq + 1
                  IN
                  /\ seq' = nseq
                  /\ head' = [head EXCEPT !.seq = nseq]
                  /\ conflict' = [conflict EXCEPT !.sides = @ \cup {inc}]
                  /\ unmergedWrites' = unmergedWrites \cup {inc}
                  /\ nextCid' = cid + 1
                  /\ appliedChanges' = [appliedChanges EXCEPT ![cid] =
                        [present |-> TRUE, base |-> base, content |-> inc,
                         status |-> "conflict", seq |-> nseq]]
                  /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                                 recVersions, posRec, pendingCrash, replayViolation,
                                 diskFull, primaryBlobStored, diskFullBlockedRecord>>
             [] dec = "RejectBadBase" ->
               /\ UNCHANGED vars

(* ------------------------------------------------------------------------- *)
(* Action 3: Idempotent Replay & Mismatched Reuse (§5.3, store.rs:536-584)   *)
(*   - Same batchId/changeId + same body -> replays stored result, seq same. *)
(*   - Same batchId/changeId + diff body -> 422 / rejected change_id_reused. *)
(* ------------------------------------------------------------------------- *)

ReplayOrReuseChange(cid, altBase, altInc) ==
    /\ ~pendingCrash
    /\ cid < nextCid
    /\ appliedChanges[cid].present
    /\ LET sameBody == (altBase = appliedChanges[cid].base
                        /\ altInc = appliedChanges[cid].content)
       IN
       \* Same body replays stored response; different body rejects change_id_reused.
       \* Neither mutates records, conflicts, or seq.
       /\ replayViolation' = (replayViolation \/ (~sameBody /\ FALSE))
       /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
                      head, recVersions, conflict, unmergedWrites,
                      posRec, nextCid, appliedChanges, pendingCrash,
                      diskFull, primaryBlobStored, diskFullBlockedRecord>>

(* ------------------------------------------------------------------------- *)
(* Action 4: Crash-before-commit & retry (§8.4, store.rs:560-567)            *)
(* A push transaction executes provisionally and aborts before tx.commit().  *)
(* SQLite rolls back all writes so nextCid can be retried cleanly.           *)
(* ------------------------------------------------------------------------- *)

BeginCrashablePush ==
    /\ ~pendingCrash
    /\ nextCid <= MaxChanges
    /\ PrimaryBookLive
    /\ pendingCrash' = TRUE
    /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
                   head, recVersions, conflict, unmergedWrites,
                   posRec, nextCid, appliedChanges, replayViolation,
                   diskFull, primaryBlobStored, diskFullBlockedRecord>>

CrashBeforeCommitRollback ==
    /\ pendingCrash
    \* SQLite transaction aborts: records, applied_changes, batches, seq untouched!
    /\ pendingCrash' = FALSE
    /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
                   head, recVersions, conflict, unmergedWrites,
                   posRec, nextCid, appliedChanges, replayViolation,
                   diskFull, primaryBlobStored, diskFullBlockedRecord>>

(* ------------------------------------------------------------------------- *)
(* Action 5: ResolveConflict (§5.5, store.rs:996-1053)                       *)
(* Checks already_resolved (409), stale_base (409), else writes head+1 and   *)
(* closes the conflict atomically.                                           *)
(* ------------------------------------------------------------------------- *)

ResolveConflict(base, inc) ==
    /\ ~pendingCrash
    /\ head.present
    /\ IF ~conflict.open THEN
           \* 409 already_resolved — no state mutation
           UNCHANGED vars
       ELSE IF base /= head.version THEN
           \* 409 stale_base — no state mutation
           UNCHANGED vars
       ELSE
           /\ seq < MaxSeq
           /\ LET nseq == seq + 1
                  nver == head.version + 1
              IN
              /\ seq' = nseq
              /\ head' = [present |-> TRUE, version |-> nver, deleted |-> inc.deleted,
                          payload |-> inc.payload, seq |-> nseq,
                          conflictOpen |-> FALSE]
              /\ recVersions' = Append(recVersions,
                    [deleted |-> inc.deleted, payload |-> inc.payload, seq |-> nseq])
              /\ conflict' = NoConflict
              /\ unmergedWrites' = {inc}
              /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                             posRec, nextCid, appliedChanges, pendingCrash,
                             replayViolation, diskFull, primaryBlobStored,
                             diskFullBlockedRecord>>

(* ------------------------------------------------------------------------- *)
(* Action 6: PushPosition (§3.4, wire.rs:162-196, store.rs:648-663)          *)
(* Owner-only, no deletes, baseVersion ignored, never opens a conflict.      *)
(* ------------------------------------------------------------------------- *)

PushPosition(caller, owner, base, deleted, payload) ==
    /\ ~pendingCrash
    /\ PrimaryBookLive
    /\ LET rej == PositionRejection(caller, owner, deleted)
       IN
       IF rej /= "OK" THEN
           \* Rejected not_owner or bad_request — no state mutation
           UNCHANGED vars
       ELSE
           LET pHead == posRec[owner]
               dec == DecidePosition(pHead, deleted, payload)
           IN
           IF dec = "Create" THEN
               /\ seq < MaxSeq
               /\ LET nseq == seq + 1
                  IN
                  /\ seq' = nseq
                  /\ posRec' = [posRec EXCEPT ![owner] =
                        [present |-> TRUE, version |-> 1, payload |-> payload,
                         seq |-> nseq, conflictOpen |-> FALSE]]
                  /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                                 head, recVersions, conflict, unmergedWrites,
                                 nextCid, appliedChanges, pendingCrash,
                                 replayViolation, diskFull, primaryBlobStored,
                                 diskFullBlockedRecord>>
           ELSE IF dec = "NoopHead" THEN
               UNCHANGED vars
           ELSE \* FastForward (base ignored!)
               /\ seq < MaxSeq
               /\ LET nseq == seq + 1
                      bugConflict == (BUG_MODE = "position_opens_conflict" /\ base < pHead.version)
                  IN
                  /\ seq' = nseq
                  /\ posRec' = [posRec EXCEPT ![owner] =
                        [present |-> TRUE, version |-> pHead.version + 1, payload |-> payload,
                         seq |-> nseq, conflictOpen |-> bugConflict]]
                  /\ UNCHANGED <<bookFiles, bookDocIds, bookMeta, bookRegOk,
                                 head, recVersions, conflict, unmergedWrites,
                                 nextCid, appliedChanges, pendingCrash,
                                 replayViolation, diskFull, primaryBlobStored,
                                 diskFullBlockedRecord>>

(* ------------------------------------------------------------------------- *)
(* Action 7: Blob upload & disk_full toggle (§0.1, §5.7, blobs.rs:218-321)   *)
(* Duplicate blob PUT returns 200 without checking floor; new blob PUT fails *)
(* with 507 disk_full when diskFull = TRUE; record endpoints never check it. *)
(* ------------------------------------------------------------------------- *)

ToggleDiskFull ==
    /\ ~pendingCrash
    /\ diskFull' = ~diskFull
    /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
                   head, recVersions, conflict, unmergedWrites,
                   posRec, nextCid, appliedChanges, pendingCrash,
                   replayViolation, primaryBlobStored, diskFullBlockedRecord>>

UploadPrimaryBlob ==
    /\ ~pendingCrash
    /\ IF primaryBlobStored THEN
           \* Duplicate blob: 200 OK even if diskFull
           UNCHANGED vars
       ELSE IF diskFull THEN
           \* 507 disk_full — blob not stored, records unaffected
           UNCHANGED vars
       ELSE
           /\ primaryBlobStored' = TRUE
           /\ UNCHANGED <<seq, bookFiles, bookDocIds, bookMeta, bookRegOk,
                          head, recVersions, conflict, unmergedWrites,
                          posRec, nextCid, appliedChanges, pendingCrash,
                          replayViolation, diskFull, diskFullBlockedRecord>>

(* ------------------------------------------------------------------------- *)
(* Next-state relation                                                       *)
(* ------------------------------------------------------------------------- *)

AllowedBase ==
    IF ~head.present THEN {0, 1}
    ELSE 0..(head.version + 1)

Next ==
    \/ \E e \in BookCatalog : RegisterBook(e)
    \/ \E b \in AllowedBase, inc \in CanonContents : PushFreshNote(b, inc)
    \/ \E cid \in 1..MaxChanges, b \in {0, 1}, inc \in CanonContents :
           ReplayOrReuseChange(cid, b, inc)
    \/ BeginCrashablePush
    \/ CrashBeforeCommitRollback
    \/ \E b \in AllowedBase, inc \in CanonContents : ResolveConflict(b, inc)
    \/ \E caller, owner \in Devices, b \in {0, 1}, del \in BOOLEAN, p \in Payloads :
           PushPosition(caller, owner, b, del, p)
    \/ UploadPrimaryBlob
    \/ ToggleDiskFull

Spec == Init /\ [][Next]_vars

(* ------------------------------------------------------------------------- *)
(* Safety Invariants                                                         *)
(* ------------------------------------------------------------------------- *)

TypeOK ==
    /\ seq \in 0..MaxSeq
    /\ bookFiles \in [Shas -> Shas \cup {"NONE"}]
    /\ bookDocIds \in [Formats \X RealDocIds -> Shas \cup {"NONE"}]
    /\ bookMeta \in [Shas -> BookMetaDomain]
    /\ bookRegOk \in BOOLEAN
    /\ head \in HeadDomain
    /\ Len(recVersions) = IF head.present THEN head.version ELSE 0
    /\ conflict \in ConflictDomain
    /\ unmergedWrites \subseteq CanonContents
    /\ posRec \in [Devices -> PosDomain]
    /\ nextCid \in 1..(MaxChanges + 1)
    /\ pendingCrash \in BOOLEAN
    /\ replayViolation \in BOOLEAN
    /\ diskFull \in BOOLEAN
    /\ primaryBlobStored \in BOOLEAN
    /\ diskFullBlockedRecord \in BOOLEAN

\* Every persisted version and live record carries a valid, strictly increasing,
\* globally unique sequence number <= seq (matching SQLite `records.seq UNIQUE`).
SeqStrictlyMonotonic ==
    /\ (head.present =>
            /\ head.seq >= 1 /\ head.seq <= seq
            /\ head.seq >= recVersions[head.version].seq
            /\ \A i \in 1..Len(recVersions) : recVersions[i].seq >= 1 /\ recVersions[i].seq <= seq
            /\ \A i, j \in 1..Len(recVersions) : i < j => recVersions[i].seq < recVersions[j].seq)
    /\ (conflict.open => conflict.openedSeq >= 1 /\ conflict.openedSeq <= seq)
    /\ \A d \in Devices : posRec[d].present => posRec[d].seq >= 1 /\ posRec[d].seq <= seq
    /\ \A s \in Shas : bookMeta[s].live => bookMeta[s].seq >= 1 /\ bookMeta[s].seq <= seq
    \* Global uniqueness of records.seq across all live records (books, note, positions)
    /\ \A s1, s2 \in Shas :
           (s1 /= s2 /\ bookMeta[s1].live /\ bookMeta[s2].live)
           => bookMeta[s1].seq /= bookMeta[s2].seq
    /\ \A d1, d2 \in Devices :
           (d1 /= d2 /\ posRec[d1].present /\ posRec[d2].present)
           => posRec[d1].seq /= posRec[d2].seq
    /\ (head.present =>
           /\ \A s \in Shas : bookMeta[s].live => head.seq /= bookMeta[s].seq
           /\ \A d \in Devices : posRec[d].present => head.seq /= posRec[d].seq)
    /\ \A s \in Shas, d \in Devices :
           (bookMeta[s].live /\ posRec[d].present)
           => bookMeta[s].seq /= posRec[d].seq

\* Every committed batch/changeId is recorded consistently with seq <= current seq;
\* replays never advance seq, and mismatched reuse never mutates records.
IdempotentReplaySound ==
    /\ ~replayViolation
    /\ \A i \in 1..MaxChanges :
           /\ (i >= nextCid => ~appliedChanges[i].present)
           /\ (i < nextCid =>
                  /\ appliedChanges[i].present
                  /\ appliedChanges[i].seq >= 1
                  /\ appliedChanges[i].seq <= seq)

\* §3.4: position records never open a conflict.
PositionNeverConflicts ==
    \A d \in Devices : posRec[d].present => ~posRec[d].conflictOpen

\* §6: identical content is never a conflict — every open conflict must have
\* at least 2 canonically distinct sides, and head.conflictOpen matches conflict.open.
IdenticalContentNeverConflicts ==
    /\ (~head.present => ~conflict.open)
    /\ (head.present => (head.conflictOpen <=> conflict.open))
    /\ (conflict.open => Cardinality(conflict.sides) >= 2)

\* §6 / §12.3: no concurrent divergent edit is ever silently overwritten;
\* every unmerged write remains visible either as head or in conflict.sides
\* until explicitly superseded by a valid fast-forward or ResolveConflict.
VisibleContents ==
    (IF head.present THEN {Canon(head.deleted, head.payload)} ELSE {})
    \cup (IF conflict.open THEN conflict.sides ELSE {})

NoSilentOverwrite ==
    unmergedWrites \subseteq VisibleContents

\* §3.3: Book registration Rules 1–3 are deterministic:
\* - Every registered sha points to a live book.
\* - Two registered files with the same (fmt, non-empty docId) share the same bookId.
\* - Two registered files with different formats NEVER share a bookId.
BookRegisterDeterministic ==
    /\ bookRegOk
    /\ \A s \in Shas :
           bookFiles[s] /= "NONE" => bookMeta[bookFiles[s]].live
    /\ \A e1, e2 \in BookCatalog :
           (bookFiles[e1.sha] /= "NONE" /\ bookFiles[e2.sha] /= "NONE") =>
               /\ (e1.fmt = e2.fmt /\ e1.docId /= "none" /\ e1.docId = e2.docId
                   => bookFiles[e1.sha] = bookFiles[e2.sha])
               /\ (e1.fmt /= e2.fmt
                   => bookFiles[e1.sha] /= bookFiles[e2.sha])

\* §0.1 / §5.7: disk_full gates book blob uploads only; record endpoints never fail with disk_full.
DiskFullNeverBlocksRecords ==
    ~diskFullBlockedRecord

\* Action property: global seq never decreases.
SeqNeverDecreases ==
    [][seq' >= seq]_vars

=============================================================================

--------------------------- MODULE TailDriveSync3Way ---------------------------
(*
 * Formal TLA+ specification of TailDrive's 3-way folder sync planner and
 * reconciliation worker.
 *
 * Faithful to implementation files (HEAD fc03a50):
 *   - crates/taildrive-ffi/src/sync.rs:
 *       plan_folder_sync (Pass 1 top-down kind-conflict isolation,
 *                         Pass 2 bottom-up action & keep_*_folders planning,
 *                         Pass 3 forward top-down / delete bottom-up emission)
 *   - clients/macos/Sources/TailDriveSyncKit/SyncWorker.swift:
 *       SyncWorker.sync, SyncWorker.apply, SyncWorker.saveConflict, Checkpoint
 *   - clients/macos/Sources/TailDriveSyncKit/LocalMirror.swift:
 *       LocalMirror.preserveConflict, LocalMirror.delete, LocalMirror.validate
 *
 * Atomic boundaries modeled:
 *   - StartSync: remoteSnapshot + mirror.scan + plan_folder_sync + checkpoint
 *     equal/deleted pruning (SyncWorker.swift:162-193).
 *   - ApplyNextAction: executes the next planned SyncAction in exact Pass 3
 *     order, with server-side If-Match CAS on existing-file uploads (409 ->
 *     preserveConflict sibling move) and non-empty folder protection.
 *   - MidSyncRemoteEdit: models another TailDrive client mutating an existing
 *     remote file between remoteSnapshot and apply(.upload), triggering
 *     409 ERR_CONFLICT -> saveConflict (SyncWorker.swift:294-305).
 *)

EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    BasePaths,      \* Subset of {"a", "a/f", "a/g", "b"}; always contains "a", "a/f", "b"
    ContentVals,    \* Set of file content hashes, e.g. {"v1", "v2"}
    MaxEdits,       \* Maximum number of user mutations before quiescence
    BUG_MODE        \* "none" | "drop_keep_folder_guard" | "delete_overwrites_edit" | "unconditional_delete_race"

ASSUME BUG_MODE \in {"none", "drop_keep_folder_guard", "delete_overwrites_edit", "unconditional_delete_race"}
ASSUME {"a", "a/f", "b"} \subseteq BasePaths
ASSUME BasePaths \subseteq {"a", "a/f", "a/g", "b"}

(* ------------------------------------------------------------------------- *)
(* Path hierarchy & conflict-sibling mapping (taildrive-core/src/path.rs)    *)
(* ------------------------------------------------------------------------- *)

HasG == "a/g" \in BasePaths

ConflictOf(p) ==
    CASE p = "a"   -> "a_c"
      [] p = "a/f" -> "a/f_c"
      [] p = "a/g" -> "a/g_c"
      [] p = "b"   -> "b_c"
      [] OTHER     -> p

MovedUnderAC(p) ==
    CASE p = "a/f" -> "a_c/f"
      [] p = "a/g" -> "a_c/g"
      [] OTHER     -> p

AllPaths ==
    BasePaths
    \cup {ConflictOf(p) : p \in BasePaths}
    \cup {"a_c2", "a_c/f"}
    \cup (IF HasG THEN {"a_c/g"} ELSE {})

ParentOf(p) ==
    IF p \in {"a/f", "a/g", "a/f_c", "a/g_c"} THEN "a"
    ELSE IF p \in {"a_c/f", "a_c/g"} THEN "a_c"
    ELSE ""

Ancestors(p) ==
    IF ParentOf(p) = "" THEN {} ELSE {ParentOf(p)}

Descendants(p) ==
    {q \in AllPaths : ParentOf(q) = p}

Depth(p) ==
    IF ParentOf(p) = "" THEN 0 ELSE 1

\* Canonical ordering matching BTreeMap<DrivePath, _> / Reverse(depth) (sync.rs:63-67,173-174)
PathOrder ==
    IF HasG
    THEN <<"a", "a_c", "a_c2", "b", "b_c", "a/f", "a/f_c", "a/g", "a/g_c", "a_c/f", "a_c/g">>
    ELSE <<"a", "a_c", "a_c2", "b", "b_c", "a/f", "a/f_c", "a_c/f">>

\* Where a user edit originally made at base path p may legitimately reside
\* (at p itself, its conflict sibling(s), or under the moved conflict folder a_c/*).
AllowedLocations(p) ==
    IF p = "a" THEN {"a", "a_c", "a_c2"}
    ELSE IF p \in {"a/f", "a/g"} THEN {p, ConflictOf(p), MovedUnderAC(p)}
    ELSE {p, ConflictOf(p)}

(* ------------------------------------------------------------------------- *)
(* Entry representation and pure plan_folder_sync (sync.rs:27-176)           *)
(* ------------------------------------------------------------------------- *)

Entries == {"NONE", "DIR"} \cup ContentVals

IsFile(e)    == e \in ContentVals
IsFolder(e)  == e = "DIR"
IsPresent(e) == e /= "NONE"
KindOf(e)    == IF e = "NONE" THEN "NONE" ELSE IF e = "DIR" THEN "DIR" ELSE "FILE"

Actions == {
    "None", "Upload", "Download",
    "CreateRemoteFolder", "CreateLocalFolder",
    "DeleteRemote", "DeleteLocal", "Conflict"
}

IsForward(a) == a \in {"Upload", "Download", "CreateRemoteFolder", "CreateLocalFolder", "Conflict"}
IsDelete(a)  == a \in {"DeleteRemote", "DeleteLocal"}

\* Pass 1 (top-down, sync.rs:42-59): identify kind conflicts so descendants are skipped.
KindConflicts(L, R) ==
    {p \in AllPaths :
        /\ IsPresent(L[p]) /\ IsPresent(R[p])
        /\ KindOf(L[p]) /= KindOf(R[p])
        /\ \A anc \in Ancestors(p) :
               ~(IsPresent(L[anc]) /\ IsPresent(R[anc]) /\ KindOf(L[anc]) /= KindOf(R[anc]))}

ActivePaths(L, R, B) ==
    {p \in AllPaths :
        /\ (IsPresent(L[p]) \/ IsPresent(R[p]) \/ IsPresent(B[p]))
        /\ \A anc \in Ancestors(p) : anc \notin KindConflicts(L, R)}

\* Core 3-way decision match (sync.rs:80-147)
ActionFor(p, L, R, B, keepRemote, keepLocal) ==
    LET l == L[p]
        r == R[p]
        b == B[p]
    IN
    IF IsPresent(l) /\ IsPresent(r) /\ KindOf(l) /= KindOf(r) THEN
        "Conflict"
    ELSE IF IsPresent(l) /\ IsPresent(r) THEN
        IF l = r THEN "None"
        ELSE IF IsFile(b) /\ b = r THEN "Upload"
        ELSE IF IsFile(b) /\ b = l THEN "Download"
        ELSE "Conflict"
    ELSE IF IsPresent(l) /\ ~IsPresent(r) /\ ~IsPresent(b) THEN
        IF IsFile(l) THEN "Upload" ELSE "CreateRemoteFolder"
    ELSE IF ~IsPresent(l) /\ IsPresent(r) /\ ~IsPresent(b) THEN
        IF IsFile(r) THEN "Download" ELSE "CreateLocalFolder"
    ELSE IF ~IsPresent(l) /\ IsPresent(r) /\ IsPresent(b) THEN
        IF IsFile(r) THEN
            IF IsFile(b) /\ b = r THEN "DeleteRemote"
            ELSE IF BUG_MODE = "delete_overwrites_edit" THEN "DeleteRemote"
            ELSE "Download"
        ELSE \* r is Folder
            IF IsFolder(b) /\ (BUG_MODE = "drop_keep_folder_guard" \/ p \notin keepRemote)
            THEN "DeleteRemote"
            ELSE "CreateLocalFolder"
    ELSE IF IsPresent(l) /\ ~IsPresent(r) /\ IsPresent(b) THEN
        IF IsFile(l) THEN
            IF IsFile(b) /\ b = l THEN "DeleteLocal"
            ELSE IF BUG_MODE = "delete_overwrites_edit" THEN "DeleteLocal"
            ELSE "Upload"
        ELSE \* l is Folder
            IF IsFolder(b) /\ (BUG_MODE = "drop_keep_folder_guard" \/ p \notin keepLocal)
            THEN "DeleteLocal"
            ELSE "CreateRemoteFolder"
    ELSE
        "None"

\* Pass 2 (bottom-up, sync.rs:61-170): evaluate depth-1 descendants first to
\* populate keep_remote_folders and keep_local_folders, then evaluate depth-0 roots.
ChildAction(p, L, R, B) ==
    IF p \in ActivePaths(L, R, B)
    THEN ActionFor(p, L, R, B, {}, {})
    ELSE "None"

KeepRemoteFolders(L, R, B) ==
    {anc \in AllPaths :
        \E c \in ActivePaths(L, R, B) :
            /\ anc \in Ancestors(c)
            /\ IsPresent(R[c])
            /\ ChildAction(c, L, R, B) /= "DeleteRemote"}

KeepLocalFolders(L, R, B) ==
    {anc \in AllPaths :
        \E c \in ActivePaths(L, R, B) :
            /\ anc \in Ancestors(c)
            /\ IsPresent(L[c])
            /\ ChildAction(c, L, R, B) /= "DeleteLocal"}

PlanFolderSync(L, R, B) ==
    [p \in AllPaths |->
        IF p \notin ActivePaths(L, R, B) THEN "None"
        ELSE IF Depth(p) = 1 THEN ChildAction(p, L, R, B)
        ELSE ActionFor(p, L, R, B, KeepRemoteFolders(L, R, B), KeepLocalFolders(L, R, B))]

(* ------------------------------------------------------------------------- *)
(* State variables                                                           *)
(* ------------------------------------------------------------------------- *)

VARIABLES
    local,              \* [AllPaths -> Entries]: local mirror filesystem tree
    remote,             \* [AllPaths -> Entries]: server tree
    baseline,           \* [AllPaths -> Entries]: SyncWorker checkpoint baseline
    phase,              \* "idle" | "syncing"
    snapLocal,          \* Local snapshot at StartSync (SyncWorker.swift:163)
    snapRemote,         \* Remote snapshot at StartSync (SyncWorker.swift:162)
    plan,               \* [AllPaths -> Actions]: remaining actions in current sync cycle
    unavailable,        \* SUBSET AllPaths: failed/blocked paths in current sync cycle (SyncWorker.swift:196)
    editsLeft,          \* 0..MaxEdits: remaining user edit budget for quiescence
    requiredSurviving,  \* Ghost set SUBSET (BasePaths \X ContentVals): active user edits that must not be silently lost
    cleanDeleted        \* Ghost set SUBSET BasePaths: files cleanly deleted by one side and untouched by the other

vars == <<
    local, remote, baseline, phase, snapLocal, snapRemote, plan, unavailable,
    editsLeft, requiredSurviving, cleanDeleted
>>

(* ------------------------------------------------------------------------- *)
(* Representative synced initial states                                      *)
(* ------------------------------------------------------------------------- *)

EmptyTree == [p \in AllPaths |-> "NONE"]
TreeWithFolderAndFile ==
    [p \in AllPaths |->
        IF p = "a" THEN "DIR"
        ELSE IF p = "a/f" THEN "v1"
        ELSE IF p = "b" THEN "v1"
        ELSE "NONE"]
TreeWithEmptyFolder ==
    [p \in AllPaths |->
        IF p = "a" THEN "DIR"
        ELSE IF p = "b" THEN "v1"
        ELSE "NONE"]

InitTrees == {EmptyTree, TreeWithFolderAndFile, TreeWithEmptyFolder}

Init ==
    /\ \E t \in InitTrees :
           /\ local = t
           /\ remote = t
           /\ baseline = t
           /\ snapLocal = t
           /\ snapRemote = t
           /\ requiredSurviving = {<<p, t[p]>> : p \in {q \in BasePaths : IsFile(t[q])}}
    /\ phase = "idle"
    /\ plan = [p \in AllPaths |-> "None"]
    /\ unavailable = {}
    /\ editsLeft = MaxEdits
    /\ cleanDeleted = {}

(* ------------------------------------------------------------------------- *)
(* Helper: ghost-set updates for user edits                                  *)
(* ------------------------------------------------------------------------- *)

\* Removing old file content at p when side S intentionally supersedes or deletes it.
RemoveOld(req, p, oldVal, otherVal, baseVal) ==
    IF IsFile(oldVal) /\ (otherVal /= oldVal \/ baseVal = oldVal)
    THEN req \ {<<p, oldVal>>}
    ELSE req

\* Adding new file content v at p when side S creates or edits it.
\* Note: reverting S[p] back to baseline[p] while O[p] /= baseline[p] is a revert
\* to baseline rather than a new edit in content-hash 3-way sync.
AddNew(req, p, newVal, otherVal, baseVal) ==
    IF newVal = baseVal /\ otherVal /= baseVal
    THEN req
    ELSE req \cup {<<p, newVal>>}

\* Clean-delete tracking for a single file p deleted on side S.
UpdateCleanDel(cd, p, oldVal, otherVal, baseVal) ==
    IF IsFile(oldVal) /\ otherVal = oldVal /\ baseVal = oldVal
    THEN cd \cup {p}
    ELSE cd \ {p}

(* ------------------------------------------------------------------------- *)
(* User mutations (when phase = "idle" /\ editsLeft > 0)                     *)
(* ------------------------------------------------------------------------- *)

\* Edit or create a file at p \in BasePaths on local (or remote)
UserWriteFile(side, p, v) ==
    /\ phase = "idle"
    /\ editsLeft > 0
    /\ p \in BasePaths
    /\ v \in ContentVals
    /\ LET S == IF side = "local" THEN local ELSE remote
           O == IF side = "local" THEN remote ELSE local
       IN
       /\ ~IsFolder(S[p])
       /\ S[p] /= v
       /\ \A anc \in Ancestors(p) : IsFolder(S[anc])
       /\ LET newS == [S EXCEPT ![p] = v]
              req1 == RemoveOld(requiredSurviving, p, S[p], O[p], baseline[p])
              req2 == AddNew(req1, p, v, O[p], baseline[p])
          IN
          /\ local' = IF side = "local" THEN newS ELSE local
          /\ remote' = IF side = "remote" THEN newS ELSE remote
          /\ requiredSurviving' = req2
          /\ cleanDeleted' = cleanDeleted \ {p}
          /\ editsLeft' = editsLeft - 1
          /\ UNCHANGED <<baseline, phase, snapLocal, snapRemote, plan, unavailable>>

\* Delete a file at p \in BasePaths on local (or remote)
UserDeleteFile(side, p) ==
    /\ phase = "idle"
    /\ editsLeft > 0
    /\ p \in BasePaths
    /\ LET S == IF side = "local" THEN local ELSE remote
           O == IF side = "local" THEN remote ELSE local
       IN
       /\ IsFile(S[p])
       /\ LET newS == [S EXCEPT ![p] = "NONE"]
              req1 == RemoveOld(requiredSurviving, p, S[p], O[p], baseline[p])
              cd1  == UpdateCleanDel(cleanDeleted, p, S[p], O[p], baseline[p])
          IN
          /\ local' = IF side = "local" THEN newS ELSE local
          /\ remote' = IF side = "remote" THEN newS ELSE remote
          /\ requiredSurviving' = req1
          /\ cleanDeleted' = cd1
          /\ editsLeft' = editsLeft - 1
          /\ UNCHANGED <<baseline, phase, snapLocal, snapRemote, plan, unavailable>>

\* Create folder "a" on local (or remote)
UserCreateFolder(side) ==
    /\ phase = "idle"
    /\ editsLeft > 0
    /\ LET S == IF side = "local" THEN local ELSE remote
       IN
       /\ S["a"] = "NONE"
       /\ LET newS == [S EXCEPT !["a"] = "DIR"]
          IN
          /\ local' = IF side = "local" THEN newS ELSE local
          /\ remote' = IF side = "remote" THEN newS ELSE remote
          /\ cleanDeleted' = cleanDeleted \ {"a"}
          /\ editsLeft' = editsLeft - 1
          /\ UNCHANGED <<baseline, phase, snapLocal, snapRemote, plan, unavailable, requiredSurviving>>

\* Recursively delete folder "a" (and all its children) on local (or remote)
UserDeleteFolderSubtree(side) ==
    /\ phase = "idle"
    /\ editsLeft > 0
    /\ LET S == IF side = "local" THEN local ELSE remote
           O == IF side = "local" THEN remote ELSE local
           children == {c \in BasePaths : ParentOf(c) = "a"}
       IN
       /\ IsFolder(S["a"])
       /\ LET newS == [q \in AllPaths |->
                          IF q = "a" \/ ParentOf(q) = "a" THEN "NONE" ELSE S[q]]
              \* Remove any child file values that were cleanly superseded/deleted
              removedPairs ==
                  {<<c, S[c]>> : c \in {x \in children :
                      IsFile(S[x]) /\ (O[x] /= S[x] \/ baseline[x] = S[x])}}
              newlyClean ==
                  {c \in children : IsFile(S[c]) /\ O[c] = S[c] /\ baseline[c] = S[c]}
          IN
          /\ local' = IF side = "local" THEN newS ELSE local
          /\ remote' = IF side = "remote" THEN newS ELSE remote
          /\ requiredSurviving' = requiredSurviving \ removedPairs
          /\ cleanDeleted' = cleanDeleted \cup newlyClean
          /\ editsLeft' = editsLeft - 1
          /\ UNCHANGED <<baseline, phase, snapLocal, snapRemote, plan, unavailable>>

\* Kind change on "a": replace folder "a" (and its subtree) with a file "a" = v
UserReplaceFolderWithFile(side, v) ==
    /\ phase = "idle"
    /\ editsLeft > 0
    /\ v \in ContentVals
    /\ LET S == IF side = "local" THEN local ELSE remote
           O == IF side = "local" THEN remote ELSE local
           children == {c \in BasePaths : ParentOf(c) = "a"}
       IN
       /\ IsFolder(S["a"])
       /\ LET newS == [q \in AllPaths |->
                          IF q = "a" THEN v
                          ELSE IF ParentOf(q) = "a" THEN "NONE"
                          ELSE S[q]]
              removedPairs ==
                  {<<c, S[c]>> : c \in {x \in children :
                      IsFile(S[x]) /\ (O[x] /= S[x] \/ baseline[x] = S[x])}}
              newlyClean ==
                  {c \in children : IsFile(S[c]) /\ O[c] = S[c] /\ baseline[c] = S[c]}
              req1 == requiredSurviving \ removedPairs
              req2 == AddNew(req1, "a", v, O["a"], baseline["a"])
          IN
          /\ local' = IF side = "local" THEN newS ELSE local
          /\ remote' = IF side = "remote" THEN newS ELSE remote
          /\ requiredSurviving' = req2
          /\ cleanDeleted' = (cleanDeleted \ {"a"}) \cup newlyClean
          /\ editsLeft' = editsLeft - 1
          /\ UNCHANGED <<baseline, phase, snapLocal, snapRemote, plan, unavailable>>

\* Mid-sync concurrent remote edit on an existing file with a pending Upload
\* (exercises SyncWorker.swift:294-305 If-Match 409 -> preserveConflict).
\* Under BUG_MODE = "unconditional_delete_race", also permits racing against
\* a pending DeleteRemote (modeling SyncWorker.swift:263 lacking If-Match).
MidSyncRemoteEdit(p, v) ==
    /\ phase = "syncing"
    /\ editsLeft > 0
    /\ p \in BasePaths
    /\ v \in ContentVals
    /\ IsFile(snapRemote[p])
    /\ IsFile(remote[p])
    /\ remote[p] /= v
    /\ (plan[p] = "Upload"
        \/ (BUG_MODE = "unconditional_delete_race" /\ plan[p] = "DeleteRemote"))
    /\ LET req1 == RemoveOld(requiredSurviving, p, remote[p], local[p], baseline[p])
           req2 == AddNew(req1, p, v, local[p], baseline[p])
       IN
       /\ remote' = [remote EXCEPT ![p] = v]
       /\ requiredSurviving' = req2
       /\ cleanDeleted' = cleanDeleted \ {p}
       /\ editsLeft' = editsLeft - 1
       /\ UNCHANGED <<local, baseline, phase, snapLocal, snapRemote, plan, unavailable>>

(* ------------------------------------------------------------------------- *)
(* SyncWorker.sync reconciliation (SyncWorker.swift:156-365)                 *)
(* ------------------------------------------------------------------------- *)

StartSync ==
    /\ phase = "idle"
    /\ (local /= remote \/ baseline /= local)
    /\ LET p == PlanFolderSync(local, remote, baseline)
           \* SyncWorker.swift:183-192: prune paths absent from both sides,
           \* and immediately record paths already equal on both sides.
           newBase == [q \in AllPaths |->
                          IF local[q] = "NONE" /\ remote[q] = "NONE" THEN "NONE"
                          ELSE IF local[q] = remote[q] /\ IsPresent(local[q]) THEN local[q]
                          ELSE baseline[q]]
           hasWork == \E q \in AllPaths : p[q] /= "None"
       IN
       /\ baseline' = newBase
       /\ snapLocal' = local
       /\ snapRemote' = remote
       /\ plan' = p
       /\ unavailable' = {}
       /\ phase' = IF hasWork THEN "syncing" ELSE "idle"
       /\ UNCHANGED <<local, remote, editsLeft, requiredSurviving, cleanDeleted>>

\* Deterministic Pass 3 emission order (sync.rs:172-175):
\* Tier 1: Forward depth 0, Tier 2: Forward depth 1,
\* Tier 3: Delete  depth 1, Tier 4: Delete  depth 0.
PendingAt(tier) ==
    CASE tier = 1 -> {i \in 1..Len(PathOrder) :
                         LET q == PathOrder[i] IN Depth(q) = 0 /\ IsForward(plan[q])}
      [] tier = 2 -> {i \in 1..Len(PathOrder) :
                         LET q == PathOrder[i] IN Depth(q) = 1 /\ IsForward(plan[q])}
      [] tier = 3 -> {i \in 1..Len(PathOrder) :
                         LET q == PathOrder[i] IN Depth(q) = 1 /\ IsDelete(plan[q])}
      [] tier = 4 -> {i \in 1..Len(PathOrder) :
                         LET q == PathOrder[i] IN Depth(q) = 0 /\ IsDelete(plan[q])}

MinOf(S) == CHOOSE x \in S : \A y \in S : x <= y

NextTier ==
    IF PendingAt(1) /= {} THEN 1
    ELSE IF PendingAt(2) /= {} THEN 2
    ELSE IF PendingAt(3) /= {} THEN 3
    ELSE IF PendingAt(4) /= {} THEN 4
    ELSE 0

\* Helper: LocalMirror.preserveConflict (LocalMirror.swift:115-134) +
\* SyncWorker.saveConflict (SyncWorker.swift:330-340):
\* Picks an unoccupied sibling path (checking both local and remote) and moves
\* the local item (and subtree if folder "a") there, clearing p from baseline.
PreserveConflictLocal(L, R, p) ==
    IF p = "a" /\ IsFolder(L["a"]) THEN
        [q \in AllPaths |->
            IF q = "a" \/ ParentOf(q) = "a" THEN "NONE"
            ELSE IF q = "a_c" THEN "DIR"
            ELSE IF q = "a_c/f" THEN L["a/f"]
            ELSE IF HasG /\ q = "a_c/g" THEN L["a/g"]
            ELSE L[q]]
    ELSE IF p = "a" /\ (IsPresent(L["a_c"]) \/ IsPresent(R["a_c"])) THEN
        [L EXCEPT !["a_c2"] = L["a"], !["a"] = "NONE"]
    ELSE
        [L EXCEPT ![ConflictOf(p)] = L[p], ![p] = "NONE"]

ApplyNextAction ==
    /\ phase = "syncing"
    /\ NextTier > 0
    /\ LET idx == MinOf(PendingAt(NextTier))
           p   == PathOrder[idx]
           act == plan[p]
           \* SyncWorker.swift:200-206: skip action if blocked by unavailable ancestor/descendant
           blocked == IF IsDelete(act)
                      THEN \E c \in Descendants(p) : c \in unavailable
                      ELSE \E anc \in Ancestors(p) : anc \in unavailable
       IN
       IF blocked THEN
           /\ plan' = [plan EXCEPT ![p] = "None"]
           /\ unavailable' = unavailable \cup {p}
           /\ UNCHANGED <<local, remote, baseline, phase, snapLocal, snapRemote,
                          editsLeft, requiredSurviving, cleanDeleted>>
       ELSE
           CASE act = "CreateRemoteFolder" ->
               /\ remote' = [remote EXCEPT ![p] = "DIR"]
               /\ baseline' = [baseline EXCEPT ![p] = "DIR"]
               /\ plan' = [plan EXCEPT ![p] = "None"]
               /\ UNCHANGED <<local, phase, snapLocal, snapRemote, unavailable,
                              editsLeft, requiredSurviving, cleanDeleted>>
             [] act = "CreateLocalFolder" ->
               /\ local' = [local EXCEPT ![p] = "DIR"]
               /\ baseline' = [baseline EXCEPT ![p] = "DIR"]
               /\ plan' = [plan EXCEPT ![p] = "None"]
               /\ UNCHANGED <<remote, phase, snapLocal, snapRemote, unavailable,
                              editsLeft, requiredSurviving, cleanDeleted>>
             [] act = "Upload" ->
               \* If-Match CAS on existing remote file (SyncWorker.swift:284-305)
               IF IsPresent(snapRemote[p]) /\ remote[p] /= snapRemote[p] THEN
                   \* 409 ERR_CONFLICT -> saveConflict preserves local copy as sibling
                   /\ local' = PreserveConflictLocal(local, remote, p)
                   /\ baseline' = [baseline EXCEPT ![p] = "NONE"]
                   /\ plan' = [plan EXCEPT ![p] = "None"]
                   /\ UNCHANGED <<remote, phase, snapLocal, snapRemote, unavailable,
                                  editsLeft, requiredSurviving, cleanDeleted>>
               ELSE
                   /\ remote' = [remote EXCEPT ![p] = local[p]]
                   /\ baseline' = [baseline EXCEPT ![p] = local[p]]
                   /\ plan' = [plan EXCEPT ![p] = "None"]
                   /\ UNCHANGED <<local, phase, snapLocal, snapRemote, unavailable,
                                  editsLeft, requiredSurviving, cleanDeleted>>
             [] act = "Download" ->
               /\ local' = [local EXCEPT ![p] = remote[p]]
               /\ baseline' = [baseline EXCEPT ![p] = remote[p]]
               /\ plan' = [plan EXCEPT ![p] = "None"]
               /\ UNCHANGED <<remote, phase, snapLocal, snapRemote, unavailable,
                              editsLeft, requiredSurviving, cleanDeleted>>
             [] act = "DeleteRemote" ->
               \* Server rejects recursive=false delete on non-empty folder (db.rs:319-336 -> 409 ERR_NOT_EMPTY)
               IF IsFolder(remote[p]) /\ (\E c \in Descendants(p) : IsPresent(remote[c])) THEN
                   /\ plan' = [plan EXCEPT ![p] = "None"]
                   /\ unavailable' = unavailable \cup {p}
                   /\ UNCHANGED <<local, remote, baseline, phase, snapLocal, snapRemote,
                                  editsLeft, requiredSurviving, cleanDeleted>>
               ELSE
                   /\ remote' = [remote EXCEPT ![p] = "NONE"]
                   /\ baseline' = [baseline EXCEPT ![p] = "NONE"]
                   /\ plan' = [plan EXCEPT ![p] = "None"]
                   /\ UNCHANGED <<local, phase, snapLocal, snapRemote, unavailable,
                                  editsLeft, requiredSurviving, cleanDeleted>>
             [] act = "DeleteLocal" ->
               \* LocalMirror.delete rejects non-empty folder (LocalMirror.swift:150-155)
               IF IsFolder(local[p]) /\ (\E c \in Descendants(p) : IsPresent(local[c])) THEN
                   /\ plan' = [plan EXCEPT ![p] = "None"]
                   /\ unavailable' = unavailable \cup {p}
                   /\ UNCHANGED <<local, remote, baseline, phase, snapLocal, snapRemote,
                                  editsLeft, requiredSurviving, cleanDeleted>>
               ELSE
                   /\ local' = [local EXCEPT ![p] = "NONE"]
                   /\ baseline' = [baseline EXCEPT ![p] = "NONE"]
                   /\ plan' = [plan EXCEPT ![p] = "None"]
                   /\ UNCHANGED <<remote, phase, snapLocal, snapRemote, unavailable,
                                  editsLeft, requiredSurviving, cleanDeleted>>
             [] act = "Conflict" ->
               \* SyncWorker.saveConflict (SyncWorker.swift:324-340): moves local copy
               \* to sibling path and clears baseline[p], leaving rescan to converge.
               /\ local' = PreserveConflictLocal(local, remote, p)
               /\ baseline' = [baseline EXCEPT ![p] = "NONE"]
               /\ plan' = [plan EXCEPT ![p] = "None"]
               /\ UNCHANGED <<remote, phase, snapLocal, snapRemote, unavailable,
                              editsLeft, requiredSurviving, cleanDeleted>>

FinishSync ==
    /\ phase = "syncing"
    /\ NextTier = 0
    /\ phase' = "idle"
    /\ unavailable' = {}
    /\ UNCHANGED <<local, remote, baseline, snapLocal, snapRemote, plan,
                   editsLeft, requiredSurviving, cleanDeleted>>

\* Stuttering step once converged (keeps the transition system deadlock-free)
ConvergedIdle ==
    /\ phase = "idle"
    /\ local = remote
    /\ baseline = local
    /\ UNCHANGED vars

(* ------------------------------------------------------------------------- *)
(* Next-state relation and fairness                                          *)
(* ------------------------------------------------------------------------- *)

SyncProgress == StartSync \/ ApplyNextAction \/ FinishSync

Next ==
    \/ \E s \in {"local", "remote"}, p \in BasePaths, v \in ContentVals : UserWriteFile(s, p, v)
    \/ \E s \in {"local", "remote"}, p \in BasePaths : UserDeleteFile(s, p)
    \/ \E s \in {"local", "remote"} : UserCreateFolder(s)
    \/ \E s \in {"local", "remote"} : UserDeleteFolderSubtree(s)
    \/ \E s \in {"local", "remote"}, v \in ContentVals : UserReplaceFolderWithFile(s, v)
    \/ \E p \in BasePaths, v \in ContentVals : MidSyncRemoteEdit(p, v)
    \/ SyncProgress
    \/ ConvergedIdle

Spec == Init /\ [][Next]_vars /\ WF_vars(SyncProgress)

(* ------------------------------------------------------------------------- *)
(* Safety Invariants                                                         *)
(* ------------------------------------------------------------------------- *)

TypeOK ==
    /\ local \in [AllPaths -> Entries]
    /\ remote \in [AllPaths -> Entries]
    /\ baseline \in [AllPaths -> Entries]
    /\ phase \in {"idle", "syncing"}
    /\ snapLocal \in [AllPaths -> Entries]
    /\ snapRemote \in [AllPaths -> Entries]
    /\ plan \in [AllPaths -> Actions]
    /\ unavailable \subseteq AllPaths
    /\ editsLeft \in 0..MaxEdits
    /\ requiredSurviving \subseteq (BasePaths \X ContentVals)
    /\ cleanDeleted \subseteq BasePaths

\* Every present entry in both local and remote trees has its parent present as a folder.
TreeWellFormed ==
    /\ \A p \in AllPaths :
           IsPresent(local[p]) => \A anc \in Ancestors(p) : IsFolder(local[anc])
    /\ \A p \in AllPaths :
           IsPresent(remote[p]) => \A anc \in Ancestors(p) : IsFolder(remote[anc])

\* Every active user edit survives either on local or remote at its original path,
\* its conflict sibling, or under the moved conflict folder (a_c/*).
NoSilentEditLoss ==
    \A pair \in requiredSurviving :
        \E q \in AllowedLocations(pair[1]) :
            local[q] = pair[2] \/ remote[q] = pair[2]

\* A cleanly deleted file (deleted on one side, unchanged on the other, not recreated)
\* is never resurrected by sync (never planned for Upload/Download, absent once converged).
NoResurrectionOfCleanDelete ==
    \A p \in cleanDeleted :
        /\ (phase = "syncing" => plan[p] \notin {"Upload", "Download"})
        /\ (phase = "idle" /\ local = remote => local[p] = "NONE" /\ remote[p] = "NONE")

\* plan_folder_sync never plans DeleteRemote/DeleteLocal on a folder whose descendant
\* on that side is being kept (keep_remote_folders / keep_local_folders guard, sync.rs:119,137).
NonEmptyFolderProtected ==
    /\ \A p \in AllPaths :
           (phase = "syncing" /\ plan[p] = "DeleteRemote" /\ IsFolder(snapRemote[p])) =>
               \A c \in Descendants(p) :
                   IsPresent(snapRemote[c]) =>
                       (plan[c] = "DeleteRemote" \/ remote[c] = "NONE")
    /\ \A p \in AllPaths :
           (phase = "syncing" /\ plan[p] = "DeleteLocal" /\ IsFolder(snapLocal[p])) =>
               \A c \in Descendants(p) :
                   IsPresent(snapLocal[c]) =>
                       (plan[c] = "DeleteLocal" \/ local[c] = "NONE")

(* ------------------------------------------------------------------------- *)
(* Temporal Liveness Property                                                *)
(* ------------------------------------------------------------------------- *)

\* Once the bounded user edits finish, fair sync cycles eventually converge
\* local, remote, and baseline to identical well-formed trees.
EventualConvergence ==
    <>[] (phase = "idle" /\ local = remote /\ baseline = local)

=============================================================================

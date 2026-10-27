---------------------------- MODULE LockSession ----------------------------
(***************************************************************************)
(* The per-target build protocol of redo-msh for ONE target, across one or *)
(* more SESSIONS, with crashes, source edits, coarse file timestamps,      *)
(* forced builds, phony targets and the static-source rule (src/build.rs:  *)
(* ensure, is_target, is_ood, current_csum, build, build_inner,            *)
(* check_not_hand_edited).                                                 *)
(*                                                                         *)
(* Scope: redo's OWN correctness -- database/disk consistency, no stale    *)
(* builds, never refusing or misclassifying its own output. People editing *)
(* built outputs while builds run is an accepted limitation (README.md),   *)
(* so the model has no hand edits of the output.                           *)
(*                                                                         *)
(* Each process belongs to a session (a top-level `redo` invocation; its   *)
(* threads and child redo processes share it). A non-forced process (a     *)
(* redo-ifchange) runs:                                                    *)
(*                                                                         *)
(*   start     - is_target: an existing file with no files row, with only  *)
(*               a default*.do matching (~ExactDoFile), is recorded as a   *)
(*               static SOURCE. Otherwise ensure(): skip if runid = our    *)
(*               session; else the UNLOCKED is_ood -- out of date -> lock; *)
(*               up to date -> mark verified.                              *)
(*   acquire   - build(): blocking per-target kernel file lock.            *)
(*   check2    - the double-check under the lock (runid = our session;     *)
(*               RecheckOod also re-runs is_ood).                          *)
(*   guard     - build_inner: the static-source rule again, then           *)
(*               check_not_hand_edited (a refusal is final).               *)
(*   markertxn - one transaction: clear deps, set the Uncommitted marker.  *)
(*   stamp     - the do-file's redo-ifchange records the source edge (and  *)
(*               refreshes the source's stamp cache) ...                   *)
(*   read      - ... then the do-file reads the source and writes $3 (or,  *)
(*               for a Phony target, writes nothing).                      *)
(*   pendingw  - (HashBeforeRename) hash $3, record it as pending.         *)
(*   rename    - atomic rename of $3 over the target (phony: remove it).   *)
(*   commit    - one transaction: record the output hash, drop the marker, *)
(*               set runid; unlock.                                        *)
(*                                                                         *)
(* A FORCED process (a top-level `redo <target>`) starts at acquire, never *)
(* skips at check2, and bypasses the static-source rule. Source edits      *)
(* happen at any moment (bounded); with CoarseTicks, writes share a        *)
(* timestamp until a Tick advances the clock (filesystem timestamp         *)
(* granularity), otherwise every write gets a fresh timestamp. `Always`    *)
(* models redo-always; a redo-ifcreate path appearing is a source edit.    *)
(*                                                                         *)
(* The stamp cache (current_csum): is_ood trusts the cached hash of the    *)
(* source when its mtime matches the cached mtime. RacyGitRule (the fix)   *)
(* trusts it only if the hash was taken in a LATER tick than the mtime it  *)
(* recorded -- git's "racily clean" rule.                                  *)
(*                                                                         *)
(* Fix switches: RecheckOod; HashBeforeRename (record the hash of $3 as    *)
(* pending before the rename; the guard and the static rule accept a file  *)
(* matching it; the marker transaction adopts an accepted pending hash);   *)
(* RacyGitRule.                                                            *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   Mutex / LockHeld, MarkerDiscipline                                    *)
(*   NoRedundantBuild    - a non-forced build starts only if the target is *)
(*                         truly out of date at that moment.               *)
(*   NoFalseRefusal      - the guard never refuses (there are no hand      *)
(*                         edits in this model, so every refusal would be  *)
(*                         a refusal of redo's own output).                *)
(*   NoOwnOutputAsSource - redo never records its own output as a source.  *)
(*   DiskDbAgree         - with no build in flight (no marker), the        *)
(*                         database describes what is on disk.             *)
(*   UpToDateIsTrue      - if is_ood (as the code computes it, cache and   *)
(*                         all) says up to date, the output was built from *)
(*                         the CURRENT source (no silent staleness).       *)
(*   Termination                                                           *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Procs, SessionOf, Forced, MaxSrcEdits, CoarseTicks, MaxTicks,
          ExactDoFile, Phony, Always, RecheckOod, HashBeforeRename,
          RacyGitRule

ASSUME /\ SessionOf \in [Procs -> Nat \ {0}]
       /\ Forced \subseteq Procs
       /\ MaxSrcEdits \in 0..9 /\ MaxTicks \in Nat
       /\ \A b \in {CoarseTicks, ExactDoFile, Phony, Always, RecheckOod,
                    HashBeforeRename, RacyGitRule} : b \in BOOLEAN

\* --- instances, selected from the .cfg files via <- ---------------------
OneProc == {"p1"}
TwoProcs == {"p1", "p2"}
ThreeProcs == {"p1", "p2", "p3"}
OneSession1 == ("p1" :> 1)
OneSession2 == ("p1" :> 1) @@ ("p2" :> 1)
OneSession3 == ("p1" :> 1) @@ ("p2" :> 1) @@ ("p3" :> 1)
TwoSessions2 == ("p1" :> 1) @@ ("p2" :> 2)
TwoSessions3 == ("p1" :> 1) @@ ("p2" :> 1) @@ ("p3" :> 2)
NoneForced == {}
P2Forced == {"p2"}
P3Forced == {"p3"}

FREE == "FREE"
\* Sentinels, encoded as integers outside the source-version range (TLC
\* cannot compare integers with strings).
PHONYREC == 97   \* files row exists (generated) with csum NULL: phony
NONE == 98       \* no recorded source edge (cleared by the marker txn)

\* Content ids: 0 = no file; 1 = output of a build before the model starts;
\* each process builds at most once.
MaxId == 1 + Cardinality(Procs)
Ids == 0..MaxId
RecVals == Ids \cup {PHONYREC}

VARIABLES
    pc,           \* [Procs -> phase]
    lock,         \* holder of the per-target lock, or FREE
    \* filesystem
    disk,         \* content id of the target file (0 = absent)
    src,          \* current source version
    srcTick,      \* the source's mtime
    madeFrom,     \* [Ids -> source version | NONE]: provenance of each output
    nextId,       \* next content id to mint
    now,          \* the filesystem clock (timestamp granularity)
    \* database
    rec,          \* files.csum: content redo vouches for (0 = no row; PHONYREC)
    marker,       \* the Uncommitted marker row
    pending,      \* hash of $3 recorded before the rename (0 = none)
    runid,        \* session that last built or verified the target (0 = none)
    edge,         \* source version recorded on the target's dep edge, or NONE
    static,       \* files row with dofile NULL: the target is a static source
    cache,        \* the source's stamp cache: [tick, ver, at]
    \* per process
    gview,        \* [Procs -> Ids]: the content the guard examined
    out,          \* [Procs -> Ids]: the $3 output (0 = phony: none)
    \* environment budget
    srcEdits,
    \* history
    redundant, refusedFlag, adoptedOwn

fsv == <<disk, src, srcTick, madeFrom, nextId, now>>
dbv == <<rec, marker, pending, runid, edge, static, cache>>
pv == <<gview, out>>
hv == <<redundant, refusedFlag, adoptedOwn>>
vars == <<pc, lock, fsv, dbv, pv, srcEdits, hv>>

Terminal == {"done", "refused", "crashed"}
Locked == {"check2", "guard", "markertxn", "stamp", "read", "pendingw",
           "rename", "commit"}
Phases == {"start", "acquire"} \cup Locked \cup Terminal

TypeOK ==
    /\ pc \in [Procs -> Phases]
    /\ lock \in Procs \cup {FREE}
    /\ disk \in Ids /\ src \in 0..MaxSrcEdits
    /\ srcTick \in Nat /\ now \in Nat
    /\ madeFrom \in [Ids -> (0..MaxSrcEdits) \cup {NONE}]
    /\ nextId \in 2..(MaxId + 1)
    /\ rec \in RecVals /\ marker \in BOOLEAN /\ pending \in Ids
    /\ runid \in {0} \cup {SessionOf[p] : p \in Procs}
    /\ edge \in (0..MaxSrcEdits) \cup {NONE}
    /\ static \in BOOLEAN
    /\ cache \in [tick : Nat, ver : 0..MaxSrcEdits, at : Nat]
    /\ gview \in [Procs -> Ids] /\ out \in [Procs -> Ids]
    /\ srcEdits \in 0..MaxSrcEdits
    /\ redundant \in BOOLEAN /\ refusedFlag \in BOOLEAN /\ adoptedOwn \in BOOLEAN

(* Two starting points: never built, or built cleanly from source 0. The
   source was written at tick 0; the clock is at 1; the source's stamp was
   cached at tick 1 (trustworthy). *)
Init ==
    /\ pc = [p \in Procs |-> IF p \in Forced THEN "acquire" ELSE "start"]
    /\ lock = FREE
    /\ \E built \in BOOLEAN :
         /\ disk = IF built /\ ~Phony THEN 1 ELSE 0
         /\ rec = IF ~built THEN 0 ELSE IF Phony THEN PHONYREC ELSE 1
         /\ edge = IF built THEN 0 ELSE NONE
    /\ src = 0 /\ srcTick = 0 /\ now = 1
    /\ madeFrom = [i \in Ids |-> IF i = 1 THEN 0 ELSE NONE]
    /\ nextId = 2
    /\ marker = FALSE /\ pending = 0 /\ runid = 0 /\ static = FALSE
    /\ cache = [tick |-> 0, ver |-> 0, at |-> 1]
    /\ gview = [p \in Procs |-> 0] /\ out = [p \in Procs |-> 0]
    /\ srcEdits = 0
    /\ redundant = FALSE /\ refusedFlag = FALSE /\ adoptedOwn = FALSE

----------------------------------------------------------------------------
(* The stamp cache and the two views of out-of-dateness. *)
Trusted == ~RacyGitRule \/ cache.at > cache.tick
FastHit == srcTick = cache.tick /\ Trusted
CurSrc == IF FastHit THEN cache.ver ELSE src            \* current_csum
CacheAfterOod == IF FastHit THEN cache
                 ELSE [tick |-> srcTick, ver |-> src, at |-> now]

Missing == rec # PHONYREC /\ disk = 0        \* built output gone (not phony)

(* is_ood as the code computes it (through the cache) ... *)
OOD == Always \/ rec = 0 \/ marker \/ Missing \/ edge # CurSrc
(* ... and the truth. *)
TrueOOD == Always \/ rec = 0 \/ marker \/ Missing \/ edge # src

(* A pending hash vouches for the file on disk (HashBeforeRename). *)
PendingVouches == HashBeforeRename /\ pending # 0 /\ disk = pending

(* djb's static-file rule as is_target / build_inner apply it. *)
StaticRule == ~ExactDoFile /\ disk # 0 /\ rec = 0 /\ ~PendingVouches

(* check_not_hand_edited's verdict (generated phony targets are exempt). *)
GuardOk == disk = 0 \/ disk = rec \/ rec = PHONYREC \/ PendingVouches

Unlock == lock' = FREE

(* Record the target as a static source. *)
Adopt == /\ static' = TRUE
         /\ adoptedOwn' = (adoptedOwn \/ ~static)

----------------------------------------------------------------------------
(* is_target, then ensure(). *)
Check1(p) ==
    /\ pc[p] = "start"
    /\ IF static \/ StaticRule
         THEN /\ pc' = [pc EXCEPT ![p] = "done"]
              /\ Adopt
              /\ runid' = SessionOf[p]
              /\ UNCHANGED <<cache, redundant, refusedFlag>>
         ELSE /\ IF runid = SessionOf[p]
                   THEN /\ pc' = [pc EXCEPT ![p] = "done"]
                        /\ UNCHANGED <<runid, cache>>
                   ELSE /\ cache' = CacheAfterOod
                        /\ IF OOD
                             THEN /\ pc' = [pc EXCEPT ![p] = "acquire"]
                                  /\ UNCHANGED runid
                             ELSE /\ pc' = [pc EXCEPT ![p] = "done"]
                                  /\ runid' = SessionOf[p]   \* mark_verified
              /\ UNCHANGED <<static, hv>>
    /\ UNCHANGED <<lock, fsv, rec, marker, pending, edge, pv, srcEdits>>

Acquire(p) ==
    /\ pc[p] = "acquire"
    /\ lock = FREE
    /\ lock' = p
    /\ pc' = [pc EXCEPT ![p] = "check2"]
    /\ UNCHANGED <<fsv, dbv, pv, srcEdits, hv>>

(* The double-check under the lock (a forced build never skips). *)
Check2(p) ==
    /\ pc[p] = "check2"
    /\ IF p \notin Forced /\ (runid = SessionOf[p] \/ (RecheckOod /\ ~OOD))
         THEN /\ pc' = [pc EXCEPT ![p] = "done"]
              /\ runid' = SessionOf[p]
              /\ cache' = IF RecheckOod THEN CacheAfterOod ELSE cache
              /\ Unlock
              /\ UNCHANGED redundant
         ELSE /\ pc' = [pc EXCEPT ![p] = "guard"]
              /\ redundant' = (redundant \/ (p \notin Forced /\ ~TrueOOD))
              /\ UNCHANGED <<lock, runid, cache>>
    /\ UNCHANGED <<fsv, rec, marker, pending, edge, static, pv, srcEdits,
                   refusedFlag, adoptedOwn>>

(* build_inner: the static rule (non-forced), then the overwrite guard. *)
Guard(p) ==
    /\ pc[p] = "guard"
    /\ IF p \notin Forced /\ StaticRule
         THEN /\ pc' = [pc EXCEPT ![p] = "done"]
              /\ Adopt
              /\ Unlock
              /\ UNCHANGED <<gview, refusedFlag>>
         ELSE IF GuardOk
           THEN /\ pc' = [pc EXCEPT ![p] = "markertxn"]
                /\ gview' = [gview EXCEPT ![p] = disk]
                /\ UNCHANGED <<lock, static, refusedFlag, adoptedOwn>>
           ELSE /\ pc' = [pc EXCEPT ![p] = "refused"]
                /\ refusedFlag' = TRUE
                /\ Unlock
                /\ UNCHANGED <<gview, static, adoptedOwn>>
    /\ UNCHANGED <<fsv, rec, marker, pending, runid, edge, cache, out,
                   srcEdits, redundant>>

(* Clear deps + set the marker; adopt a pending hash the guard accepted. *)
AdoptPending(p) == HashBeforeRename /\ pending # 0 /\ gview[p] = pending

MarkerTxn(p) ==
    /\ pc[p] = "markertxn"
    /\ marker' = TRUE
    /\ edge' = NONE
    /\ rec' = IF AdoptPending(p) THEN pending ELSE rec
    /\ pending' = IF HashBeforeRename THEN 0 ELSE pending
    /\ pc' = [pc EXCEPT ![p] = "stamp"]
    /\ UNCHANGED <<lock, fsv, runid, static, cache, pv, srcEdits, hv>>

(* redo-ifchange: hash the source, record the edge, refresh the cache. *)
Stamp(p) ==
    /\ pc[p] = "stamp"
    /\ edge' = src
    /\ cache' = [tick |-> srcTick, ver |-> src, at |-> now]
    /\ pc' = [pc EXCEPT ![p] = "read"]
    /\ UNCHANGED <<lock, fsv, rec, marker, pending, runid, static, pv,
                   srcEdits, hv>>

(* The do-file reads the source (after its redo-ifchange), writes $3. *)
Read(p) ==
    /\ pc[p] = "read"
    /\ IF Phony
         THEN /\ out' = [out EXCEPT ![p] = 0]
              /\ UNCHANGED <<madeFrom, nextId>>
         ELSE /\ out' = [out EXCEPT ![p] = nextId]
              /\ madeFrom' = [madeFrom EXCEPT ![nextId] = src]
              /\ nextId' = nextId + 1
    /\ pc' = [pc EXCEPT ![p] = IF HashBeforeRename THEN "pendingw" ELSE "rename"]
    /\ UNCHANGED <<lock, disk, src, srcTick, now, dbv, gview, srcEdits, hv>>

PendingW(p) ==
    /\ pc[p] = "pendingw"
    /\ pending' = out[p]
    /\ pc' = [pc EXCEPT ![p] = "rename"]
    /\ UNCHANGED <<lock, fsv, rec, marker, runid, edge, static, cache, pv,
                   srcEdits, hv>>

(* Rename $3 over the target, or remove the target if there is no output. *)
Rename(p) ==
    /\ pc[p] = "rename"
    /\ disk' = out[p]
    /\ pc' = [pc EXCEPT ![p] = "commit"]
    /\ UNCHANGED <<lock, src, srcTick, madeFrom, nextId, now, dbv, pv,
                   srcEdits, hv>>

(* The commit transaction, then unlock. *)
Commit(p) ==
    /\ pc[p] = "commit"
    /\ rec' = IF out[p] = 0 THEN PHONYREC ELSE out[p]
    /\ marker' = FALSE
    /\ pending' = 0
    /\ static' = FALSE
    /\ runid' = SessionOf[p]
    /\ Unlock
    /\ pc' = [pc EXCEPT ![p] = "done"]
    /\ UNCHANGED <<fsv, edge, cache, pv, srcEdits, hv>>

(* A process dies (or its transaction fails) at any point; the OS releases
   its kernel lock; the database and disk keep what was written. *)
Crash(p) ==
    /\ pc[p] \notin Terminal
    /\ pc' = [pc EXCEPT ![p] = "crashed"]
    /\ lock' = IF lock = p THEN FREE ELSE lock
    /\ UNCHANGED <<fsv, dbv, pv, srcEdits, hv>>

(* Environment: a user edits the source; the clock ticks (only matters
   with CoarseTicks). *)
SrcEdit ==
    /\ srcEdits < MaxSrcEdits
    /\ src' = src + 1
    /\ srcTick' = IF CoarseTicks THEN now ELSE now + 1
    /\ now' = IF CoarseTicks THEN now ELSE now + 1
    /\ srcEdits' = srcEdits + 1
    /\ UNCHANGED <<pc, lock, disk, madeFrom, nextId, dbv, pv, hv>>

Tick ==
    /\ CoarseTicks
    /\ now < 1 + MaxTicks
    /\ now' = now + 1
    /\ UNCHANGED <<pc, lock, disk, src, srcTick, madeFrom, nextId, dbv, pv,
                   srcEdits, hv>>

Step(p) ==
    \/ Check1(p) \/ Acquire(p) \/ Check2(p) \/ Guard(p) \/ MarkerTxn(p)
    \/ Stamp(p) \/ Read(p) \/ PendingW(p) \/ Rename(p) \/ Commit(p)

AllTerminal == \A p \in Procs : pc[p] \in Terminal

Terminating == AllTerminal /\ UNCHANGED vars

Next ==
    \/ \E p \in Procs : Step(p) \/ Crash(p)
    \/ SrcEdit \/ Tick
    \/ Terminating

(* Each live process eventually acts; crashes, edits and ticks are never
   forced. *)
Spec == Init /\ [][Next]_vars /\ (\A p \in Procs : WF_vars(Step(p)))

----------------------------------------------------------------------------
Mutex == Cardinality({p \in Procs : pc[p] \in Locked}) <= 1

LockHeld == \A p \in Procs : pc[p] \in Locked => lock = p

MarkerDiscipline ==
    \A p \in Procs :
        pc[p] \in {"stamp", "read", "pendingw", "rename", "commit"} => marker

NoRedundantBuild == ~redundant

NoFalseRefusal == ~refusedFlag

NoOwnOutputAsSource == ~adoptedOwn

DiskDbAgree ==
    (~marker /\ rec # 0) =>
        IF rec = PHONYREC THEN disk = 0 ELSE disk = rec

UpToDateIsTrue ==
    (~OOD /\ disk = rec /\ disk # 0) => madeFrom[disk] = src

(* Coverage, EXPECTED TO BE VIOLATED in LockSession_Coverage: a crash
   between the rename and the commit really is explored (a new output on
   disk that nobody is about to commit), so NoFalseRefusal and
   NoOwnOutputAsSource are not passing on a model that never reaches the
   window they are about. *)
NeverCommitWindowLeftover ==
    ~(disk # 0 /\ disk # rec /\ \A p \in Procs : pc[p] # "commit")

Termination == <>AllTerminal

============================================================================

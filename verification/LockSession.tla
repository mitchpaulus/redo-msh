---------------------------- MODULE LockSession ----------------------------
(***************************************************************************)
(* The per-target build-exclusion protocol of redo-msh for ONE target and  *)
(* one session, with crashes (src/build.rs: ensure, build, build_inner):   *)
(*                                                                         *)
(*   check1  - ensure() reads the target's runid WITHOUT the lock and      *)
(*             skips if it was built this session.                         *)
(*   acquire - build() takes the per-target kernel file lock (blocking).   *)
(*   check2  - the double-check: re-read runid under the lock; whoever     *)
(*             held the lock before us may have built the target. If not   *)
(*             built, atomically clear deps and set the Uncommitted        *)
(*             marker (one SQLite transaction), then run the do-file.      *)
(*   commit  - post-build stamp + drop the marker, atomically; release.    *)
(*                                                                         *)
(* Crash: a process may die at any point. The OS releases its file lock;   *)
(* the database keeps whatever was committed (marker included).            *)
(*                                                                         *)
(* Abstractions: SQLite transactions are atomic actions; the do-file body  *)
(* is the "run" state; force (top-level redo) is not modeled -- every      *)
(* process is an ifchange-style non-forced builder.                        *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   Mutex            - at most one process between lock-acquire and       *)
(*                      commit (no two builders of one target).            *)
(*   AtMostOnce       - the target commits at most once per session,       *)
(*                      despite racing processes (the double-check works). *)
(*   NoDirtyCommit    - the session never records "built" without a        *)
(*                      completed build.                                   *)
(*   MarkerDiscipline - a build in flight always has the Uncommitted       *)
(*                      marker set, so a crash mid-build leaves the target *)
(*                      unconditionally out of date.                       *)
(*   RunidCleanMarker - a committed target never carries the marker.       *)
(*   Progress         - unless every process crashes, the target gets      *)
(*                      built (liveness; crashes are never forced).        *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANT Procs

\* Instantiated from the .cfg via  Procs <- ThreeProcs
ThreeProcs == {"p1", "p2", "p3"}

FREE == "FREE"

VARIABLES
    pc,      \* [Procs -> phase]
    lock,    \* holder of the per-target lock, or FREE
    runid,   \* TRUE iff the target committed this session
    marker,  \* the Uncommitted (build-in-flight) marker row
    builds   \* completed builds this session (history, for invariants)

vars == <<pc, lock, runid, marker, builds>>

Phases == {"start", "acquire", "check2", "run", "done", "crashed"}

TypeOK ==
    /\ pc \in [Procs -> Phases]
    /\ lock \in Procs \cup {FREE}
    /\ runid \in BOOLEAN
    /\ marker \in BOOLEAN
    /\ builds \in 0..Cardinality(Procs)

Init ==
    /\ pc = [p \in Procs |-> "start"]
    /\ lock = FREE
    /\ runid = FALSE
    /\ marker = FALSE
    /\ builds = 0

(* ensure(): unlocked session check. *)
Check1(p) ==
    /\ pc[p] = "start"
    /\ pc' = [pc EXCEPT ![p] = IF runid THEN "done" ELSE "acquire"]
    /\ UNCHANGED <<lock, runid, marker, builds>>

(* build(): blocking per-target lock (enabled only while free). *)
Acquire(p) ==
    /\ pc[p] = "acquire"
    /\ lock = FREE
    /\ lock' = p
    /\ pc' = [pc EXCEPT ![p] = "check2"]
    /\ UNCHANGED <<runid, marker, builds>>

(* The double-check under the lock; if stale, the clear-deps + marker
   transaction runs and the do-file starts. *)
Check2(p) ==
    /\ pc[p] = "check2"
    /\ IF runid
         THEN /\ lock' = FREE
              /\ pc' = [pc EXCEPT ![p] = "done"]
              /\ UNCHANGED marker
         ELSE /\ marker' = TRUE
              /\ pc' = [pc EXCEPT ![p] = "run"]
              /\ UNCHANGED lock
    /\ UNCHANGED <<runid, builds>>

(* The post-build commit transaction: stamp + drop marker, then unlock. *)
Commit(p) ==
    /\ pc[p] = "run"
    /\ runid' = TRUE
    /\ marker' = FALSE
    /\ builds' = builds + 1
    /\ lock' = FREE
    /\ pc' = [pc EXCEPT ![p] = "done"]

(* A process dies at any point; the OS releases its kernel file lock. *)
Crash(p) ==
    /\ pc[p] \in {"start", "acquire", "check2", "run"}
    /\ pc' = [pc EXCEPT ![p] = "crashed"]
    /\ lock' = IF lock = p THEN FREE ELSE lock
    /\ UNCHANGED <<runid, marker, builds>>

Terminal == \A p \in Procs : pc[p] \in {"done", "crashed"}

Terminating == Terminal /\ UNCHANGED vars

Next ==
    \/ \E p \in Procs :
         Check1(p) \/ Acquire(p) \/ Check2(p) \/ Commit(p) \/ Crash(p)
    \/ Terminating

(* Fairness on everything except Crash: a live process eventually acts,
   but nothing ever forces a crash. *)
Spec ==
    /\ Init
    /\ [][Next]_vars
    /\ WF_vars(\E p \in Procs : Check1(p) \/ Acquire(p) \/ Check2(p) \/ Commit(p))

----------------------------------------------------------------------------
Mutex == Cardinality({p \in Procs : pc[p] \in {"check2", "run"}}) <= 1

AtMostOnce == builds <= 1

NoDirtyCommit == runid => (builds >= 1)

MarkerDiscipline == (\E p \in Procs : pc[p] = "run") => marker

RunidCleanMarker == runid => ~marker

Progress == <>((builds >= 1) \/ (\A p \in Procs : pc[p] = "crashed"))

============================================================================

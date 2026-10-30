--------------------------- MODULE ShallowRecheck ---------------------------
(***************************************************************************)
(* The double-check under a target's lock, across concurrent redo runs,   *)
(* with real content: when a run has waited for a target's lock, may it   *)
(* skip the build because another run just brought the target up to date? *)
(* (src/build.rs: build, is_ood_shallow; the checker is                    *)
(* parallel::check_recorded, before that main's is_ood.)                   *)
(*                                                                         *)
(* CycleLock and LockSession each cover half of this. CycleLock has two    *)
(* sessions over a dependency graph, but nothing ever changes, so its      *)
(* re-check is an oracle (`fresh`). LockSession has edits at any moment,   *)
(* but one target over one source, so there is no buildable dependency for *)
(* a shallow re-check to misjudge. The failure needs both.                 *)
(*                                                                         *)
(* Content is modeled exactly: a source's content is its version; a        *)
(* target's content IS the record of the dependency contents its do-file   *)
(* read (deterministic do-files), which is also what the database records  *)
(* as its edges. So a target built over a stale dependency is itself       *)
(* stale, transitively, and "truly up to date" is decidable (TrueUTD).     *)
(*                                                                         *)
(* A session is one top-level run ensuring Root, with a per-session task   *)
(* registry (one task per target per session). A task:                    *)
(*   check   - the unlocked out-of-date check: skip if this session        *)
(*             already settled it; otherwise examine evidence in ANY       *)
(*             order and stop at the first out-of-date sign (a build in    *)
(*             flight, a changed source, a buildable dep whose settled     *)
(*             content differs). Buildable deps are settled through their  *)
(*             own tasks, which the checker may start at any moment and    *)
(*             which keep running after it gives up. This one action set   *)
(*             covers main's is_ood (in order, starting a dep task only    *)
(*             when it reaches it) and this branch's checker (start every  *)
(*             recorded dep at once, return on the first evidence). If    *)
(*             every dep checks out, mark verified (no lock, as            *)
(*             mark_verified).                                             *)
(*   lock    - the blocking per-target lock.                               *)
(*   recheck - the double-check, by `Recheck`:                             *)
(*               "none"     skip only if this session settled it           *)
(*                          (this branch before the merge);                *)
(*               "shallow"  ... or if every recorded edge matches what is  *)
(*                          on disk now (main's is_ood_shallow);           *)
(*               "settled"  ... and additionally every buildable dep has   *)
(*                          settled in THIS session (the merged code);     *)
(*               "await"    wait (lock held) until every buildable dep has *)
(*                          settled in this session, then as "settled"     *)
(*                          (an alternative; not implemented).             *)
(*   SpawnFirst: the checker starts every recorded buildable dep before it *)
(*   looks at any evidence, as this branch's checker does (activate_spec   *)
(*   precedes its early return). FALSE also allows main's in-order walk.   *)
(*   build   - the do-file walk under the lock: for each dep in order,     *)
(*             redo-ifchange it (a buildable dep: demand its task and wait *)
(*             for it to settle) and read its content; then commit.        *)
(* Sessions start at any time; sources are edited at any time (bounded).   *)
(*                                                                         *)
(* Properties:                                                             *)
(*   NoStaleRun       - a run during which no source was edited ends with  *)
(*                      every target it settled truly up to date. (Edits   *)
(*                      DURING a run may be missed by any build system.)   *)
(*   NoRedundantBuild - a build starts only if the target is truly out of  *)
(*                      date at that moment (history flag `redundant`).    *)
(*   NeverSkipAtRecheck (coverage) - the re-check never skips on content;  *)
(*                      violated exactly when the re-check does its job.   *)
(*   Termination      - every session finishes.                            *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Targets, Sources, DepsOf, Root, Sessions, MaxEdits, Recheck,
          SpawnFirst

ASSUME /\ Root \in Targets
       /\ MaxEdits \in Nat
       /\ Recheck \in {"none", "shallow", "settled", "await"}
       /\ SpawnFirst \in BOOLEAN
       /\ Targets \cap Sources = {}

\* --- graph instances, selected from the .cfg files via <- ----------------
\* T needs a source and a buildable dep; the dep needs a source (the
\* reproduced shape: T <- {s.txt, D}, D <- d.src).
PairTargets == {"T", "D"}
PairSources == {"s", "ds"}
PairDeps == ("T" :> <<"s", "D">>) @@ ("D" :> <<"ds">>)
\* The same one level deeper: staleness two hops below the re-checked target.
ChainTargets == {"T", "D", "E"}
ChainSources == {"s", "es"}
ChainDeps == ("T" :> <<"s", "D">>) @@ ("D" :> <<"E">>) @@ ("E" :> <<"es">>)
TwoSessions == {1, 2}
ThreeSessions == {1, 2, 3}

Deps(t) == {DepsOf[t][i] : i \in 1..Len(DepsOf[t])}
Buildable(t) == Deps(t) \cap Targets

\* Every target built cleanly from version 0 of every source.
RECURSIVE Built0(_)
Built0(t) == [d \in Deps(t) |-> IF d \in Sources THEN 0 ELSE Built0(d)]

Phases == {"idle", "check", "lock", "recheck", "build", "done"}

VARIABLES
    ver,        \* [Sources -> Nat]: current source content
    rec,        \* [Targets -> record]: content of the file = recorded edges
    inflight,   \* [Targets -> BOOLEAN]: Uncommitted marker (a build runs)
    lock,       \* [Targets -> Sessions \cup {0}]
    runid,      \* [Targets -> Sessions \cup {0}]: last session to settle it
    sess,       \* [Sessions -> {"idle","running","done"}]
    startEdits, \* [Sessions -> Nat]: the edit count when the session began
    edits,      \* Nat: source edits so far
    ph,         \* [Sessions -> [Targets -> Phases]]: the task registry
    ex,         \* [Sessions -> [Targets -> SUBSET deps]]: checked so far
    bi,         \* [Sessions -> [Targets -> Nat]]: do-file walk position
    part,       \* [Sessions -> [Targets -> record]]: contents read so far
    redundant,  \* history: a build started on a truly up-to-date target
    skipped     \* history: a re-check skipped on content (not on runid)

vars == <<ver, rec, inflight, lock, runid, sess, startEdits, edits, ph, ex,
          bi, part, redundant, skipped>>

Content(d) == IF d \in Sources THEN ver[d] ELSE rec[d]

(* Under SpawnFirst, no evidence is read before every recorded buildable
   dep's task has been started. *)
Spawned(S, t) == SpawnFirst => \A d \in Buildable(t) : ph[S][d] # "idle"

\* The ground truth: built from the current content of every dependency,
\* all the way down.
RECURSIVE TrueUTD(_)
TrueUTD(t) == \A d \in Deps(t) :
                  /\ rec[t][d] = Content(d)
                  /\ (d \in Targets => TrueUTD(d))

\* Every recorded edge of t matches what is on disk now (one level).
RecMatch(t) == \A d \in Deps(t) : rec[t][d] = Content(d)

Extend(f, d, v) == [x \in DOMAIN f \cup {d} |-> IF x = d THEN v ELSE f[x]]
Empty == [x \in {} |-> 0]

TypeOK ==
    /\ ver \in [Sources -> 0..MaxEdits]
    /\ DOMAIN rec = Targets /\ \A t \in Targets : DOMAIN rec[t] = Deps(t)
    /\ inflight \in [Targets -> BOOLEAN]
    /\ lock \in [Targets -> Sessions \cup {0}]
    /\ runid \in [Targets -> Sessions \cup {0}]
    /\ sess \in [Sessions -> {"idle", "running", "done"}]
    /\ ph \in [Sessions -> [Targets -> Phases]]
    /\ edits \in 0..MaxEdits
    /\ redundant \in BOOLEAN /\ skipped \in BOOLEAN

Init ==
    /\ ver = [s \in Sources |-> 0]
    /\ rec = [t \in Targets |-> Built0(t)]
    /\ inflight = [t \in Targets |-> FALSE]
    /\ lock = [t \in Targets |-> 0]
    /\ runid = [t \in Targets |-> 0]
    /\ sess = [s \in Sessions |-> "idle"]
    /\ startEdits = [s \in Sessions |-> 0]
    /\ edits = 0
    /\ ph = [s \in Sessions |-> [t \in Targets |-> "idle"]]
    /\ ex = [s \in Sessions |-> [t \in Targets |-> {}]]
    /\ bi = [s \in Sessions |-> [t \in Targets |-> 0]]
    /\ part = [s \in Sessions |-> [t \in Targets |-> Empty]]
    /\ redundant = FALSE
    /\ skipped = FALSE

SetPh(S, t, p) == ph' = [ph EXCEPT ![S][t] = p]

(* ---- sessions and edits ----------------------------------------------- *)

Start(S) ==
    /\ sess[S] = "idle"
    /\ sess' = [sess EXCEPT ![S] = "running"]
    /\ startEdits' = [startEdits EXCEPT ![S] = edits]
    /\ SetPh(S, Root, "check")
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, edits, ex, bi, part,
                   redundant, skipped>>

(* The run is over when its root has settled and no task of it is still
   running (redo_build drains speculative tasks before returning). *)
Finish(S) ==
    /\ sess[S] = "running"
    /\ ph[S][Root] = "done"
    /\ \A t \in Targets : ph[S][t] \in {"idle", "done"}
    /\ sess' = [sess EXCEPT ![S] = "done"]
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, startEdits, edits, ph, ex,
                   bi, part, redundant, skipped>>

Edit(s) ==
    /\ edits < MaxEdits
    /\ ver' = [ver EXCEPT ![s] = @ + 1]
    /\ edits' = edits + 1
    /\ UNCHANGED <<rec, inflight, lock, runid, sess, startEdits, ph, ex, bi,
                   part, redundant, skipped>>

(* ---- the unlocked check ------------------------------------------------ *)

CheckSeen(S, t) ==
    /\ ph[S][t] = "check"
    /\ runid[t] = S
    /\ SetPh(S, t, "done")
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   ex, bi, part, redundant, skipped>>

(* The Uncommitted marker of a build in flight is out-of-date evidence. *)
CheckInflight(S, t) ==
    /\ ph[S][t] = "check"
    /\ runid[t] # S
    /\ Spawned(S, t)
    /\ inflight[t]
    /\ SetPh(S, t, "lock")
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   ex, bi, part, redundant, skipped>>

(* Start a buildable dep's task (speculation, or reaching it in order). *)
CheckSpawn(S, t, d) ==
    /\ ph[S][t] = "check"
    /\ runid[t] # S
    /\ d \in Buildable(t)
    /\ ph[S][d] = "idle"
    /\ SetPh(S, d, "check")
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   ex, bi, part, redundant, skipped>>

(* Examine one dep: a source directly, a buildable dep once its task has
   settled. A mismatch sends the task to the build path; the rest of the
   evidence is abandoned, and dep tasks already started keep going. *)
CheckDep(S, t, d) ==
    /\ ph[S][t] = "check"
    /\ runid[t] # S
    /\ Spawned(S, t)
    /\ d \in Deps(t) \ ex[S][t]
    /\ d \in Targets => ph[S][d] = "done"
    /\ IF Content(d) # rec[t][d]
         THEN /\ SetPh(S, t, "lock")
              /\ UNCHANGED ex
         ELSE /\ ex' = [ex EXCEPT ![S][t] = @ \cup {d}]
              /\ UNCHANGED ph
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   bi, part, redundant, skipped>>

(* Everything checked out (the marker is an edge too, so no build may be in
   flight): mark verified for this session, without the lock. *)
CheckClean(S, t) ==
    /\ ph[S][t] = "check"
    /\ runid[t] # S
    /\ Spawned(S, t)
    /\ ex[S][t] = Deps(t)
    /\ ~inflight[t]
    /\ runid' = [runid EXCEPT ![t] = S]
    /\ SetPh(S, t, "done")
    /\ UNCHANGED <<ver, rec, inflight, lock, sess, startEdits, edits, ex, bi,
                   part, redundant, skipped>>

(* ---- the locked build path --------------------------------------------- *)

Acquire(S, t) ==
    /\ ph[S][t] = "lock"
    /\ lock[t] = 0
    /\ lock' = [lock EXCEPT ![t] = S]
    /\ SetPh(S, t, "recheck")
    /\ UNCHANGED <<ver, rec, inflight, runid, sess, startEdits, edits, ex, bi,
                   part, redundant, skipped>>

(* "settled" reads its evidence from the shared runid column, as the code
   does; a concurrent session's later mark_verified overwrites it, so the
   evidence can vanish (never appear falsely: only S writes S). "await" uses
   the session's own task results, which nobody else can overwrite. *)
OnContent(S, t) ==
    /\ Recheck \in {"shallow", "settled", "await"}
    /\ RecMatch(t)
    /\ Recheck = "settled" => \A d \in Buildable(t) : runid[d] = S
    /\ Recheck = "await" => \A d \in Buildable(t) : ph[S][d] = "done"

(* "await": the re-check first lets every buildable dep settle in this
   session, starting idle ones (the lock stays held, as in the build path's
   do-file walk). *)
Awaited(S, t) == Recheck = "await" => \A d \in Buildable(t) : ph[S][d] = "done"

RecheckSpawn(S, t, d) ==
    /\ Recheck = "await"
    /\ ph[S][t] = "recheck"
    /\ d \in Buildable(t)
    /\ ph[S][d] = "idle"
    /\ SetPh(S, d, "check")
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   ex, bi, part, redundant, skipped>>

RecheckSkip(S, t) ==
    /\ ph[S][t] = "recheck"
    /\ runid[t] = S \/ (Awaited(S, t) /\ OnContent(S, t))
    /\ skipped' = (skipped \/ runid[t] # S)
    /\ runid' = [runid EXCEPT ![t] = S]
    /\ lock' = [lock EXCEPT ![t] = 0]
    /\ SetPh(S, t, "done")
    /\ UNCHANGED <<ver, rec, inflight, sess, startEdits, edits, ex, bi, part,
                   redundant>>

RecheckBuild(S, t) ==
    /\ ph[S][t] = "recheck"
    /\ Awaited(S, t)
    /\ ~(runid[t] = S \/ OnContent(S, t))
    /\ redundant' = (redundant \/ TrueUTD(t))
    /\ inflight' = [inflight EXCEPT ![t] = TRUE]
    /\ bi' = [bi EXCEPT ![S][t] = 0]
    /\ part' = [part EXCEPT ![S][t] = Empty]
    /\ SetPh(S, t, "build")
    /\ UNCHANGED <<ver, rec, lock, runid, sess, startEdits, edits, ex,
                   skipped>>

(* The do-file's redo-ifchange of the next dep: a source is stamped and read
   (one step; LockSession covers the window between the two); a buildable
   dep is demanded -- its task started if idle -- and read once settled. *)
BuildDemand(S, t) ==
    /\ ph[S][t] = "build"
    /\ bi[S][t] < Len(DepsOf[t])
    /\ LET d == DepsOf[t][bi[S][t] + 1] IN
         /\ d \in Targets /\ ph[S][d] = "idle"
         /\ SetPh(S, d, "check")
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   ex, bi, part, redundant, skipped>>

BuildRead(S, t) ==
    /\ ph[S][t] = "build"
    /\ bi[S][t] < Len(DepsOf[t])
    /\ LET d == DepsOf[t][bi[S][t] + 1] IN
         /\ d \in Targets => ph[S][d] = "done"
         /\ part' = [part EXCEPT ![S][t] = Extend(@, d, Content(d))]
    /\ bi' = [bi EXCEPT ![S][t] = @ + 1]
    /\ UNCHANGED <<ver, rec, inflight, lock, runid, sess, startEdits, edits,
                   ph, ex, redundant, skipped>>

Commit(S, t) ==
    /\ ph[S][t] = "build"
    /\ bi[S][t] = Len(DepsOf[t])
    /\ rec' = [rec EXCEPT ![t] = part[S][t]]
    /\ inflight' = [inflight EXCEPT ![t] = FALSE]
    /\ runid' = [runid EXCEPT ![t] = S]
    /\ lock' = [lock EXCEPT ![t] = 0]
    /\ SetPh(S, t, "done")
    /\ UNCHANGED <<ver, sess, startEdits, edits, ex, bi, part, redundant,
                   skipped>>

TaskStep(S, t) ==
    \/ CheckSeen(S, t) \/ CheckInflight(S, t) \/ CheckClean(S, t)
    \/ \E d \in Deps(t) : CheckSpawn(S, t, d) \/ CheckDep(S, t, d)
    \/ Acquire(S, t) \/ RecheckSkip(S, t) \/ RecheckBuild(S, t)
    \/ \E d \in Deps(t) : RecheckSpawn(S, t, d)
    \/ BuildDemand(S, t) \/ BuildRead(S, t) \/ Commit(S, t)

SStep(S) == Start(S) \/ Finish(S) \/ \E t \in Targets : TaskStep(S, t)

AllDone == \A S \in Sessions : sess[S] = "done"

Terminating == AllDone /\ UNCHANGED vars

Next ==
    \/ \E S \in Sessions : SStep(S)
    \/ \E s \in Sources : Edit(s)
    \/ Terminating

(* Every session's own steps are weakly fair; edits are never forced. *)
Spec == Init /\ [][Next]_vars /\ (\A S \in Sessions : WF_vars(SStep(S)))

----------------------------------------------------------------------------
(* A run during which nothing was edited ends consistent: every target it
   settled is built from the current content of everything below it. *)
NoStaleRun ==
    \A S \in Sessions :
        (sess[S] = "done" /\ edits = startEdits[S])
            => \A t \in Targets : runid[t] = S => TrueUTD(t)

NoRedundantBuild == ~redundant

(* Coverage: violated exactly when a re-check skips a build on content. *)
NeverSkipAtRecheck == ~skipped

Termination == <>AllDone

============================================================================

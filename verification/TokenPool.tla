---------------------------- MODULE TokenPool ----------------------------
(***************************************************************************)
(* The jobserver token protocol of redo-msh (src/jobserver.rs,             *)
(* run_parallel in src/build.rs), abstracted:                              *)
(*                                                                         *)
(*   - A build has J total tokens: every process holds one implicit "own"  *)
(*     token, and a shared pool starts with J-1 extra tokens.              *)
(*   - The top-level process runs NTop top jobs in parallel (own token     *)
(*     first, then pool tokens, never blocking on the pool).               *)
(*   - Each top job's do-file executes ("pre"), then calls redo-ifchange,  *)
(*     which blocks it ("waiting") while a child process runs NSub sub     *)
(*     jobs under the same discipline (its own fresh own token + pool),    *)
(*     then resumes ("post") and finishes, returning its token.            *)
(*                                                                         *)
(* Sub jobs are leaves; the model is two levels deep.                      *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   TokenConservation - pool tokens are never lost or duplicated, on any  *)
(*                       interleaving (including the error-free paths      *)
(*                       modeled here).                                    *)
(*   Bound             - at most J do-file bodies EXECUTE simultaneously   *)
(*                       ("pre"/"post" tops + running subs). Blocked       *)
(*                       ancestors hold tokens but are not executing; each *)
(*                       child process's fresh own token compensates.      *)
(*                       This is the precise statement of the concurrency  *)
(*                       limit -- the naive "at most J tokens exist" is    *)
(*                       false by design.                                  *)
(*   Termination       - every job eventually completes (deadlock-freedom  *)
(*                       of the try-acquire discipline).                   *)
(*                                                                         *)
(* Assumption made explicit by the "waiting" state: a do-file blocks for   *)
(* the whole redo-ifchange call.  A do-file that backgrounds redo-ifchange *)
(* and keeps computing violates Bound (but not TokenConservation).         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS J, NTop, NSub

ASSUME /\ J \in Nat \ {0}
       /\ NTop \in Nat \ {0}
       /\ NSub \in Nat \ {0}

TopJobs == 1..NTop
SubJobs == TopJobs \X (1..NSub)

VARIABLES
    pool,       \* extra tokens currently free in the shared pool
    topState,   \* [TopJobs -> {"queued","pre","waiting","post","done"}]
    topSrc,     \* token source of each top job: "own" | "pool" | "none"
    topOwnFree, \* top-level process's own token is unused
    subState,   \* [SubJobs -> {"queued","running","done"}]
    subSrc,     \* token source of each sub job
    subOwnFree  \* [TopJobs -> BOOLEAN]: child process t's own token unused

vars == <<pool, topState, topSrc, topOwnFree, subState, subSrc, subOwnFree>>

TopStates == {"queued", "pre", "waiting", "post", "done"}
SubStates == {"queued", "running", "done"}
Srcs == {"own", "pool", "none"}

TypeOK ==
    /\ pool \in 0..(J - 1)
    /\ topState \in [TopJobs -> TopStates]
    /\ topSrc \in [TopJobs -> Srcs]
    /\ topOwnFree \in BOOLEAN
    /\ subState \in [SubJobs -> SubStates]
    /\ subSrc \in [SubJobs -> Srcs]
    /\ subOwnFree \in [TopJobs -> BOOLEAN]

Init ==
    /\ pool = J - 1
    /\ topState = [t \in TopJobs |-> "queued"]
    /\ topSrc = [t \in TopJobs |-> "none"]
    /\ topOwnFree = TRUE
    /\ subState = [u \in SubJobs |-> "queued"]
    /\ subSrc = [u \in SubJobs |-> "none"]
    /\ subOwnFree = [t \in TopJobs |-> TRUE]

(* Launch a top job on the process's own token (own-first, as in the code). *)
LaunchTopOwn(t) ==
    /\ topState[t] = "queued"
    /\ topOwnFree
    /\ topOwnFree' = FALSE
    /\ topState' = [topState EXCEPT ![t] = "pre"]
    /\ topSrc' = [topSrc EXCEPT ![t] = "own"]
    /\ UNCHANGED <<pool, subState, subSrc, subOwnFree>>

(* Launch a top job on a try-acquired pool token. *)
LaunchTopPool(t) ==
    /\ topState[t] = "queued"
    /\ ~topOwnFree
    /\ pool > 0
    /\ pool' = pool - 1
    /\ topState' = [topState EXCEPT ![t] = "pre"]
    /\ topSrc' = [topSrc EXCEPT ![t] = "pool"]
    /\ UNCHANGED <<topOwnFree, subState, subSrc, subOwnFree>>

(* The do-file calls redo-ifchange: it blocks, its child process starts. *)
SpawnChild(t) ==
    /\ topState[t] = "pre"
    /\ topState' = [topState EXCEPT ![t] = "waiting"]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree>>

LaunchSubOwn(t, s) ==
    /\ topState[t] = "waiting"
    /\ subState[<<t, s>>] = "queued"
    /\ subOwnFree[t]
    /\ subOwnFree' = [subOwnFree EXCEPT ![t] = FALSE]
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "running"]
    /\ subSrc' = [subSrc EXCEPT ![<<t, s>>] = "own"]
    /\ UNCHANGED <<pool, topState, topSrc, topOwnFree>>

LaunchSubPool(t, s) ==
    /\ topState[t] = "waiting"
    /\ subState[<<t, s>>] = "queued"
    /\ ~subOwnFree[t]
    /\ pool > 0
    /\ pool' = pool - 1
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "running"]
    /\ subSrc' = [subSrc EXCEPT ![<<t, s>>] = "pool"]
    /\ UNCHANGED <<topState, topSrc, topOwnFree, subOwnFree>>

FinishSub(t, s) ==
    /\ subState[<<t, s>>] = "running"
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "done"]
    /\ IF subSrc[<<t, s>>] = "own"
         THEN /\ subOwnFree' = [subOwnFree EXCEPT ![t] = TRUE]
              /\ UNCHANGED pool
         ELSE /\ pool' = pool + 1
              /\ UNCHANGED subOwnFree
    /\ UNCHANGED <<topState, topSrc, topOwnFree, subSrc>>

(* redo-ifchange returns: the do-file resumes executing after its deps. *)
ResumeTop(t) ==
    /\ topState[t] = "waiting"
    /\ \A s \in 1..NSub : subState[<<t, s>>] = "done"
    /\ topState' = [topState EXCEPT ![t] = "post"]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree>>

FinishTop(t) ==
    /\ topState[t] = "post"
    /\ topState' = [topState EXCEPT ![t] = "done"]
    /\ IF topSrc[t] = "own"
         THEN /\ topOwnFree' = TRUE
              /\ UNCHANGED pool
         ELSE /\ pool' = pool + 1
              /\ UNCHANGED topOwnFree
    /\ UNCHANGED <<topSrc, subState, subSrc, subOwnFree>>

AllDone == \A t \in TopJobs : topState[t] = "done"

Terminating == AllDone /\ UNCHANGED vars

Next ==
    \/ \E t \in TopJobs :
         \/ LaunchTopOwn(t)
         \/ LaunchTopPool(t)
         \/ SpawnChild(t)
         \/ ResumeTop(t)
         \/ FinishTop(t)
         \/ \E s \in 1..NSub :
              \/ LaunchSubOwn(t, s)
              \/ LaunchSubPool(t, s)
              \/ FinishSub(t, s)
    \/ Terminating

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

----------------------------------------------------------------------------
(* Pool tokens held by launched-but-unfinished jobs. *)
PoolHeldTop ==
    Cardinality({t \in TopJobs :
        topState[t] \in {"pre", "waiting", "post"} /\ topSrc[t] = "pool"})
PoolHeldSub ==
    Cardinality({u \in SubJobs :
        subState[u] = "running" /\ subSrc[u] = "pool"})

TokenConservation == pool + PoolHeldTop + PoolHeldSub = J - 1

(* Do-file bodies actually executing (not blocked in redo-ifchange). *)
Active ==
    Cardinality({t \in TopJobs : topState[t] \in {"pre", "post"}})
    + Cardinality({u \in SubJobs : subState[u] = "running"})

Bound == Active <= J

Termination == <>AllDone

============================================================================

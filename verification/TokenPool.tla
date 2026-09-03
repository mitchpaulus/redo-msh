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
(*                                                                         *)
(* THE OVERWRITE PROMPT (prompt_overwrite in src/build.rs). A top job may  *)
(* stop before running its do-file to ask the human whether a hand-edited  *)
(* target may be overwritten. Facts the model keeps from the code:         *)
(*   - the prompt happens AFTER the per-target kernel lock is taken, so    *)
(*     any other job that needs that target blocks on the lock until the   *)
(*     prompter's whole build is over ("lockw"); lock waiters keep their   *)
(*     token (build() blocks outright; no release);                        *)
(*   - PromptMode = "release": the prompter writes one token to the POOL   *)
(*     whatever its own token's source was (Jobserver::release does not    *)
(*     know), and afterwards spins on try_acquire against the POOL only    *)
(*     ("reacq"). This was the shipping code until 2026-09-03; the model   *)
(*     deadlocks it in ten steps at J=2: every other token ends up held by *)
(*     a job blocked on the prompter's lock, and the prompter spins        *)
(*     forever. No .cfg runs it any more; the mode stays as the record.   *)
(*   - PromptMode = "hold": the prompter keeps its token while blocked on  *)
(*     the human. One slot idles while the user thinks; nothing can        *)
(*     deadlock. TokenPool_PromptHold.cfg verifies it.                     *)
(*   - PromptMode = "none": the original model (regression).               *)
(* Prompts are modeled at the top level only; a nested process's prompt   *)
(* is the same shape one level down.                                       *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets

CONSTANTS J, NTop, NSub, PromptMode

ASSUME /\ J \in Nat \ {0}
       /\ NTop \in Nat \ {0}
       /\ NSub \in Nat \ {0}
       /\ PromptMode \in {"none", "release", "hold"}

TopJobs == 1..NTop
SubJobs == TopJobs \X (1..NSub)

VARIABLES
    pool,       \* extra tokens currently free in the shared pool
    topState,   \* [TopJobs -> {"queued","pre","waiting","post","done"}]
    topSrc,     \* token source of each top job: "own" | "pool" | "none"
    topOwnFree, \* top-level process's own token is unused
    subState,   \* [SubJobs -> {"queued","running","done"}]
    subSrc,     \* token source of each sub job
    subOwnFree, \* [TopJobs -> BOOLEAN]: child process t's own token unused
    prompted,   \* [TopJobs -> BOOLEAN]: this job already asked its question
    topWait,    \* [TopJobs -> TopJobs \cup {0}]: lock held by which prompter
    subWait     \* [SubJobs -> TopJobs \cup {0}]

vars == <<pool, topState, topSrc, topOwnFree, subState, subSrc, subOwnFree,
          prompted, topWait, subWait>>

(* "prompt": blocked on the human. "reacq": answered, spinning for a pool  *)
(* token (release mode only). "lockw": blocked on a prompter's kernel lock. *)
TopStates == {"queued", "pre", "prompt", "reacq", "lockw", "waiting", "post",
              "done"}
SubStates == {"queued", "running", "lockw", "done"}
Prompting == {t \in TopJobs : topState[t] \in {"prompt", "reacq"}}
Srcs == {"own", "pool", "none"}

TypeOK ==
    \* release mode lends own tokens into the pool: it can hold J for a while
    /\ pool \in 0..(IF PromptMode = "release" THEN J ELSE J - 1)
    /\ topState \in [TopJobs -> TopStates]
    /\ topSrc \in [TopJobs -> Srcs]
    /\ topOwnFree \in BOOLEAN
    /\ subState \in [SubJobs -> SubStates]
    /\ subSrc \in [SubJobs -> Srcs]
    /\ subOwnFree \in [TopJobs -> BOOLEAN]
    /\ prompted \in [TopJobs -> BOOLEAN]
    /\ topWait \in [TopJobs -> TopJobs \cup {0}]
    /\ subWait \in [SubJobs -> TopJobs \cup {0}]

Init ==
    /\ pool = J - 1
    /\ topState = [t \in TopJobs |-> "queued"]
    /\ topSrc = [t \in TopJobs |-> "none"]
    /\ topOwnFree = TRUE
    /\ subState = [u \in SubJobs |-> "queued"]
    /\ subSrc = [u \in SubJobs |-> "none"]
    /\ subOwnFree = [t \in TopJobs |-> TRUE]
    /\ prompted = [t \in TopJobs |-> FALSE]
    /\ topWait = [t \in TopJobs |-> 0]
    /\ subWait = [u \in SubJobs |-> 0]

(* Launch a top job on the process's own token (own-first, as in the code). *)
LaunchTopOwn(t) ==
    /\ topState[t] = "queued"
    /\ topOwnFree
    /\ topOwnFree' = FALSE
    /\ topState' = [topState EXCEPT ![t] = "pre"]
    /\ topSrc' = [topSrc EXCEPT ![t] = "own"]
    /\ UNCHANGED <<pool, subState, subSrc, subOwnFree, prompted, topWait, subWait>>

(* Launch a top job on a try-acquired pool token. *)
LaunchTopPool(t) ==
    /\ topState[t] = "queued"
    /\ ~topOwnFree
    /\ pool > 0
    /\ pool' = pool - 1
    /\ topState' = [topState EXCEPT ![t] = "pre"]
    /\ topSrc' = [topSrc EXCEPT ![t] = "pool"]
    /\ UNCHANGED <<topOwnFree, subState, subSrc, subOwnFree, prompted, topWait, subWait>>

(* The do-file calls redo-ifchange: it blocks, its child process starts. *)
SpawnChild(t) ==
    /\ topState[t] = "pre"
    /\ topState' = [topState EXCEPT ![t] = "waiting"]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree, prompted, topWait, subWait>>

LaunchSubOwn(t, s) ==
    /\ topState[t] = "waiting"
    /\ subState[<<t, s>>] = "queued"
    /\ subOwnFree[t]
    /\ subOwnFree' = [subOwnFree EXCEPT ![t] = FALSE]
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "running"]
    /\ subSrc' = [subSrc EXCEPT ![<<t, s>>] = "own"]
    /\ UNCHANGED <<pool, topState, topSrc, topOwnFree, prompted, topWait, subWait>>

LaunchSubPool(t, s) ==
    /\ topState[t] = "waiting"
    /\ subState[<<t, s>>] = "queued"
    /\ ~subOwnFree[t]
    /\ pool > 0
    /\ pool' = pool - 1
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "running"]
    /\ subSrc' = [subSrc EXCEPT ![<<t, s>>] = "pool"]
    /\ UNCHANGED <<topState, topSrc, topOwnFree, subOwnFree, prompted, topWait, subWait>>

FinishSub(t, s) ==
    /\ subState[<<t, s>>] = "running"
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "done"]
    /\ IF subSrc[<<t, s>>] = "own"
         THEN /\ subOwnFree' = [subOwnFree EXCEPT ![t] = TRUE]
              /\ UNCHANGED pool
         ELSE /\ pool' = pool + 1
              /\ UNCHANGED subOwnFree
    /\ UNCHANGED <<topState, topSrc, topOwnFree, subSrc, prompted, topWait, subWait>>

(* redo-ifchange returns: the do-file resumes executing after its deps. *)
ResumeTop(t) ==
    /\ topState[t] = "waiting"
    /\ \A s \in 1..NSub : subState[<<t, s>>] = "done"
    /\ topState' = [topState EXCEPT ![t] = "post"]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree, prompted, topWait, subWait>>

FinishTop(t) ==
    /\ topState[t] = "post"
    /\ topState' = [topState EXCEPT ![t] = "done"]
    /\ IF topSrc[t] = "own"
         THEN /\ topOwnFree' = TRUE
              /\ UNCHANGED pool
         ELSE /\ pool' = pool + 1
              /\ UNCHANGED topOwnFree
    /\ UNCHANGED <<topSrc, subState, subSrc, subOwnFree, prompted, topWait, subWait>>

(* ---- the overwrite prompt ------------------------------------------- *)

(* The job stops to ask (the hand-edit check runs before the do-file, with *)
(* the target lock held). In release mode one token goes back to the pool  *)
(* -- ALWAYS the pool, even if the job runs on the own token: that is what *)
(* the code does, and it is why the pool can transiently hold J tokens.    *)
Prompt(t) ==
    /\ PromptMode # "none"
    /\ topState[t] = "pre" /\ ~prompted[t]
    /\ prompted' = [prompted EXCEPT ![t] = TRUE]
    /\ topState' = [topState EXCEPT ![t] = "prompt"]
    /\ pool' = IF PromptMode = "release" THEN pool + 1 ELSE pool
    /\ UNCHANGED <<topSrc, topOwnFree, subState, subSrc, subOwnFree, topWait,
                   subWait>>

(* The human answers. Hold mode resumes at once; release mode must first   *)
(* win a token back.                                                       *)
Answer(t) ==
    /\ topState[t] = "prompt"
    /\ topState' = [topState EXCEPT ![t] =
                      IF PromptMode = "release" THEN "reacq" ELSE "pre"]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree,
                   prompted, topWait, subWait>>

(* try_acquire in a loop: only the pool is consulted, never the own token, *)
(* and there is no action at all while pool = 0 -- the spin.               *)
Reacquire(t) ==
    /\ topState[t] = "reacq"
    /\ pool > 0
    /\ pool' = pool - 1
    /\ topState' = [topState EXCEPT ![t] = "pre"]
    /\ UNCHANGED <<topSrc, topOwnFree, subState, subSrc, subOwnFree,
                   prompted, topWait, subWait>>

(* ---- contention on the prompter's kernel lock ------------------------ *)

(* Another top job (or a sub job in a child process) turns out to need the *)
(* target the prompter is sitting on. It blocks on the kernel lock,        *)
(* holding its token, until the prompter's build has committed; then the   *)
(* double-checked runid skip lets it finish without building.              *)
LockWaitTop(u, t) ==
    /\ u # t /\ t \in Prompting
    /\ topState[u] = "pre"
    /\ topState' = [topState EXCEPT ![u] = "lockw"]
    /\ topWait' = [topWait EXCEPT ![u] = t]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree,
                   prompted, subWait>>

LockResumeTop(u) ==
    /\ topState[u] = "lockw"
    /\ topState[topWait[u]] = "done"
    /\ topState' = [topState EXCEPT ![u] = "post"]
    /\ topWait' = [topWait EXCEPT ![u] = 0]
    /\ UNCHANGED <<pool, topSrc, topOwnFree, subState, subSrc, subOwnFree,
                   prompted, subWait>>

LockWaitSub(t, s, w) ==
    /\ w # t /\ w \in Prompting
    /\ subState[<<t, s>>] = "running"
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "lockw"]
    /\ subWait' = [subWait EXCEPT ![<<t, s>>] = w]
    /\ UNCHANGED <<pool, topState, topSrc, topOwnFree, subSrc, subOwnFree,
                   prompted, topWait>>

LockResumeSub(t, s) ==
    /\ subState[<<t, s>>] = "lockw"
    /\ topState[subWait[<<t, s>>]] = "done"
    /\ subState' = [subState EXCEPT ![<<t, s>>] = "running"]
    /\ subWait' = [subWait EXCEPT ![<<t, s>>] = 0]
    /\ UNCHANGED <<pool, topState, topSrc, topOwnFree, subSrc, subOwnFree,
                   prompted, topWait>>

AllDone == \A t \in TopJobs : topState[t] = "done"

Terminating == AllDone /\ UNCHANGED vars

Next ==
    \/ \E t \in TopJobs :
         \/ LaunchTopOwn(t)
         \/ LaunchTopPool(t)
         \/ SpawnChild(t)
         \/ ResumeTop(t)
         \/ FinishTop(t)
         \/ Prompt(t) \/ Answer(t) \/ Reacquire(t) \/ LockResumeTop(t)
         \/ \E w \in TopJobs : LockWaitTop(t, w)
         \/ \E s \in 1..NSub :
              \/ LaunchSubOwn(t, s)
              \/ LaunchSubPool(t, s)
              \/ FinishSub(t, s)
              \/ LockResumeSub(t, s)
              \/ \E w \in TopJobs : LockWaitSub(t, s, w)
    \/ Terminating

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

----------------------------------------------------------------------------
(* Pool tokens held by launched-but-unfinished jobs. A job blocked on a    *)
(* lock or (hold mode) on the human still holds its token; in release mode *)
(* the prompter has given its token up.                                    *)
HoldingStates ==
    IF PromptMode = "release"
      THEN {"pre", "waiting", "post", "lockw"}
      ELSE {"pre", "waiting", "post", "lockw", "prompt"}
PoolHeldTop ==
    Cardinality({t \in TopJobs :
        topState[t] \in HoldingStates /\ topSrc[t] = "pool"})
PoolHeldSub ==
    Cardinality({u \in SubJobs :
        subState[u] \in {"running", "lockw"} /\ subSrc[u] = "pool"})

(* Release mode lends an OWN token into the pool while its holder is       *)
(* prompting: the pool then over-counts by exactly that many. The precise  *)
(* conservation statement accounts for the loan; the reacquired token is  *)
(* the loan coming back (the count is right again, the labels are not:    *)
(* the job then runs a pool token under topSrc = "own", and FinishTop     *)
(* frees the own flag instead of returning it -- which is the same thing  *)
(* numerically, so the invariant holds).                                   *)
LentOwn ==
    IF PromptMode = "release"
      THEN Cardinality({t \in Prompting : topSrc[t] = "own"})
      ELSE 0

TokenConservation == pool + PoolHeldTop + PoolHeldSub = J - 1 + LentOwn

(* Do-file bodies actually executing (not blocked in redo-ifchange). *)
Active ==
    Cardinality({t \in TopJobs : topState[t] \in {"pre", "post"}})
    + Cardinality({u \in SubJobs : subState[u] = "running"})

Bound == Active <= J

Termination == <>AllDone

============================================================================

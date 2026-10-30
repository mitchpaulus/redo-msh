---------------------------- MODULE TokenPool ----------------------------
(***************************************************************************)
(* The jobserver token protocol of redo-msh (src/jobserver.rs,             *)
(* run_parallel and prompt_overwrite in src/build.rs) over a job TREE of   *)
(* arbitrary depth:                                                        *)
(*                                                                         *)
(*   - A build has J total tokens: every process holds one implicit "own"  *)
(*     token, and a shared pool starts with J-1 extra tokens.              *)
(*   - Jobs form a tree: FanOut[k] is the number of children of each job   *)
(*     at depth k-1 (the top-level process runs FanOut[1] jobs). A job's   *)
(*     do-file runs ("pre"), and a non-leaf job then calls redo-ifchange   *)
(*     and blocks ("waiting") while a CHILD PROCESS runs its children,     *)
(*     resumes ("post") when they are all done, and finishes.              *)
(*   - Each process launches its jobs in order, own token first, then pool *)
(*     tokens, never blocking on the pool. With Lazy (today's              *)
(*     run_parallel), a process that finds the pool empty sleeps until one *)
(*     of ITS OWN jobs completes (it blocks in rx.recv()); without Lazy it *)
(*     launches whenever a token is available (the eager scheduler the     *)
(*     implementation contract requires).                                  *)
(*   - Overwrite prompt (Prompts): a job about to run its do-file may stop *)
(*     to ask on the terminal. PromptMode:                                 *)
(*       "pool"   - today: release one token to the POOL, then spin on     *)
(*                  pool try-acquire to resume;                            *)
(*       "hold"   - keep the token while waiting;                          *)
(*       "source" - give the token back to where it came from (own token   *)
(*                  -> the process's own slot, waking its scheduler; pool  *)
(*                  token -> the pool) and resume on the process's own     *)
(*                  token if free, else a pool token.                      *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   TokenConservation - pool tokens are never lost or duplicated (a       *)
(*                       prompting job in "pool" mode accounts for the one *)
(*                       extra token it released).                         *)
(*   OwnConsistent     - a process's own token is free iff none of its     *)
(*                       jobs holds it (so at most one does).              *)
(*   ChildrenOnlyWhileWaiting - a child process's jobs are live only while *)
(*                       their parent job is blocked in redo-ifchange.     *)
(*   Bound             - at most J jobs execute at once.                   *)
(*   ExactBound        - executing jobs + free pool tokens <= J.           *)
(*   SemaphoreMax      - the pool never exceeds J (on Windows the pool is  *)
(*                       a semaphore with maximum count J, and releasing   *)
(*                       past it fails silently).                          *)
(*   Termination       - every job completes.                              *)
(*   WorkConserving    - no process sleeps with a launchable job while a   *)
(*                       pool token is free (EXPECTED VIOLATED with Lazy:  *)
(*                       today's under-utilization, made precise).         *)
(*                                                                         *)
(* OwnUnique, TokenHeld, ChildrenOnlyWhileWaiting and TokenConservation   *)
(* are the lemmas of the depth-independent argument for ExactBound in      *)
(* README.md: each relates one process to its parent job and the pool, so  *)
(* checking them at depths 2 and 3 exercises every step shape the          *)
(* argument uses.                                                          *)
(*                                                                         *)
(* Assumption made explicit by the "waiting" state: a do-file blocks for   *)
(* the whole redo-ifchange call. A do-file that backgrounds redo-ifchange  *)
(* and keeps computing violates Bound (but not TokenConservation).         *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS J, FanOut, Lazy, Prompts, PromptMode

ASSUME /\ J \in Nat \ {0}
       /\ FanOut \in Seq(Nat \ {0}) /\ Len(FanOut) >= 1
       /\ Lazy \in BOOLEAN /\ Prompts \in BOOLEAN
       /\ PromptMode \in {"pool", "hold", "source"}

\* --- tree shapes, selected from the .cfg files via <- ---------------------
Tree22 == <<2, 2>>       \* the original NTop=2, NSub=2
Tree32 == <<3, 2>>       \* the original NTop=3, NSub=2
Tree122 == <<1, 2, 2>>   \* depth 3, branching at every level below the top
Tree222 == <<2, 2, 2>>

D == Len(FanOut)

RECURSIVE JobsAt(_)
JobsAt(k) ==
    IF k = 1 THEN {<<i>> : i \in 1..FanOut[1]}
    ELSE {Append(j, i) : j \in JobsAt(k - 1), i \in 1..FanOut[k]}

Jobs == UNION {JobsAt(k) : k \in 1..D}
Parent(j) == SubSeq(j, 1, Len(j) - 1)
Leaf(j) == Len(j) = D
Children(j) == {c \in Jobs : Len(c) = Len(j) + 1 /\ Parent(c) = j}

\* A process: the top level (<<>>) or the child process of a non-leaf job.
ProcSet == {<<>>} \cup {j \in Jobs : ~Leaf(j)}
InProc(p) == {j \in Jobs : Parent(j) = p}
Earlier(j) == {c \in InProc(Parent(j)) : c[Len(c)] < j[Len(j)]}

States == {"queued", "start", "prompt", "pre", "waiting", "post", "done"}
Holding == {"start", "prompt", "pre", "waiting", "post"}   \* launched, unfinished
Executing == {"start", "pre", "post"}                      \* not blocked

VARIABLES
    pool,      \* free tokens in the shared pool
    st,        \* [Jobs -> States]
    src,       \* [Jobs -> {"own", "pool", "none"}]: the token a job holds
    ownFree,   \* [ProcSet -> BOOLEAN]: the process's own token is unused
    asleep     \* [ProcSet -> BOOLEAN]: blocked in rx.recv() (Lazy only)

vars == <<pool, st, src, ownFree, asleep>>

TypeOK ==
    /\ pool \in Nat
    /\ st \in [Jobs -> States]
    /\ src \in [Jobs -> {"own", "pool", "none"}]
    /\ ownFree \in [ProcSet -> BOOLEAN]
    /\ asleep \in [ProcSet -> BOOLEAN]

Init ==
    /\ pool = J - 1
    /\ st = [j \in Jobs |-> "queued"]
    /\ src = [j \in Jobs |-> "none"]
    /\ ownFree = [p \in ProcSet |-> TRUE]
    /\ asleep = [p \in ProcSet |-> FALSE]

(* A process runs while it is the top level or its parent job is blocked.
   (An IF, not a disjunction: TLC evaluates both disjuncts inside actions,
   and st has no entry for the top level.) *)
ProcActive(p) == IF p = <<>> THEN TRUE ELSE st[p] = "waiting"

(* j is the next job its process would launch. *)
Next1(j) ==
    /\ st[j] = "queued"
    /\ \A c \in Earlier(j) : st[c] # "queued"
    /\ ProcActive(Parent(j))
    /\ ~asleep[Parent(j)]

HasNext(p) == \E j \in InProc(p) : Next1(j)

----------------------------------------------------------------------------
LaunchOwn(j) ==
    /\ Next1(j)
    /\ ownFree[Parent(j)]
    /\ ownFree' = [ownFree EXCEPT ![Parent(j)] = FALSE]
    /\ st' = [st EXCEPT ![j] = "start"]
    /\ src' = [src EXCEPT ![j] = "own"]
    /\ UNCHANGED <<pool, asleep>>

LaunchPool(j) ==
    /\ Next1(j)
    /\ ~ownFree[Parent(j)]
    /\ pool > 0
    /\ pool' = pool - 1
    /\ st' = [st EXCEPT ![j] = "start"]
    /\ src' = [src EXCEPT ![j] = "pool"]
    /\ UNCHANGED <<ownFree, asleep>>

(* run_parallel found no token: block until one of its own jobs finishes. *)
Sleep(p) ==
    /\ Lazy
    /\ HasNext(p)
    /\ ~ownFree[p]
    /\ pool = 0
    /\ asleep' = [asleep EXCEPT ![p] = TRUE]
    /\ UNCHANGED <<pool, st, src, ownFree>>

(* No prompt needed: the do-file starts. *)
Begin(j) ==
    /\ st[j] = "start"
    /\ st' = [st EXCEPT ![j] = "pre"]
    /\ UNCHANGED <<pool, src, ownFree, asleep>>

(* prompt_overwrite: block on the human. *)
Prompt(j) ==
    /\ Prompts
    /\ st[j] = "start"
    /\ st' = [st EXCEPT ![j] = "prompt"]
    /\ CASE PromptMode = "pool" ->
              /\ pool' = pool + 1
              /\ UNCHANGED <<src, ownFree, asleep>>
         [] PromptMode = "hold" ->
              UNCHANGED <<pool, src, ownFree, asleep>>
         [] PromptMode = "source" ->
              /\ src' = [src EXCEPT ![j] = "none"]
              /\ IF src[j] = "own"
                   THEN /\ ownFree' = [ownFree EXCEPT ![Parent(j)] = TRUE]
                        /\ asleep' = [asleep EXCEPT ![Parent(j)] = FALSE]
                        /\ UNCHANGED pool
                   ELSE /\ pool' = pool + 1
                        /\ UNCHANGED <<ownFree, asleep>>

(* Answered: take a token back, then run. *)
Reacquire(j) ==
    /\ st[j] = "prompt"
    /\ st' = [st EXCEPT ![j] = "pre"]
    /\ CASE PromptMode = "pool" ->
              /\ pool > 0
              /\ pool' = pool - 1
              /\ UNCHANGED <<src, ownFree, asleep>>
         [] PromptMode = "hold" ->
              UNCHANGED <<pool, src, ownFree, asleep>>
         [] PromptMode = "source" ->
              \/ /\ ownFree[Parent(j)]
                 /\ ownFree' = [ownFree EXCEPT ![Parent(j)] = FALSE]
                 /\ src' = [src EXCEPT ![j] = "own"]
                 /\ UNCHANGED <<pool, asleep>>
              \/ /\ ~ownFree[Parent(j)]
                 /\ pool > 0
                 /\ pool' = pool - 1
                 /\ src' = [src EXCEPT ![j] = "pool"]
                 /\ UNCHANGED <<ownFree, asleep>>

(* The do-file calls redo-ifchange: it blocks, its child process starts. *)
Spawn(j) ==
    /\ st[j] = "pre"
    /\ ~Leaf(j)
    /\ st' = [st EXCEPT ![j] = "waiting"]
    /\ UNCHANGED <<pool, src, ownFree, asleep>>

(* redo-ifchange returned: the do-file resumes. *)
Resume(j) ==
    /\ st[j] = "waiting"
    /\ \A c \in Children(j) : st[c] = "done"
    /\ st' = [st EXCEPT ![j] = "post"]
    /\ UNCHANGED <<pool, src, ownFree, asleep>>

(* A job finishes: its token goes back where it came from, and its
   process's scheduler wakes (the completion arrives on its channel). *)
Finish(j) ==
    /\ \/ st[j] = "post"
       \/ st[j] = "pre" /\ Leaf(j)
    /\ st' = [st EXCEPT ![j] = "done"]
    /\ src' = [src EXCEPT ![j] = "none"]
    /\ asleep' = [asleep EXCEPT ![Parent(j)] = FALSE]
    /\ IF src[j] = "own"
         THEN /\ ownFree' = [ownFree EXCEPT ![Parent(j)] = TRUE]
              /\ UNCHANGED pool
         ELSE /\ pool' = pool + 1
              /\ UNCHANGED ownFree

AllDone == \A j \in Jobs : st[j] = "done"

Terminating == AllDone /\ UNCHANGED vars

Next ==
    \/ \E j \in Jobs :
         LaunchOwn(j) \/ LaunchPool(j) \/ Begin(j) \/ Prompt(j)
         \/ Reacquire(j) \/ Spawn(j) \/ Resume(j) \/ Finish(j)
    \/ \E p \in ProcSet : Sleep(p)
    \/ Terminating

Spec == Init /\ [][Next]_vars /\ WF_vars(Next)

----------------------------------------------------------------------------
PoolHeld == Cardinality({j \in Jobs : st[j] \in Holding /\ src[j] = "pool"})
Prompting == Cardinality({j \in Jobs : st[j] = "prompt"})

TokenConservation ==
    pool + PoolHeld = J - 1 + (IF PromptMode = "pool" THEN Prompting ELSE 0)

OwnConsistent ==
    \A p \in ProcSet :
        ownFree[p] <=> ~\E j \in InProc(p) : st[j] \in Holding /\ src[j] = "own"

OwnUnique ==
    \A p \in ProcSet :
        Cardinality({j \in InProc(p) : st[j] \in Holding /\ src[j] = "own"}) <= 1

(* Every launched, unfinished job holds a token -- except a job waiting at
   a prompt in "source" mode, which gave its token back. *)
TokenHeld ==
    \A j \in Jobs :
        (st[j] \in Holding /\ ~(st[j] = "prompt" /\ PromptMode = "source"))
            => src[j] # "none"

ChildrenOnlyWhileWaiting ==
    \A j \in Jobs : (Len(j) > 1 /\ st[j] \notin {"queued", "done"})
                        => st[Parent(j)] = "waiting"

Active == Cardinality({j \in Jobs : st[j] \in Executing})

Bound == Active <= J

ExactBound == Active + pool <= J

SemaphoreMax == pool <= J

WorkConserving == \A p \in ProcSet : ~(asleep[p] /\ pool > 0 /\ ProcActive(p)
                                       /\ \E j \in InProc(p) :
                                            st[j] = "queued"
                                            /\ \A c \in Earlier(j) : st[c] # "queued")

(* Coverage, EXPECTED TO BE VIOLATED in TokenPool_DepthCoverage: jobs at
   the deepest level, in different child processes, really do execute at
   the same time, so the depth-3 checks are not passing on a model that
   serializes the nesting. *)
NeverTwoDeepExecuting ==
    Cardinality({j \in Jobs : Len(j) = D /\ st[j] \in Executing}) <= 1

Termination == <>AllDone

============================================================================

----------------------------- MODULE CycleLock -----------------------------
(***************************************************************************)
(* The interaction of redo-msh's blocking per-target locks with its        *)
(* chain-based cycle detection, over a recorded dependency graph           *)
(* (src/build.rs: ensure, build, is_ood recursion via REDO_CHAIN).         *)
(*                                                                         *)
(* A worker is one traversal with its own chain: a top-level redo process, *)
(* a build_parallel thread, or (the motivating case) a hypothetical        *)
(* parallel is_ood/ensure worker. Each worker recursively brings its start *)
(* target up to date:                                                      *)
(*                                                                         *)
(*   enter   - skip if built this session (runid); ERROR OUT if the target *)
(*             is already on this worker's chain (cycle detected before    *)
(*             locking, unwinding releases all held locks -- RAII);        *)
(*             otherwise proceed to lock.                                  *)
(*   lock    - blocking per-target lock.                                   *)
(*   recheck - the double-check: skip (and unlock) if another worker built *)
(*             it while we waited.                                         *)
(*   deps    - recurse into each recorded dependency in order (each        *)
(*             pushed frame models the child redo-ifchange process, whose  *)
(*             REDO_CHAIN is exactly the targets of the frames below).     *)
(*   commit  - mark built (runid), unlock, pop.                            *)
(*                                                                         *)
(* Everything starts out of date (worst case); content hashing, do-files   *)
(* and the jobserver are abstracted away -- locks and chains are the whole *)
(* story here.                                                             *)
(*                                                                         *)
(* Configurations:                                                         *)
(*   CycleLock_Diamond.cfg       two workers, diamond DAG a->{b,c}->d:     *)
(*                               must terminate, d built exactly once      *)
(*                               (validates lock + double-check on shared  *)
(*                               deps under concurrency).                  *)
(*   CycleLock_CycleSerial.cfg   ONE worker, cyclic graph a<->b: the chain *)
(*                               check fires and the worker fails cleanly  *)
(*                               (today's serial behavior -- sound).       *)
(*   CycleLock_CycleParallel.cfg TWO workers entering the cycle a<->b from *)
(*                               different sides. EXPECTED TO DEADLOCK:    *)
(*                               each worker's chain sees only its own     *)
(*                               path, so neither detects the cycle, and   *)
(*                               they block on each other's locks. This    *)
(*                               models both two concurrent redo           *)
(*                               invocations today AND two branches of one *)
(*                               parallel build reaching a cycle from      *)
(*                               different entry points -- the gap to fix  *)
(*                               before parallelizing the ood traversal.   *)
(***************************************************************************)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS Targets, DepsOf, Workers, StartOf

\* --- graph/worker instances, selected from the .cfg files via <- --------
DiamondTargets == {"a", "b", "c", "d"}
DiamondDeps == ("a" :> <<"b", "c">>) @@ ("b" :> <<"d">>)
               @@ ("c" :> <<"d">>) @@ ("d" :> <<>>)
CycleTargets == {"a", "b"}
CycleDeps == ("a" :> <<"b">>) @@ ("b" :> <<"a">>)
OneWorker == {"w1"}
TwoWorkers == {"w1", "w2"}
StartA == ("w1" :> "a")
StartAB == ("w1" :> "a") @@ ("w2" :> "b")
StartAC == ("w1" :> "a") @@ ("w2" :> "c")

FREE == "FREE"

VARIABLES
    stack,   \* [Workers -> Seq(frame)]: the ensure recursion, innermost last
    status,  \* [Workers -> {"running","done","failed"}]
    lock,    \* [Targets -> holder or FREE]
    runid,   \* [Targets -> BOOLEAN]: built this session
    builds   \* [Targets -> Nat]: commit count (history, for invariants)

vars == <<stack, status, lock, runid, builds>>

Phases == {"enter", "lock", "recheck", "deps", "commit"}

Frame(t) == [t |-> t, i |-> 0, ph |-> "enter"]

TypeOK ==
    /\ status \in [Workers -> {"running", "done", "failed"}]
    /\ lock \in [Targets -> Workers \cup {FREE}]
    /\ runid \in [Targets -> BOOLEAN]
    /\ builds \in [Targets -> Nat]
    /\ \A w \in Workers : \A k \in 1..Len(stack[w]) :
           stack[w][k] \in [t : Targets, i : Nat, ph : Phases]

Init ==
    /\ stack = [w \in Workers |-> <<Frame(StartOf[w])>>]
    /\ status = [w \in Workers |-> "running"]
    /\ lock = [t \in Targets |-> FREE]
    /\ runid = [t \in Targets |-> FALSE]
    /\ builds = [t \in Targets |-> 0]

Busy(w) == status[w] = "running" /\ Len(stack[w]) > 0
TopIdx(w) == Len(stack[w])
Top(w) == stack[w][TopIdx(w)]
Chain(w) == {stack[w][k].t : k \in 1..(Len(stack[w]) - 1)}
Popped(w) == SubSeq(stack[w], 1, Len(stack[w]) - 1)

(* ensure(): already built this session -- skip. *)
EnterSkip(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "enter"
    /\ runid[Top(w).t]
    /\ stack' = [stack EXCEPT ![w] = Popped(w)]
    /\ UNCHANGED <<status, lock, runid, builds>>

(* build(): the chain check, BEFORE locking. The error unwinds the whole
   traversal; every lock the worker holds is released on the way out. *)
EnterCycle(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "enter"
    /\ ~runid[Top(w).t]
    /\ Top(w).t \in Chain(w)
    /\ status' = [status EXCEPT ![w] = "failed"]
    /\ stack' = [stack EXCEPT ![w] = <<>>]
    /\ lock' = [t \in Targets |-> IF lock[t] = w THEN FREE ELSE lock[t]]
    /\ UNCHANGED <<runid, builds>>

EnterLock(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "enter"
    /\ ~runid[Top(w).t]
    /\ Top(w).t \notin Chain(w)
    /\ stack' = [stack EXCEPT ![w][TopIdx(w)].ph = "lock"]
    /\ UNCHANGED <<status, lock, runid, builds>>

(* The blocking per-target lock: enabled only while free. A worker stuck
   here while every other action is disabled is a deadlock, and TLC's
   deadlock detection reports exactly that. *)
Acquire(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "lock"
    /\ lock[Top(w).t] = FREE
    /\ lock' = [lock EXCEPT ![Top(w).t] = w]
    /\ stack' = [stack EXCEPT ![w][TopIdx(w)].ph = "recheck"]
    /\ UNCHANGED <<status, runid, builds>>

(* The double-check: built while we waited for the lock -- skip. *)
RecheckSkip(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "recheck"
    /\ runid[Top(w).t]
    /\ lock' = [lock EXCEPT ![Top(w).t] = FREE]
    /\ stack' = [stack EXCEPT ![w] = Popped(w)]
    /\ UNCHANGED <<status, runid, builds>>

RecheckBuild(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "recheck"
    /\ ~runid[Top(w).t]
    /\ stack' = [stack EXCEPT ![w][TopIdx(w)].ph = "deps"]
    /\ UNCHANGED <<status, lock, runid, builds>>

(* Recurse into the next recorded dependency (a child redo-ifchange). *)
DepNext(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "deps"
    /\ Top(w).i < Len(DepsOf[Top(w).t])
    /\ LET st == stack[w]
           n == Len(st)
           f == st[n]
           d == DepsOf[f.t][f.i + 1]
       IN stack' = [stack EXCEPT
                        ![w] = Append([st EXCEPT ![n].i = f.i + 1], Frame(d))]
    /\ UNCHANGED <<status, lock, runid, builds>>

DepsDone(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "deps"
    /\ Top(w).i = Len(DepsOf[Top(w).t])
    /\ stack' = [stack EXCEPT ![w][TopIdx(w)].ph = "commit"]
    /\ UNCHANGED <<status, lock, runid, builds>>

Commit(w) ==
    /\ Busy(w)
    /\ Top(w).ph = "commit"
    /\ runid' = [runid EXCEPT ![Top(w).t] = TRUE]
    /\ builds' = [builds EXCEPT ![Top(w).t] = @ + 1]
    /\ lock' = [lock EXCEPT ![Top(w).t] = FREE]
    /\ stack' = [stack EXCEPT ![w] = Popped(w)]
    /\ UNCHANGED status

Finish(w) ==
    /\ status[w] = "running"
    /\ Len(stack[w]) = 0
    /\ status' = [status EXCEPT ![w] = "done"]
    /\ UNCHANGED <<stack, lock, runid, builds>>

WStep(w) ==
    \/ EnterSkip(w) \/ EnterCycle(w) \/ EnterLock(w)
    \/ Acquire(w) \/ RecheckSkip(w) \/ RecheckBuild(w)
    \/ DepNext(w) \/ DepsDone(w) \/ Commit(w) \/ Finish(w)

Terminating ==
    /\ \A w \in Workers : status[w] \in {"done", "failed"}
    /\ UNCHANGED vars

Next == (\E w \in Workers : WStep(w)) \/ Terminating

Spec == Init /\ [][Next]_vars /\ (\A w \in Workers : WF_vars(WStep(w)))

----------------------------------------------------------------------------
(* Each target is built at most once per session (the double-check works
   even when two workers race to the same shared dependency). *)
BuildOnce == \A t \in Targets : builds[t] <= 1

(* No lock outlives its worker's traversal (RAII unwinding is faithful). *)
LockOwnedByBusy ==
    \A t \in Targets : lock[t] # FREE => status[lock[t]] = "running"

EventuallyAllDone == <>(\A w \in Workers : status[w] = "done")

EventuallyHalted == <>(\A w \in Workers : status[w] # "running")

============================================================================

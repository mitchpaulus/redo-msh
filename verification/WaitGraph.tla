----------------------------- MODULE WaitGraph -----------------------------
(***************************************************************************)
(* The PROPOSED waits-for graph (ParallelEnsure / Speculation, contract    *)
(* items 1, 2, 4 and 6) across SESSIONS and CRASHES -- the case those      *)
(* single-session specs cannot see.                                        *)
(*                                                                         *)
(* The graph lives in the shared database: an edge (t, d, s) says "the     *)
(* builder of t, in session s, is waiting for d". Before adding an edge, a *)
(* builder checks -- atomically with the insert -- whether t is reachable  *)
(* from d; if so it fails t with a cycle error instead of blocking.        *)
(*                                                                         *)
(* Each session traverses from its roots over the deps ITS do-files        *)
(* declare (DepsOf[s]): do-files can change between runs, so two sessions  *)
(* may see different graphs. Per target, a session's traversal:            *)
(*   claiming - waiting for the target's kernel lock (or for someone else  *)
(*              to finish building it);                                    *)
(*   active   - holds the lock, its do-file is declaring deps: for each    *)
(*              dep, skip it if built, fail on its failure, or check-and-  *)
(*              insert a wait edge and start traversing it;                *)
(*   done / failed.                                                        *)
(* A session can crash at any point (at most MaxCrashes in total): the OS  *)
(* releases its locks, but its edges stay in the database. GcByLiveness    *)
(* (contract item 6) makes reachability ignore edges whose session is      *)
(* dead (equivalently: they are swept under the same liveness rule as the  *)
(* locks).                                                                 *)
(*                                                                         *)
(* Properties:                                                             *)
(*   NoFalseCycle - a cycle error is raised only when the new edge closes  *)
(*                  a cycle through edges of LIVE builders (a real         *)
(*                  deadlock that would otherwise happen).                 *)
(*   Termination  - every live session's traversal settles (no deadlock,   *)
(*                  also across sessions blocking on each other's locks).  *)
(*   SomeoneFails - (cyclic configs) the cycle is reported, never hung.    *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Sessions, Targets, DepsOf, RootsOf, MaxCrashes, GcByLiveness

ASSUME /\ MaxCrashes \in Nat /\ GcByLiveness \in BOOLEAN

\* --- instances, selected from the .cfg files via <- ---------------------
TwoSessions == {"s1", "s2"}
OneSession == {"s1"}
AB == {"a", "b"}
GraphAtoB == ("a" :> {"b"}) @@ ("b" :> {})
GraphBtoA == ("a" :> {}) @@ ("b" :> {"a"})
GraphCycle == ("a" :> {"b"}) @@ ("b" :> {"a"})
\* s1's do-files say a -> b; by the time s2 runs, they say b -> a.
ChangedGraphs == ("s1" :> GraphAtoB) @@ ("s2" :> GraphBtoA)
CycleGraph1 == ("s1" :> GraphCycle)
RootsAB2 == ("s1" :> {"a"}) @@ ("s2" :> {"b"})
RootsA1 == ("s1" :> {"a"})

FREE == "FREE"

VARIABLES
    ts,          \* [Sessions -> [Targets -> traversal state]]
    pend,        \* [Sessions -> [Targets -> SUBSET Targets]]: deps not yet declared
    claim,       \* [Targets -> session holding the target's lock, or FREE]
    built,       \* [Targets -> BOOLEAN]
    E,           \* the database's wait edges: set of <<t, d, session>>
    dead,        \* [Sessions -> BOOLEAN]
    crashes,
    falseCycle   \* history: a cycle error not backed by live edges

vars == <<ts, pend, claim, built, E, dead, crashes, falseCycle>>

TStates == {"idle", "claiming", "active", "done", "failed"}

TypeOK ==
    /\ ts \in [Sessions -> [Targets -> TStates]]
    /\ pend \in [Sessions -> [Targets -> SUBSET Targets]]
    /\ claim \in [Targets -> Sessions \cup {FREE}]
    /\ built \in [Targets -> BOOLEAN]
    /\ E \subseteq (Targets \X Targets \X Sessions)
    /\ dead \in [Sessions -> BOOLEAN]
    /\ crashes \in 0..MaxCrashes
    /\ falseCycle \in BOOLEAN

Init ==
    /\ ts = [s \in Sessions |-> [t \in Targets |->
               IF t \in RootsOf[s] THEN "claiming" ELSE "idle"]]
    /\ pend = [s \in Sessions |-> [t \in Targets |-> {}]]
    /\ claim = [t \in Targets |-> FREE]
    /\ built = [t \in Targets |-> FALSE]
    /\ E = {}
    /\ dead = [s \in Sessions |-> FALSE]
    /\ crashes = 0
    /\ falseCycle = FALSE

(* Reachability over a set of edges. *)
RECURSIVE ReachFrom(_, _)
ReachFrom(S, Es) ==
    LET nxt == S \cup {e[2] : e \in {e \in Es : e[1] \in S}}
    IN IF nxt = S THEN S ELSE ReachFrom(nxt, Es)

LiveEdges == {e \in E : ~dead[e[3]]}
CheckedEdges == IF GcByLiveness THEN LiveEdges ELSE E

Reach(x, y, Es) == y \in ReachFrom({x}, Es)

Settle(s, t, outcome) ==
    /\ ts' = [ts EXCEPT ![s][t] = outcome]
    /\ claim' = [claim EXCEPT ![t] = FREE]
    /\ E' = {e \in E : ~(e[1] = t /\ e[3] = s)}
    /\ pend' = [pend EXCEPT ![s][t] = {}]

----------------------------------------------------------------------------
(* Someone else built it while we waited: nothing to do. *)
SkipBuilt(s, t) ==
    /\ ~dead[s] /\ ts[s][t] = "claiming" /\ built[t]
    /\ ts' = [ts EXCEPT ![s][t] = "done"]
    /\ UNCHANGED <<pend, claim, built, E, dead, crashes, falseCycle>>

(* Take the target's lock; the do-file starts declaring deps. *)
Claim(s, t) ==
    /\ ~dead[s] /\ ts[s][t] = "claiming" /\ ~built[t]
    /\ claim[t] = FREE
    /\ claim' = [claim EXCEPT ![t] = s]
    /\ ts' = [ts EXCEPT ![s][t] = "active"]
    /\ pend' = [pend EXCEPT ![s][t] = DepsOf[s][t]]
    /\ UNCHANGED <<built, E, dead, crashes, falseCycle>>

(* Declared dep already built: nothing to wait for. *)
DepBuilt(s, t, d) ==
    /\ ~dead[s] /\ ts[s][t] = "active" /\ d \in pend[s][t] /\ built[d]
    /\ pend' = [pend EXCEPT ![s][t] = @ \ {d}]
    /\ UNCHANGED <<ts, claim, built, E, dead, crashes, falseCycle>>

(* Wait for a declared dep: the atomic check-and-insert. *)
DepWait(s, t, d) ==
    /\ ~dead[s] /\ ts[s][t] = "active" /\ d \in pend[s][t] /\ ~built[d]
    /\ ts[s][d] # "failed"
    /\ IF Reach(d, t, CheckedEdges)
         THEN /\ falseCycle' = (falseCycle \/ ~Reach(d, t, LiveEdges))
              /\ Settle(s, t, "failed")                  \* cycle error
              /\ UNCHANGED built
         ELSE /\ E' = E \cup {<<t, d, s>>}
              /\ pend' = [pend EXCEPT ![s][t] = @ \ {d}]
              /\ ts' = IF ts[s][d] = "idle"
                         THEN [ts EXCEPT ![s][d] = "claiming"] ELSE ts
              /\ UNCHANGED <<claim, built, falseCycle>>
    /\ UNCHANGED <<dead, crashes>>

(* A dep we need failed in our session: the failure propagates. *)
DepFailed(s, t) ==
    /\ ~dead[s] /\ ts[s][t] = "active"
    /\ \E d \in DepsOf[s][t] : ts[s][d] = "failed" /\ ~built[d]
    /\ Settle(s, t, "failed")
    /\ UNCHANGED <<built, dead, crashes, falseCycle>>

(* Every declared dep built: commit and release. *)
Build(s, t) ==
    /\ ~dead[s] /\ ts[s][t] = "active" /\ pend[s][t] = {}
    /\ \A d \in DepsOf[s][t] : built[d]
    /\ built' = [built EXCEPT ![t] = TRUE]
    /\ Settle(s, t, "done")
    /\ UNCHANGED <<dead, crashes, falseCycle>>

(* A session dies: the OS releases its locks; its edges stay behind. *)
Crash(s) ==
    /\ ~dead[s] /\ crashes < MaxCrashes
    /\ dead' = [dead EXCEPT ![s] = TRUE]
    /\ crashes' = crashes + 1
    /\ claim' = [t \in Targets |-> IF claim[t] = s THEN FREE ELSE claim[t]]
    /\ UNCHANGED <<ts, pend, built, E, falseCycle>>

SStep(s) ==
    \E t \in Targets :
        \/ SkipBuilt(s, t) \/ Claim(s, t) \/ DepFailed(s, t) \/ Build(s, t)
        \/ \E d \in Targets : DepBuilt(s, t, d) \/ DepWait(s, t, d)

Settled(s) == dead[s] \/ \A t \in Targets : ts[s][t] \in {"idle", "done", "failed"}

AllSettled == \A s \in Sessions : Settled(s)

Terminating == AllSettled /\ UNCHANGED vars

Next == (\E s \in Sessions : SStep(s) \/ Crash(s)) \/ Terminating

Spec == Init /\ [][Next]_vars /\ (\A s \in Sessions : WF_vars(SStep(s)))

----------------------------------------------------------------------------
NoFalseCycle == ~falseCycle

Termination == <>AllSettled

SomeoneFails == <>(\E s \in Sessions, t \in Targets : ts[s][t] = "failed")

(* Coverage, EXPECTED TO BE VIOLATED in WaitGraph_Coverage: two live
   sessions really do reach the mutual wait and report it, so LiveCross's
   passing properties are not vacuous. *)
NeverFails == \A s \in Sessions, t \in Targets : ts[s][t] # "failed"

============================================================================

--------------------------- MODULE ParallelEnsure ---------------------------
(***************************************************************************)
(* The PROPOSED aggressive parallel traversal for redo-msh -- the design   *)
(* to implement, specified and checked before writing the Rust.            *)
(*                                                                         *)
(* Idea: the dependency edges recorded in the database from the last run   *)
(* are a complete parallelization plan. ensure(t) fans out over ALL of     *)
(* t's recorded deps at once -- activating each dep's own ensure           *)
(* speculatively and in parallel -- instead of walking them serially as    *)
(* is_ood does today. Checking is unbounded (cheap); running do-files      *)
(* ("building") is bounded by the J-token budget. A target settles         *)
(* ("done" or "failed") only after every dep has settled.                  *)
(*                                                                         *)
(* Per-target lifecycle:                                                   *)
(*   unreq    - nobody has asked for it this session.                      *)
(*   active   - claimed (exactly once per session: the in-process task     *)
(*              registry + the per-target lock verified in LockSession);   *)
(*              fanning out wait edges to its deps and waiting for them.   *)
(*   building - all deps settled ok, target judged out of date, do-file    *)
(*              running under a token.                                     *)
(*   done     - built or verified up to date.                              *)
(*   failed   - a cycle was detected at this target, a dep failed (errors  *)
(*              propagate up, releasing resources), or its do-file failed. *)
(*                                                                         *)
(* THE FIX REQUIRED BY CycleLock_CycleParallel (which proves per-traversal *)
(* chains deadlock): a shared WAITS-FOR graph. Before target t starts      *)
(* waiting on dep d, check whether t is reachable from d through the live  *)
(* wait edges; if so, adding the edge would close a cycle -- fail t with a *)
(* cycle error instead. The check and the edge insert are ONE ATOMIC       *)
(* ACTION here; the implementation MUST make them one SQLite write         *)
(* transaction, or two concurrent inserts could each miss the other's      *)
(* edge and recreate the deadlock.                                         *)
(*                                                                         *)
(* Out-of-date evaluation is abstracted to a nondeterministic choice at    *)
(* readiness (verify vs. rebuild), covering every clean/dirty combination. *)
(* Do-file failure is a nondeterministic build outcome, switchable per     *)
(* configuration (BuildsCanFail) so the cycle configuration can prove that *)
(* cycle detection is the ONLY failure source there.                       *)
(*                                                                         *)
(* Checked properties:                                                     *)
(*   BuildOnce        - at most one build per target per session.          *)
(*   TokenBound       - running do-files never exceed J; tokens are        *)
(*                      returned on success AND failure.                   *)
(*   DepsSettledFirst - a target never builds or settles before every      *)
(*                      recorded dep has settled (the ordering the serial  *)
(*                      walk gives for free, preserved under concurrency). *)
(*   EventuallyQuiescent / Settles - every activated target settles; no    *)
(*                      deadlock on ANY graph, cyclic ones included (TLC's *)
(*                      deadlock check runs on every configuration).       *)
(*   CycleErrorsOut   - on the cyclic graph, every interleaving ends with  *)
(*                      a reported cycle error -- the scenario that        *)
(*                      deadlocks in CycleLock_CycleParallel.              *)
(*   NeverTwoBuilding - a COVERAGE check, expected to be VIOLATED: proves  *)
(*                      the model really reaches states with parallel      *)
(*                      builds (guards against a vacuously serial model).  *)
(*                                                                         *)
(* Work-conservation note: in this spec every launchable step is an        *)
(* enabled action and fairness forces it -- the scheduler is eager by      *)
(* construction. That is the OBLIGATION the implementation inherits: a     *)
(* ready target with a free token must eventually launch (no waiting only  *)
(* on one's own children while other tokens sit free).                     *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Targets, DepsOf, Roots, J, BuildsCanFail

ASSUME /\ Roots \subseteq Targets
       /\ J \in Nat \ {0}
       /\ BuildsCanFail \in BOOLEAN

\* --- graph/root instances, selected from the .cfg files via <- ----------
DiamondTargets == {"a", "b", "c", "d"}
DiamondDeps == ("a" :> {"b", "c"}) @@ ("b" :> {"d"})
               @@ ("c" :> {"d"}) @@ ("d" :> {})
WideTargets == {"a", "b", "c", "d", "e"}
WideDeps == ("a" :> {"b", "c", "d"}) @@ ("b" :> {"e"})
            @@ ("c" :> {"e"}) @@ ("d" :> {"e"}) @@ ("e" :> {})
CycleTargets == {"a", "b"}
CycleDeps == ("a" :> {"b"}) @@ ("b" :> {"a"})
RootsA == {"a"}
RootsAB == {"a", "b"}

VARIABLES
    st,       \* [Targets -> lifecycle state]
    pending,  \* [Targets -> SUBSET Targets]: deps not yet edge-processed
    edges,    \* [Targets -> SUBSET Targets]: live waits-for edges (the DB);
              \* cleared when the target settles, so reachability sees only
              \* traversals that can still block
    tokens,   \* free do-file tokens
    builds    \* [Targets -> Nat]: commit count (history, for invariants)

vars == <<st, pending, edges, tokens, builds>>

States == {"unreq", "active", "building", "done", "failed"}

TypeOK ==
    /\ st \in [Targets -> States]
    /\ \A t \in Targets : pending[t] \subseteq DepsOf[t]
    /\ \A t \in Targets : edges[t] \subseteq DepsOf[t]
    /\ tokens \in 0..J
    /\ builds \in [Targets -> Nat]

Init ==
    /\ st = [t \in Targets |-> IF t \in Roots THEN "active" ELSE "unreq"]
    /\ pending = [t \in Targets |-> IF t \in Roots THEN DepsOf[t] ELSE {}]
    /\ edges = [t \in Targets |-> {}]
    /\ tokens = J
    /\ builds = [t \in Targets |-> 0]

(* Everything reachable from S through the live waits-for edges. *)
RECURSIVE Closure(_)
Closure(S) ==
    LET nxt == S \cup UNION {edges[x] : x \in S}
    IN IF nxt = S THEN S ELSE Closure(nxt)

CanReach(x, y) == y \in Closure({x})

----------------------------------------------------------------------------
(* Fan-out: target t processes one recorded dep d. Four cases by d's
   state; each is atomic (one DB transaction in the implementation). *)

(* d already settled ok this session: the wait is satisfied immediately. *)
AddEdgeDone(t, d) ==
    /\ st[t] = "active"
    /\ d \in pending[t]
    /\ st[d] = "done"
    /\ pending' = [pending EXCEPT ![t] = @ \ {d}]
    /\ UNCHANGED <<st, edges, tokens, builds>>

(* d already failed: the error propagates into t, abandoning its fan-out. *)
AddEdgeFailedDep(t, d) ==
    /\ st[t] = "active"
    /\ d \in pending[t]
    /\ st[d] = "failed"
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ pending' = [pending EXCEPT ![t] = {}]
    /\ edges' = [edges EXCEPT ![t] = {}]
    /\ UNCHANGED <<tokens, builds>>

(* d untouched this session: claim it and fan its own check out too --
   the speculative parallelism over the recorded graph. *)
AddEdgeFresh(t, d) ==
    /\ st[t] = "active"
    /\ d \in pending[t]
    /\ st[d] = "unreq"
    /\ st' = [st EXCEPT ![d] = "active"]
    /\ pending' = [pending EXCEPT ![t] = @ \ {d}, ![d] = DepsOf[d]]
    /\ edges' = [edges EXCEPT ![t] = @ \cup {d}]
    /\ UNCHANGED <<tokens, builds>>

(* d is in flight: wait for it -- UNLESS waiting would close a cycle in
   the waits-for graph, in which case t fails with a cycle error. The
   reachability check and the edge insert are one atomic action; the
   implementation must give them the same atomicity (one write txn). *)
AddEdgeActive(t, d) ==
    /\ st[t] = "active"
    /\ d \in pending[t]
    /\ st[d] \in {"active", "building"}
    /\ IF CanReach(d, t)
         THEN /\ st' = [st EXCEPT ![t] = "failed"]
              /\ pending' = [pending EXCEPT ![t] = {}]
              /\ edges' = [edges EXCEPT ![t] = {}]
         ELSE /\ pending' = [pending EXCEPT ![t] = @ \ {d}]
              /\ edges' = [edges EXCEPT ![t] = @ \cup {d}]
              /\ UNCHANGED st
    /\ UNCHANGED <<tokens, builds>>

(* A dep t was waiting on failed after the edge was added: propagate. *)
ObserveDepFail(t) ==
    /\ st[t] = "active"
    /\ \E d \in edges[t] : st[d] = "failed"
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ pending' = [pending EXCEPT ![t] = {}]
    /\ edges' = [edges EXCEPT ![t] = {}]
    /\ UNCHANGED <<tokens, builds>>

(* All deps processed and settled ok: t may be judged. *)
Ready(t) ==
    /\ st[t] = "active"
    /\ pending[t] = {}
    /\ \A d \in edges[t] : st[d] = "done"

(* Judged up to date: settle without consuming a token. *)
Verify(t) ==
    /\ Ready(t)
    /\ st' = [st EXCEPT ![t] = "done"]
    /\ edges' = [edges EXCEPT ![t] = {}]
    /\ UNCHANGED <<pending, tokens, builds>>

(* Judged out of date: run the do-file under a token. *)
BeginBuild(t) ==
    /\ Ready(t)
    /\ tokens > 0
    /\ tokens' = tokens - 1
    /\ st' = [st EXCEPT ![t] = "building"]
    /\ UNCHANGED <<pending, edges, builds>>

FinishBuild(t) ==
    /\ st[t] = "building"
    /\ tokens' = tokens + 1
    /\ st' = [st EXCEPT ![t] = "done"]
    /\ builds' = [builds EXCEPT ![t] = @ + 1]
    /\ edges' = [edges EXCEPT ![t] = {}]
    /\ UNCHANGED pending

(* The do-file fails: the token must still come back. *)
FinishBuildFail(t) ==
    /\ BuildsCanFail
    /\ st[t] = "building"
    /\ tokens' = tokens + 1
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ edges' = [edges EXCEPT ![t] = {}]
    /\ UNCHANGED <<pending, builds>>

TStep(t) ==
    \/ \E d \in DepsOf[t] :
         \/ AddEdgeDone(t, d) \/ AddEdgeFailedDep(t, d)
         \/ AddEdgeFresh(t, d) \/ AddEdgeActive(t, d)
    \/ ObserveDepFail(t)
    \/ Verify(t) \/ BeginBuild(t)
    \/ FinishBuild(t) \/ FinishBuildFail(t)

Quiescent == \A t \in Targets : st[t] \in {"unreq", "done", "failed"}

Terminating == Quiescent /\ UNCHANGED vars

Next == (\E t \in Targets : TStep(t)) \/ Terminating

Spec == Init /\ [][Next]_vars /\ (\A t \in Targets : WF_vars(TStep(t)))

----------------------------------------------------------------------------
BuildOnce == \A t \in Targets : builds[t] <= 1

(* Running do-files and free tokens always account for exactly J. *)
TokenBound == Cardinality({t \in Targets : st[t] = "building"}) = J - tokens

(* A target never builds or settles ok before every recorded dep settled
   ok -- the ordering the serial walk provides, preserved under
   concurrency. *)
DepsSettledFirst ==
    \A t \in Targets :
        st[t] \in {"building", "done"} =>
            \A d \in DepsOf[t] : st[d] = "done"

(* Coverage check, EXPECTED TO BE VIOLATED in its configuration: proves
   the model reaches genuinely parallel builds. *)
NeverTwoBuilding == Cardinality({t \in Targets : st[t] = "building"}) <= 1

EventuallyQuiescent == <>Quiescent

Settles ==
    \A t \in Targets :
        (st[t] = "active") ~> (st[t] \in {"done", "failed"})

(* For the cyclic configuration (with BuildsCanFail = FALSE, so cycle
   detection is the only failure source): every interleaving reports the
   cycle as an error on every involved target -- the scenario that
   deadlocks under today's chain-based detection. *)
CycleErrorsOut == <>(\A t \in Targets : st[t] = "failed")

============================================================================

--------------------------- MODULE SpeculationMP ---------------------------
(***************************************************************************)
(* The first-principles multi-process ensure machine — the design the      *)
(* implementation must be rebuilt to match.                                *)
(*                                                                         *)
(* WHY THIS SPEC EXISTS. Speculation.tla proves its properties for a       *)
(* machine with ONE global claim per target and no kernel locks. The       *)
(* implementation has one claim per target PER PROCESS; second instances   *)
(* of an in-flight target block on the per-target kernel lock, and every   *)
(* redo-ifchange return drains ALL of its process's tasks — and neither    *)
(* of those waits appeared in the shared waits-for graph. Result (both     *)
(* reproduced against the shipping branch): a deterministic deadlock on    *)
(* an ACYCLIC project whose stale recorded deps cross two branches         *)
(* (SpeculationMP_CrossStale is exactly that shape), and fabricated        *)
(* cycle errors from speculative tasks. Separately, Speculation.tla's      *)
(* own BWait hard-fails a cycle that rides a SPECULATIVE edge              *)
(* (Speculation_Reversed.cfg), refuting the "stale edges never invent an   *)
(* error" claim.                                                           *)
(*                                                                         *)
(* THE CORRECT MACHINE. This spec models the real execution shape          *)
(* honestly — multiple processes, per-process registries, per-target       *)
(* kernel build locks, drain-before-return — and specifies corrected       *)
(* design rules:                                                           *)
(*                                                                         *)
(*   R1 (complete graph): every wait that blocks until a target settles    *)
(*      is an edge in ONE shared by-name graph, inserted atomically with   *)
(*      a cycle check. That includes the two waits the old design left     *)
(*      out: waiting on a speculative instance — covered by a CREATION     *)
(*      edge ctx -> s inserted when the instance is spawned, ctx being     *)
(*      the target whose do-file owns the spawning process; this is what   *)
(*      makes drain's waits visible — and checker waits. Kernel-lock       *)
(*      waits carry no edge of their own and are PROVEN safe here: edges   *)
(*      are keyed by target NAME, so waiter -> name plus the foreign       *)
(*      builder's own name -> deps edges bridge the lock wait.             *)
(*                                                                         *)
(*   R2 (typed edges): checker waits and creation edges are SOFT           *)
(*      (speculative — nobody asked for this work); running-do-file        *)
(*      demands are HARD (ground truth).                                   *)
(*                                                                         *)
(*   R3 (cycle rules):                                                     *)
(*      - soft insert, any cycle       -> refuse softly: a checker takes   *)
(*        the rebuild path; a speculative instance is simply not created   *)
(*        (speculation that could close a cycle never starts).             *)
(*      - hard insert, all-hard cycle  -> a real dependency cycle: error.  *)
(*      - hard insert, cycle riding a soft edge -> the SPECULATION         *)
(*        yields, never the demand: evict a soft edge on the cycle (its    *)
(*        checker takes the rebuild path; its speculative instance         *)
(*        aborts, retryably), then the demand retries.                     *)
(*                                                                         *)
(*   R4 (speculative outcomes are quarantined): a speculative failure or   *)
(*      abort settles as "sfail", is reported to no one, and a later       *)
(*      real demand RECLAIMS the instance and re-runs it. Demanding a      *)
(*      live speculative instance upgrades it to demanded (its creation    *)
(*      edge is superseded by the hard demand edge), making it             *)
(*      un-abortable.                                                      *)
(*                                                                         *)
(*   R5 (drain cancellation): a draining process may ABANDON any of its    *)
(*      still-speculative instances instead of waiting for them, in any    *)
(*      state including mid-build (the implementation kills the do-file;   *)
(*      crash-safety covers it). Abandonment settles "sfail" like any      *)
(*      other speculative outcome. This bounds ifchange return latency:    *)
(*      undemanded speculation can never hold the caller hostage.          *)
(*                                                                         *)
(*   R6 (hand-edited targets): the overwrite guard runs after the kernel   *)
(*      lock is won and before the do-file. A target the user may own     *)
(*      (HandEdited) is only ever ASKED ABOUT by an instance whose whole   *)
(*      lineage -- itself and every builder process above it up to top --  *)
(*      is demanded ("prompt", lock held, answer yes -> build, no ->       *)
(*      fail). Anywhere else (a speculative instance, a demanded instance  *)
(*      inside a speculative lineage, an orphan whose builder is gone)     *)
(*      the hand-edit ABORTS THE LINEAGE: the nearest speculative root is  *)
(*      killed with its whole process subtree, exactly like an R5 kill,   *)
(*      settling "sfail". Why not just refuse (fail) quietly there: a      *)
(*      demand can upgrade the lineage before the failure is observed,    *)
(*      and the refusal would then be REPORTED without the user having    *)
(*      been asked. With sfail, the demand reclaims and re-runs the        *)
(*      instance in a demanded lineage, which asks. Answers are per prompt *)
(*      and nondeterministic (PromptAnswers); the session-wide "all" and   *)
(*      "quit" answers are refinements of that.                            *)
(*                                                                         *)
(* Tokens are out of scope (TokenPool.tla verifies that budget             *)
(* independently; token waits cannot deadlock by the own-token argument).  *)
(* One redo-ifchange call per do-file is modeled; sequential calls repeat  *)
(* the same protocol. As in Speculation.tla, `clean` (content verdicts)    *)
(* is frozen at Init and nondeterministic (TLC explores every subset of   *)
(* Cleanable).                                                             *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Targets, Recorded, Actual, Roots, Cleanable, BuildsCanFail,
          HandEdited, PromptAnswers

ASSUME /\ Roots \subseteq Targets
       /\ Cleanable \subseteq Targets
       /\ BuildsCanFail \in BOOLEAN
       /\ HandEdited \subseteq Targets
       /\ PromptAnswers \subseteq {"yes", "no"}

(* Process identities: "top" (the user's invocation) plus, for every       *)
(* target, the redo-ifchange child process of that target's do-file. A     *)
(* process's context name is itself (the target whose do-file spawned      *)
(* it); "top" has no name — nothing can wait on it.                        *)
Top == "top"
Procs == {Top} \cup Targets

(* --- instances selected from the .cfg files via <- ----------------------*)
CrossTargets == {"t1", "t2", "d1", "d2"}
\* The reproduced implementation deadlock: stale recorded deps cross two
\* parallel branches; the real (Actual) graph is acyclic.
CrossRecorded == ("t1" :> {}) @@ ("t2" :> {})
                 @@ ("d1" :> {"t2"}) @@ ("d2" :> {"t1"})
CrossActual == ("t1" :> {"d1"}) @@ ("t2" :> {"d2"})
               @@ ("d1" :> {}) @@ ("d2" :> {})
RootsT1T2 == {"t1", "t2"}
TwoTargets == {"a", "b"}
\* The dependency reversal that refutes Speculation.tla's severity rule.
RevRecorded == ("a" :> {"b"}) @@ ("b" :> {})
RevActual == ("a" :> {}) @@ ("b" :> {"a"})
\* A genuinely cyclic project, empty database (pure discovery-time).
NoRecorded2 == ("a" :> {}) @@ ("b" :> {})
CycleActual == ("a" :> {"b"}) @@ ("b" :> {"a"})
RootsAB == {"a", "b"}
RootsA == {"a"}
NoTargets == {}
\* Speculation.tla's stale instance: b dropped, c kept, d discovered.
StaleTargets == {"a", "b", "c", "d"}
StaleRecorded == ("a" :> {"b", "c"}) @@ ("b" :> {}) @@ ("c" :> {}) @@ ("d" :> {})
StaleActual == ("a" :> {"c", "d"}) @@ ("b" :> {}) @@ ("c" :> {}) @@ ("d" :> {})
\* Mid-build speculation abort: checking u speculatively starts building s
\* (stale recorded u -> s), and s's own do-file then demands t — the very
\* target whose subtree spawned s. The demand's cycle rides s's creation
\* edge, so the speculative build of s must abort retryably (never fail t).
AbortTargets == {"t", "s", "u"}
AbortRecorded == ("t" :> {}) @@ ("s" :> {}) @@ ("u" :> {"s"})
AbortActual == ("t" :> {"u"}) @@ ("s" :> {"t"}) @@ ("u" :> {})
RootsT == {"t"}
\* R6 instances. A stale recorded edge a -> b speculatively builds b, whose
\* do-file demands hand-edited x. In LinDrop, a no longer needs b: x must
\* never be asked about. In LinKeep, a still needs b: the lineage is
\* upgraded (before or after the abort), and x is asked about exactly on
\* the demanded path.
LinTargets == {"a", "b", "c", "x"}
LinRecorded == ("a" :> {"b"}) @@ ("b" :> {}) @@ ("c" :> {}) @@ ("x" :> {})
LinDropActual == ("a" :> {"c"}) @@ ("b" :> {"x"}) @@ ("c" :> {}) @@ ("x" :> {})
LinKeepActual == ("a" :> {"b"}) @@ ("b" :> {"x"}) @@ ("c" :> {}) @@ ("x" :> {})
HandX == {"x"}
HandS == {"s"}
HandCD == {"c", "d"}
Yes == {"yes"}
No == {"no"}
YesNo == {"yes", "no"}

VARIABLES
    ist,     \* [Procs -> [Targets -> IState]] per-process instance states
    grade,   \* [Procs -> [Targets -> {"spec","dem","-"}]]
    cpend,   \* [Procs -> [Targets -> SUBSET Targets]] recorded deps unresolved
    mustR,   \* [Procs -> [Targets -> BOOLEAN]] soft failure: verify is off
    apend,   \* [Targets -> SUBSET Targets] actual deps unresolved (lock held)
    edges,   \* SUBSET [own: Procs, w: Targets, d: Targets, soft: BOOLEAN]
    lock,    \* [Targets -> Procs \cup {"-"}] the kernel build lock
    mark,    \* [Targets -> {"-","verified","committed"}] session runid mark
    clean,   \* frozen content verdicts (subset of Cleanable)
    builds   \* [Targets -> Nat] commit counter (history, for invariants)

vars == <<ist, grade, cpend, mustR, apend, edges, lock, mark, clean, builds>>

(* Instance lifecycle. "new" = claimed, not yet activated. "lockw" =       *)
(* blocked on the kernel lock (edge-free by design, R1). "sfail" =         *)
(* speculative failure/abort: quarantined, reclaimable by a demand.        *)
(* "prompt" = lock held, waiting for the human's overwrite answer (R6).   *)
IStates == {"none", "new", "checking", "lockw", "prompt", "brun", "bdrain",
            "done", "sfail", "fail"}
InFlight == {"new", "checking", "lockw", "prompt", "brun", "bdrain"}
SettledSt == {"none", "done", "sfail", "fail"}

TypeOK ==
    /\ ist \in [Procs -> [Targets -> IStates]]
    /\ grade \in [Procs -> [Targets -> {"spec", "dem", "-"}]]
    /\ \A p \in Procs, t \in Targets :
         /\ cpend[p][t] \subseteq Recorded[t]
         /\ (ist[p][t] = "none") <=> (grade[p][t] = "-")
    /\ mustR \in [Procs -> [Targets -> BOOLEAN]]
    /\ \A t \in Targets : apend[t] \subseteq Actual[t]
    /\ edges \subseteq [own: Procs, w: Targets, d: Targets, soft: BOOLEAN]
    /\ lock \in [Targets -> Procs \cup {"-"}]
    /\ mark \in [Targets -> {"-", "verified", "committed"}]
    /\ clean \subseteq Cleanable \ HandEdited
    /\ builds \in [Targets -> Nat]

Init ==
    /\ ist = [p \in Procs |-> [t \in Targets |->
                IF p = Top /\ t \in Roots THEN "new" ELSE "none"]]
    /\ grade = [p \in Procs |-> [t \in Targets |->
                  IF p = Top /\ t \in Roots THEN "dem" ELSE "-"]]
    /\ cpend = [p \in Procs |-> [t \in Targets |-> {}]]
    /\ mustR = [p \in Procs |-> [t \in Targets |-> FALSE]]
    /\ apend = [t \in Targets |-> {}]
    /\ edges = {}
    /\ lock = [t \in Targets |-> "-"]
    /\ mark = [t \in Targets |-> "-"]
    \* a hand-edited file never matches its recorded hash: never clean
    /\ clean \in SUBSET (Cleanable \ HandEdited)
    /\ builds = [t \in Targets |-> 0]

----------------------------------------------------------------------------
(* Reachability over an edge set, by target name.                          *)

RECURSIVE ClosureFrom(_, _)
ClosureFrom(S, E) ==
    LET nxt == S \cup {e.d : e \in {ee \in E : ee.w \in S}}
    IN IF nxt = S THEN S ELSE ClosureFrom(nxt, E)

Reach(x, y, E) == y \in ClosureFrom({x}, E)

HardEdges == {e \in edges : ~e.soft}

(* Would the wait w -> d close a cycle (through edges of any type)?        *)
Cyc(w, d) == Reach(d, w, edges)
(* Is there a d ~> w path made ONLY of hard edges (a real cycle)?          *)
CycHard(w, d) == Reach(d, w, HardEdges)

(* Soft edges lying on some d ~> w path: the eviction candidates. When a   *)
(* cycle exists but no all-hard one does, at least one of these exists.    *)
SoftOnPath(w, d) ==
    {e \in edges : e.soft /\ Reach(d, e.w, edges) /\ Reach(e.d, w, edges)}

(* A soft edge whose waiter name equals its owning process (and is not     *)
(* top) is a creation edge; every other soft edge is a checker edge.       *)
IsCreation(e) == e.soft /\ e.own = e.w /\ e.own # Top

(* Edge bookkeeping: an instance's own waits are the edges carrying its    *)
(* target as waiter name, inserted by its process; a builder's child       *)
(* process additionally owns demand and creation edges that also carry     *)
(* the builder's name as waiter.                                           *)
DropWaits(E, p, t) == {e \in E : ~(e.own = p /\ e.w = t)}
DropWaitsBoth(E, p, t) == {e \in E : ~(e.own \in {p, t} /\ e.w = t)}

----------------------------------------------------------------------------
(* Activation. A claimed instance first observes the session mark (the     *)
(* double-checked runid skip), then enters checking.                       *)

ActivateSkip(p, t) ==
    /\ ist[p][t] = "new" /\ mark[t] # "-"
    /\ ist' = [ist EXCEPT ![p][t] = "done"]
    /\ UNCHANGED <<grade, cpend, mustR, apend, edges, lock, mark, clean,
                   builds>>

ActivateCheck(p, t) ==
    /\ ist[p][t] = "new" /\ mark[t] = "-"
    /\ ist' = [ist EXCEPT ![p][t] = "checking"]
    /\ cpend' = [cpend EXCEPT ![p][t] = Recorded[t]]
    /\ UNCHANGED <<grade, mustR, apend, edges, lock, mark, clean, builds>>

----------------------------------------------------------------------------
(* Checking: speculative fan-out over the RECORDED deps. Once any soft     *)
(* failure is noted (mustR), the checker stops processing deps and heads   *)
(* for the rebuild path — the same short-circuit the implementation takes. *)

(* Spawn a speculative instance for a recorded dep, guarded by the         *)
(* CREATION edge ctx(p) -> d (R1): the atomic check-and-insert that makes  *)
(* the eventual drain wait visible. A cycle refuses creation outright      *)
(* (R3 soft): speculation that could close a cycle never starts, and the   *)
(* checker takes the rebuild path. Top's speculative spawns need no        *)
(* creation edge: nothing can wait on top, so no cycle can pass through    *)
(* its drain.                                                              *)
CSpawn(p, t, d) ==
    /\ ist[p][t] = "checking" /\ ~mustR[p][t]
    /\ d \in cpend[p][t] /\ ist[p][d] = "none"
    /\ IF p # Top /\ Cyc(p, d)
         THEN /\ mustR' = [mustR EXCEPT ![p][t] = TRUE]
              /\ cpend' = [cpend EXCEPT ![p][t] = @ \ {d}]
              /\ UNCHANGED <<ist, grade, edges>>
         ELSE /\ ist' = [ist EXCEPT ![p][d] = "new"]
              /\ grade' = [grade EXCEPT ![p][d] = "spec"]
              /\ edges' = IF p = Top THEN edges
                          ELSE edges \cup
                            {[own |-> p, w |-> p, d |-> d, soft |-> TRUE]}
              /\ UNCHANGED <<cpend, mustR>>
    /\ UNCHANGED <<apend, lock, mark, clean, builds>>

(* Wait for an in-flight instance: atomic soft check-and-insert of the     *)
(* checker edge t -> d. A cycle is SOFT: skip the wait, rebuild.           *)
CWait(p, t, d) ==
    /\ ist[p][t] = "checking" /\ ~mustR[p][t]
    /\ d \in cpend[p][t] /\ ist[p][d] \in InFlight
    /\ [own |-> p, w |-> t, d |-> d, soft |-> TRUE] \notin edges
    /\ IF Cyc(t, d)
         THEN /\ mustR' = [mustR EXCEPT ![p][t] = TRUE]
              /\ cpend' = [cpend EXCEPT ![p][t] = @ \ {d}]
              /\ UNCHANGED edges
         ELSE /\ edges' = edges \cup
                  {[own |-> p, w |-> t, d |-> d, soft |-> TRUE]}
              /\ UNCHANGED <<cpend, mustR>>
    /\ UNCHANGED <<ist, grade, apend, lock, mark, clean, builds>>

(* A waited-on dep settled. done: this wait verified. sfail/fail: SOFT —   *)
(* the parent just cannot verify (R4: speculative outcomes are never       *)
(* adopted as errors by a checker).                                        *)
CObserve(p, t, d) ==
    /\ ist[p][t] = "checking" /\ d \in cpend[p][t]
    /\ ist[p][d] \in {"done", "sfail", "fail"}
    /\ cpend' = [cpend EXCEPT ![p][t] = @ \ {d}]
    /\ mustR' = IF ist[p][d] = "done" THEN mustR
                ELSE [mustR EXCEPT ![p][t] = TRUE]
    /\ UNCHANGED <<ist, grade, apend, edges, lock, mark, clean, builds>>

Verify(p, t) ==
    /\ ist[p][t] = "checking" /\ cpend[p][t] = {} /\ ~mustR[p][t]
    /\ t \in clean
    /\ ist' = [ist EXCEPT ![p][t] = "done"]
    /\ mark' = [mark EXCEPT ![t] = "verified"]
    /\ edges' = DropWaits(edges, p, t)
    /\ UNCHANGED <<grade, cpend, mustR, apend, lock, clean, builds>>

(* Take the rebuild path: on any soft failure (including an evicted        *)
(* checker edge's sticky mustR), or AT ANY TIME once the content verdict   *)
(* is dirty — the implementation knows "must rebuild" up front (missing    *)
(* record, changed do-file, redo-always) and abandons the rest of the      *)
(* checking immediately, racing ahead of its own speculation. The          *)
(* checker's wait edges leave the graph before the edge-free lock wait     *)
(* starts; the speculative instances it spawned keep settling on their     *)
(* own and are drained (under creation edges) by the enclosing process.    *)
MoveToBuild(p, t) ==
    /\ ist[p][t] = "checking"
    /\ mustR[p][t] \/ t \notin clean
    /\ ist' = [ist EXCEPT ![p][t] = "lockw"]
    /\ cpend' = [cpend EXCEPT ![p][t] = {}]
    /\ edges' = DropWaits(edges, p, t)
    /\ UNCHANGED <<grade, mustR, apend, lock, mark, clean, builds>>

----------------------------------------------------------------------------
(* The kernel build lock. Blocking here carries no edge; after acquiring,  *)
(* the session mark is re-checked (the double-checked skip that makes      *)
(* concurrent claims of one target safe).                                  *)

(* --- R6 lineage helpers ------------------------------------------------ *)

(* The builder process of process p's target: process p exists because    *)
(* some instance of target p is building (and holds p's lock).            *)
Builder(p) == lock[p]

(* Is every instance on the chain p, Builder(p), ... , top demanded and    *)
(* alive? Fuel bounds the recursion (lock chains are acyclic: a cyclic     *)
(* one would be an all-hard cycle, refused by BDemand).                    *)
RECURSIVE DemLin(_, _)
DemLin(p, n) ==
    IF p = Top THEN TRUE
    ELSE IF n = 0 THEN FALSE
    ELSE LET q == Builder(p)
         IN /\ q # "-"
            /\ ist[q][p] \in {"prompt", "brun", "bdrain"}
            /\ grade[q][p] = "dem"
            /\ DemLin(q, n - 1)
DemLineage(p) == DemLin(p, Cardinality(Procs))

(* The nearest process up the chain that is speculative (its instance in  *)
(* its builder's registry is spec-graded) or orphaned (its builder is      *)
(* gone). Only meaningful when ~DemLineage(p).                             *)
RECURSIVE AbortRootOf(_, _)
AbortRootOf(p, n) ==
    IF n = 0 \/ Builder(p) = "-" \/ grade[Builder(p)][p] = "spec" THEN p
    ELSE AbortRootOf(Builder(p), n - 1)
AbortRoot(p) == AbortRootOf(p, Cardinality(Procs))

(* Every process whose builder chain passes through r (r included).       *)
RECURSIVE Subtree(_)
Subtree(S) ==
    LET nxt == S \cup {q \in Targets : Builder(q) \in S}
    IN IF nxt = S THEN S ELSE Subtree(nxt)

(* Kill the process subtree under r: the R5 kill (the do-file dies, and    *)
(* the abort watch reaches every redo process below it). Every in-flight  *)
(* instance in those registries settles sfail, every lock they hold is    *)
(* released (Uncommitted markers keep those targets out of date), and     *)
(* every edge they own leaves the graph -- as does r's own instance in    *)
(* its builder's registry, if any, with its waits and creation edge.      *)
KillSubtree(r) ==
    LET P == Subtree({r})
        q == Builder(r)
        Dead(k, x) == k \in P /\ ist[k][x] \in InFlight
    IN /\ ist' = [k \in Procs |-> [x \in Targets |->
                    IF Dead(k, x) \/ (k = q /\ x = r /\ q # "-")
                      THEN "sfail" ELSE ist[k][x]]]
       \* locks HELD BY the dead processes, and the lock ON the root that
       \* its (surviving) builder holds for it
       /\ lock' = [x \in Targets |->
                    IF lock[x] \in P \/ x = r THEN "-" ELSE lock[x]]
       /\ apend' = [x \in Targets |->
                     IF lock[x] \in P \/ x = r THEN {} ELSE apend[x]]
       /\ cpend' = [k \in Procs |-> [x \in Targets |->
                      IF k \in P \/ (k = q /\ x = r) THEN {} ELSE cpend[k][x]]]
       /\ edges' = {e \in edges :
                      /\ e.own \notin P
                      /\ ~(e.own = q /\ e.w = r)
                      /\ ~(e.own = q /\ e.w = q /\ e.d = r /\ e.soft)}

----------------------------------------------------------------------------
(* The kernel build lock. Blocking here carries no edge; after acquiring,  *)
(* the session mark is re-checked (the double-checked skip that makes      *)
(* concurrent claims of one target safe). Then the overwrite guard (R6):   *)
(* a hand-edited target in a demanded lineage is asked about; anywhere     *)
(* else it aborts the lineage instead of building.                         *)

AcquireLock(p, t) ==
    /\ ist[p][t] = "lockw" /\ lock[t] = "-"
    /\ IF mark[t] # "-"
         THEN /\ ist' = [ist EXCEPT ![p][t] = "done"]
              /\ UNCHANGED <<apend, lock, cpend, edges>>
         ELSE IF t \notin HandEdited
         THEN /\ ist' = [ist EXCEPT ![p][t] = "brun"]
              /\ lock' = [lock EXCEPT ![t] = p]
              /\ apend' = [apend EXCEPT ![t] = Actual[t]]
              /\ UNCHANGED <<cpend, edges>>
         ELSE IF DemLineage(p) /\ grade[p][t] = "dem"
         THEN /\ ist' = [ist EXCEPT ![p][t] = "prompt"]
              /\ lock' = [lock EXCEPT ![t] = p]
              /\ UNCHANGED <<apend, cpend, edges>>
         ELSE IF grade[p][t] = "spec"
         THEN \* a speculative instance simply yields: never asks
              /\ ist' = [ist EXCEPT ![p][t] = "sfail"]
              /\ edges' = DropWaits(edges, p, t)
              /\ UNCHANGED <<apend, lock, cpend>>
         ELSE \* demanded, but inside a speculative or orphaned lineage
              KillSubtree(AbortRoot(p))
    /\ UNCHANGED <<grade, mustR, mark, clean, builds>>

(* The human answers the overwrite question. *)
PromptYes(p, t) ==
    /\ "yes" \in PromptAnswers
    /\ ist[p][t] = "prompt" /\ lock[t] = p
    /\ ist' = [ist EXCEPT ![p][t] = "brun"]
    /\ apend' = [apend EXCEPT ![t] = Actual[t]]
    /\ UNCHANGED <<grade, cpend, mustR, edges, lock, mark, clean, builds>>

PromptNo(p, t) ==
    /\ "no" \in PromptAnswers
    /\ ist[p][t] = "prompt" /\ lock[t] = p
    /\ ist' = [ist EXCEPT ![p][t] = "fail"]
    /\ lock' = [lock EXCEPT ![t] = "-"]
    /\ edges' = DropWaitsBoth(edges, p, t)
    /\ UNCHANGED <<grade, cpend, mustR, apend, mark, clean, builds>>

----------------------------------------------------------------------------
(* Building: the do-file runs and demands its ACTUAL deps through its      *)
(* redo-ifchange child process (proc t). Every demand is a HARD edge.      *)

(* A SPECULATIVE builder never reports failure: it settles "sfail",        *)
(* reclaimable by a real demand (R4).                                      *)
FailState(p, t) == IF grade[p][t] = "dem" THEN "fail" ELSE "sfail"

(* The do-file demands d: atomic hard check-and-insert (R3). An all-hard   *)
(* cycle is a REAL dependency cycle: this build fails. A cycle riding      *)
(* soft edges blocks the demand until eviction resolves it (below). No     *)
(* cycle: the edge goes in, superseding any creation edge for the same     *)
(* dep, and the dep instance is created demanded — or an existing          *)
(* speculative one is upgraded (un-abortable from here on), or a           *)
(* quarantined sfail is reclaimed and re-run.                              *)
BDemand(p, t, d) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ d \in apend[t]
    /\ [own |-> t, w |-> t, d |-> d, soft |-> FALSE] \notin edges
    /\ IF CycHard(t, d)
         THEN /\ ist' = [ist EXCEPT ![p][t] = FailState(p, t)]
              /\ lock' = [lock EXCEPT ![t] = "-"]
              /\ apend' = [apend EXCEPT ![t] = {}]
              /\ edges' = DropWaitsBoth(edges, p, t)
              /\ UNCHANGED grade
         ELSE /\ ~Cyc(t, d)
              /\ edges' =
                   (edges \ {[own |-> t, w |-> t, d |-> d, soft |-> TRUE]})
                   \cup {[own |-> t, w |-> t, d |-> d, soft |-> FALSE]}
              /\ IF ist[t][d] \in {"none", "sfail"}
                   THEN /\ ist' = [ist EXCEPT ![t][d] = "new"]
                        /\ grade' = [grade EXCEPT ![t][d] = "dem"]
                   ELSE /\ grade' = [grade EXCEPT ![t][d] = "dem"]
                        /\ UNCHANGED ist
              /\ UNCHANGED <<lock, apend>>
    /\ UNCHANGED <<cpend, mustR, mark, clean, builds>>

(* R3 eviction: the hard demand t -> d found a cycle, but every d ~> t     *)
(* path rides at least one SOFT edge. The speculation yields: one soft     *)
(* edge on such a path is evicted, then the demand retries. A checker      *)
(* edge evicts to a sticky mustR (that checker rebuilds — wasted work,     *)
(* never an error).                                                        *)
EvictChecker(p, t, d, e) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ d \in apend[t]
    /\ Cyc(t, d) /\ ~CycHard(t, d)
    /\ e \in SoftOnPath(t, d) /\ ~IsCreation(e)
    /\ edges' = edges \ {e}
    /\ mustR' = [mustR EXCEPT ![e.own][e.w] = TRUE]
    /\ UNCHANGED <<ist, grade, cpend, apend, lock, mark, clean, builds>>

(* A creation edge evicts by ABORTING the speculative instance it guards:  *)
(* not yet building -> settles sfail directly; mid-build -> the do-file    *)
(* run is abandoned (its pending ifchange fails, the implementation-side   *)
(* Uncommitted marker stays), the lock releases, and speculative           *)
(* sub-instances it spawned settle on their own. Nothing is reported       *)
(* (R4). A stale creation edge to an already-settled instance is simply    *)
(* dropped. Upgraded (demanded) instances are never aborted — their        *)
(* creation edge was superseded at upgrade.                                *)
EvictCreation(p, t, d, e) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ d \in apend[t]
    /\ Cyc(t, d) /\ ~CycHard(t, d)
    /\ e \in SoftOnPath(t, d) /\ IsCreation(e)
    /\ LET q == e.own
           s == e.d
       IN IF ist[q][s] \in InFlight /\ grade[q][s] = "spec"
            THEN /\ ist' = [ist EXCEPT ![q][s] = "sfail"]
                 /\ lock' = IF lock[s] = q
                              THEN [lock EXCEPT ![s] = "-"] ELSE lock
                 /\ apend' = IF lock[s] = q
                               THEN [apend EXCEPT ![s] = {}] ELSE apend
                 /\ cpend' = [cpend EXCEPT ![q][s] = {}]
                 /\ edges' = DropWaitsBoth(edges \ {e}, q, s)
            ELSE /\ edges' = edges \ {e}
                 /\ UNCHANGED <<ist, lock, apend, cpend>>
    /\ UNCHANGED <<grade, mustR, mark, clean, builds>>

(* A demanded dep settled: done resolves it; sfail is reclaimed and        *)
(* re-run; fail is HARD — the demanding build fails.                       *)
BObserveDone(p, t, d) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ d \in apend[t]
    /\ [own |-> t, w |-> t, d |-> d, soft |-> FALSE] \in edges
    /\ ist[t][d] = "done"
    /\ apend' = [apend EXCEPT ![t] = @ \ {d}]
    /\ UNCHANGED <<ist, grade, cpend, mustR, edges, lock, mark, clean,
                   builds>>

BObserveFail(p, t, d) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ d \in apend[t]
    /\ ist[t][d] = "fail"
    /\ ist' = [ist EXCEPT ![p][t] = FailState(p, t)]
    /\ lock' = [lock EXCEPT ![t] = "-"]
    /\ apend' = [apend EXCEPT ![t] = {}]
    /\ edges' = DropWaitsBoth(edges, p, t)
    /\ UNCHANGED <<grade, cpend, mustR, mark, clean, builds>>

BReclaim(p, t, d) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ d \in apend[t]
    /\ [own |-> t, w |-> t, d |-> d, soft |-> FALSE] \in edges
    /\ ist[t][d] = "sfail"
    /\ ist' = [ist EXCEPT ![t][d] = "new"]
    /\ grade' = [grade EXCEPT ![t][d] = "dem"]
    /\ UNCHANGED <<cpend, mustR, apend, edges, lock, mark, clean, builds>>

FinishDemands(p, t) ==
    /\ ist[p][t] = "brun" /\ lock[t] = p /\ apend[t] = {}
    /\ ist' = [ist EXCEPT ![p][t] = "bdrain"]
    /\ UNCHANGED <<grade, cpend, mustR, apend, edges, lock, mark, clean,
                   builds>>

(* R5 — drain cancellation. A draining builder need not WAIT for its       *)
(* speculative instances: it may abandon any of them, in any in-flight     *)
(* state (mid-build included — the implementation kills the speculative    *)
(* do-file, which is crash-safe by the same argument as LockSession's      *)
(* crashes: Uncommitted marker, kernel-released locks, temp GC). An        *)
(* abandoned instance settles "sfail": quarantined and reclaimed by a      *)
(* real demand exactly like any other speculative failure (R4).           *)
(* Demanded instances and upgraded (formerly speculative) instances are    *)
(* never abandoned. This is what bounds an ifchange's return latency:      *)
(* speculation that nobody demanded by drain time cannot hold the caller   *)
(* hostage.                                                                *)
AbandonSpec(p, t, s) ==
    /\ ist[p][t] = "bdrain" /\ lock[t] = p
    /\ ist[t][s] \in InFlight /\ grade[t][s] = "spec"
    /\ ist' = [ist EXCEPT ![t][s] = "sfail"]
    /\ lock' = IF lock[s] = t THEN [lock EXCEPT ![s] = "-"] ELSE lock
    /\ apend' = IF lock[s] = t THEN [apend EXCEPT ![s] = {}] ELSE apend
    /\ cpend' = [cpend EXCEPT ![t][s] = {}]
    /\ edges' = {e \in DropWaitsBoth(edges, t, s) :
                   ~(e.own = t /\ e.w = t /\ e.d = s)}
    /\ UNCHANGED <<grade, mustR, mark, clean, builds>>

(* Drain-then-commit: the child process waits for every instance in ITS    *)
(* registry to settle — waits visible through the creation and demand      *)
(* edges (R1) — then the target commits. Speculative failures among the    *)
(* drained instances are quarantined, not adopted.                         *)
Commit(p, t) ==
    /\ ist[p][t] = "bdrain" /\ lock[t] = p
    /\ \A x \in Targets : ist[t][x] \in SettledSt
    /\ ist' = [ist EXCEPT ![p][t] = "done"]
    /\ mark' = [mark EXCEPT ![t] = "committed"]
    /\ builds' = [builds EXCEPT ![t] = @ + 1]
    /\ lock' = [lock EXCEPT ![t] = "-"]
    /\ edges' = DropWaitsBoth(edges, p, t)
    /\ UNCHANGED <<grade, cpend, mustR, apend, clean>>

(* The do-file itself fails (nonzero exit).                                *)
BuildFail(p, t) ==
    /\ BuildsCanFail
    /\ ist[p][t] = "brun" /\ lock[t] = p
    /\ ist' = [ist EXCEPT ![p][t] = FailState(p, t)]
    /\ lock' = [lock EXCEPT ![t] = "-"]
    /\ apend' = [apend EXCEPT ![t] = {}]
    /\ edges' = DropWaitsBoth(edges, p, t)
    /\ UNCHANGED <<grade, cpend, mustR, mark, clean, builds>>

----------------------------------------------------------------------------
PStep(p, t) ==
    \/ ActivateSkip(p, t) \/ ActivateCheck(p, t)
    \/ \E d \in Recorded[t] :
         CSpawn(p, t, d) \/ CWait(p, t, d) \/ CObserve(p, t, d)
    \/ Verify(p, t) \/ MoveToBuild(p, t) \/ AcquireLock(p, t)
    \/ PromptYes(p, t) \/ PromptNo(p, t)
    \/ \E d \in Actual[t] :
         \/ BDemand(p, t, d) \/ BObserveDone(p, t, d)
         \/ BObserveFail(p, t, d) \/ BReclaim(p, t, d)
         \/ \E e \in edges :
              EvictChecker(p, t, d, e) \/ EvictCreation(p, t, d, e)
    \/ \E s \in Targets : AbandonSpec(p, t, s)
    \/ FinishDemands(p, t) \/ Commit(p, t) \/ BuildFail(p, t)

Quiescent == \A p \in Procs, t \in Targets : ist[p][t] \in SettledSt

Terminating == Quiescent /\ UNCHANGED vars

Next == (\E p \in Procs, t \in Targets : PStep(p, t)) \/ Terminating

Spec == Init /\ [][Next]_vars
        /\ (\A p \in Procs, t \in Targets : WF_vars(PStep(p, t)))

----------------------------------------------------------------------------
(* Invariants and properties.                                              *)

(* On configs with an acyclic Actual graph and BuildsCanFail = FALSE:      *)
(* stale recorded state may waste work (sfail, needless rebuilds) but      *)
(* never invents a reported error.                                         *)
NoFailure == \A p \in Procs, t \in Targets : ist[p][t] # "fail"

(* A committed target saw every actual dep settled (verified or built)     *)
(* first — speculation can never corrupt build order.                      *)
ActualDepsFirst ==
    \A t \in Targets :
        mark[t] = "committed" => \A d \in Actual[t] : mark[d] # "-"

CommitOnce == \A t \in Targets : builds[t] <= 1

LockConsistent ==
    \A t \in Targets :
        lock[t] # "-" => ist[lock[t]][t] \in {"prompt", "brun", "bdrain"}

(* R6: a prompt is only ever entered by a demanded instance in a demanded  *)
(* lineage -- enforced at entry by AcquireLock's guard. It is not a state  *)
(* invariant that the lineage STAYS alive: a sibling demand's hard failure *)
(* (PromptNo on d while c is prompting, in HandEditNo) fails the builder   *)
(* above while the question is still open. The implementation's drain-    *)
(* before-return keeps that child process alive until c settles, so the   *)
(* open question is still answered -- stale, but harmless: yes builds c   *)
(* for next time, no fails an instance nobody reads. The configs whose    *)
(* hand-edited target is never really demanded check NeverPrompted, which *)
(* is the property with teeth.                                             *)
PromptOnlyDemanded ==
    \A p \in Procs, t \in Targets :
        ist[p][t] = "prompt" => grade[p][t] = "dem"

(* For configs where the hand-edited target is never really demanded. *)
NeverPrompted == \A p \in Procs, t \in Targets : ist[p][t] # "prompt"

EventuallyQuiescent == <>Quiescent

AllRootsDone == <>(\A r \in Roots : ist[Top][r] = "done")

AllRootsFail == <>(\A r \in Roots : ist[Top][r] = "fail")

============================================================================

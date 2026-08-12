----------------------------- MODULE Speculation -----------------------------
(***************************************************************************)
(* The full aggressive design, with the honesty ParallelEnsure lacks:      *)
(* RECORDED deps (last run's database edges, used for speculative          *)
(* parallel checking) are distinct from ACTUAL deps (what the do-file      *)
(* declares via redo-ifchange when it really runs). Recorded edges can be  *)
(* stale in every way: dropped deps, newly discovered deps, even a         *)
(* recorded cycle that no longer exists -- and a fresh database has no     *)
(* recorded edges at all, so cycle detection must also work for deps       *)
(* discovered mid-build.                                                   *)
(*                                                                         *)
(* Per-target lifecycle (claimed at most once per session):                *)
(*   unreq -> checking -> verified                (up to date)             *)
(*                     -> brun <-> bwait -> built (do-file ran)            *)
(*                     -> failed                                           *)
(*                                                                         *)
(*   checking - speculative fan-out over RECORDED deps: activate each,     *)
(*              wait for it (a live edge in the shared waits-for graph).   *)
(*   brun     - do-file executing; HOLDS a token.                          *)
(*   bwait    - do-file blocked in redo-ifchange on ACTUAL deps; holds NO  *)
(*              token. This is the flat-budget rendering of the own-token  *)
(*              mechanism: TokenPool's verified Bound (executing bodies    *)
(*              <= J, blocked ancestors compensated by children's own      *)
(*              tokens) justifies modeling "blocked builder frees a slot". *)
(*                                                                         *)
(* FAILURE SEVERITY -- the design rule this spec exists to check:          *)
(*   - SPECULATIVE failures are SOFT. A recorded dep that fails, or a      *)
(*     cycle among recorded wait edges, only means the parent cannot be    *)
(*     verified up to date: it sets mustRebuild and the parent proceeds    *)
(*     to run its do-file, whose redo-ifchange calls are the ground        *)
(*     truth. Stale edges can therefore waste work but never invent an     *)
(*     error: a stale recorded cycle SELF-HEALS (Speculation_StaleCycle    *)
(*     proves every interleaving ends all-built, none failed).             *)
(*   - ACTUAL failures are HARD. A dep that a running do-file ifchanges    *)
(*     failing, or a cycle closed by a mid-build wait edge, fails the      *)
(*     target, and the error propagates up the wait edges releasing        *)
(*     tokens on the way (Speculation_TrueCycle proves a genuinely         *)
(*     cyclic project errors out on every interleaving -- with an empty    *)
(*     database, i.e. purely discovery-time detection).                    *)
(*                                                                         *)
(* Wait edges from BUILDING targets participate in reachability exactly   *)
(* like checking-phase edges: one shared graph, one atomic                 *)
(* check-and-insert per edge (one SQLite write transaction in the          *)
(* implementation).                                                        *)
(*                                                                         *)
(* Out-of-date evaluation: `clean` is a nondeterministically chosen        *)
(* subset of Cleanable at Init (TLC explores every subset); only clean     *)
(* targets with no soft failure may verify. Do-file failure is a           *)
(* nondeterministic build outcome (BuildsCanFail).                         *)
(***************************************************************************)
EXTENDS Naturals, FiniteSets, TLC

CONSTANTS Targets, Recorded, Actual, Roots, J, Cleanable, BuildsCanFail

ASSUME /\ Roots \subseteq Targets
       /\ Cleanable \subseteq Targets
       /\ J \in Nat \ {0}
       /\ BuildsCanFail \in BOOLEAN

\* --- instances, selected from the .cfg files via <- ---------------------
DiamondTargets == {"a", "b", "c", "d"}
DiamondDeps == ("a" :> {"b", "c"}) @@ ("b" :> {"d"})
               @@ ("c" :> {"d"}) @@ ("d" :> {})
\* Stale: recorded says a needs {b,c}; the do-file now declares {c,d}:
\* b was dropped, c was kept, d is newly discovered mid-build.
StaleTargets == {"a", "b", "c", "d"}
StaleRecorded == ("a" :> {"b", "c"}) @@ ("b" :> {}) @@ ("c" :> {}) @@ ("d" :> {})
StaleActual == ("a" :> {"c", "d"}) @@ ("b" :> {}) @@ ("c" :> {}) @@ ("d" :> {})
TwoTargets == {"a", "b"}
CycleEdges == ("a" :> {"b"}) @@ ("b" :> {"a"})
NoEdges2 == ("a" :> {}) @@ ("b" :> {})
RootsA == {"a"}
RootsAB == {"a", "b"}
NoTargets == {}

VARIABLES
    st,          \* [Targets -> lifecycle state]
    cpending,    \* recorded deps not yet processed while checking
    cedges,      \* live speculative wait edges (checking phase)
    bpending,    \* actual deps not yet processed while building
    bedges,      \* live wait edges from the running do-file's ifchange
    mustRebuild, \* soft failure noted: verify is off the table
    clean,       \* targets whose content would verify up to date (frozen)
    tokens,      \* free do-file tokens
    builds       \* commit counts (history, for invariants)

vars == <<st, cpending, cedges, bpending, bedges, mustRebuild, clean,
          tokens, builds>>

States == {"unreq", "checking", "brun", "bwait", "built", "verified", "failed"}
DoneSt == {"built", "verified"}
InFlight(d) == st[d] \in {"checking", "brun", "bwait"}

TypeOK ==
    /\ st \in [Targets -> States]
    /\ \A t \in Targets : /\ cpending[t] \subseteq Recorded[t]
                          /\ cedges[t] \subseteq Recorded[t]
                          /\ bpending[t] \subseteq Actual[t]
                          /\ bedges[t] \subseteq Actual[t]
    /\ mustRebuild \in [Targets -> BOOLEAN]
    /\ clean \subseteq Cleanable
    /\ tokens \in 0..J
    /\ builds \in [Targets -> Nat]

Init ==
    /\ st = [t \in Targets |-> IF t \in Roots THEN "checking" ELSE "unreq"]
    /\ cpending = [t \in Targets |-> IF t \in Roots THEN Recorded[t] ELSE {}]
    /\ cedges = [t \in Targets |-> {}]
    /\ bpending = [t \in Targets |-> {}]
    /\ bedges = [t \in Targets |-> {}]
    /\ mustRebuild = [t \in Targets |-> FALSE]
    /\ clean \in SUBSET Cleanable
    /\ tokens = J
    /\ builds = [t \in Targets |-> 0]

(* The shared waits-for graph: speculative and mid-build edges together. *)
E(x) == cedges[x] \cup bedges[x]

RECURSIVE Closure(_)
Closure(S) ==
    LET nxt == S \cup UNION {E(x) : x \in S}
    IN IF nxt = S THEN S ELSE Closure(nxt)

CanReach(x, y) == y \in Closure({x})

----------------------------------------------------------------------------
(* Checking phase: speculative fan-out over the RECORDED deps.            *)

CSkipDone(t, d) ==
    /\ st[t] = "checking" /\ d \in cpending[t] /\ st[d] \in DoneSt
    /\ cpending' = [cpending EXCEPT ![t] = @ \ {d}]
    /\ UNCHANGED <<st, cedges, bpending, bedges, mustRebuild, clean, tokens,
                   builds>>

(* Recorded dep failed: SOFT -- the parent just cannot verify. *)
CSkipFailed(t, d) ==
    /\ st[t] = "checking" /\ d \in cpending[t] /\ st[d] = "failed"
    /\ cpending' = [cpending EXCEPT ![t] = @ \ {d}]
    /\ mustRebuild' = [mustRebuild EXCEPT ![t] = TRUE]
    /\ UNCHANGED <<st, cedges, bpending, bedges, clean, tokens, builds>>

CFresh(t, d) ==
    /\ st[t] = "checking" /\ d \in cpending[t] /\ st[d] = "unreq"
    /\ st' = [st EXCEPT ![d] = "checking"]
    /\ cpending' = [cpending EXCEPT ![t] = @ \ {d}, ![d] = Recorded[d]]
    /\ cedges' = [cedges EXCEPT ![t] = @ \cup {d}]
    /\ UNCHANGED <<bpending, bedges, mustRebuild, clean, tokens, builds>>

(* Wait for an in-flight recorded dep -- unless waiting would close a
   cycle, which is SOFT here: skip the wait, force the rebuild path (the
   do-file is the ground truth for whether the cycle is real). The check
   and the insert are one atomic action. *)
CWait(t, d) ==
    /\ st[t] = "checking" /\ d \in cpending[t] /\ InFlight(d)
    /\ IF CanReach(d, t)
         THEN /\ cpending' = [cpending EXCEPT ![t] = @ \ {d}]
              /\ mustRebuild' = [mustRebuild EXCEPT ![t] = TRUE]
              /\ UNCHANGED cedges
         ELSE /\ cpending' = [cpending EXCEPT ![t] = @ \ {d}]
              /\ cedges' = [cedges EXCEPT ![t] = @ \cup {d}]
              /\ UNCHANGED mustRebuild
    /\ UNCHANGED <<st, bpending, bedges, clean, tokens, builds>>

(* A dep we were waiting on failed after the edge went in: SOFT. *)
CDropFailed(t, d) ==
    /\ st[t] = "checking" /\ d \in cedges[t] /\ st[d] = "failed"
    /\ cedges' = [cedges EXCEPT ![t] = @ \ {d}]
    /\ mustRebuild' = [mustRebuild EXCEPT ![t] = TRUE]
    /\ UNCHANGED <<st, cpending, bpending, bedges, clean, tokens, builds>>

CheckSettled(t) ==
    /\ cpending[t] = {}
    /\ \A d \in cedges[t] : st[d] \in DoneSt

(* Up to date: only if every recorded dep settled ok, nothing soft-failed,
   and the content verdict is clean. *)
Verify(t) ==
    /\ st[t] = "checking" /\ CheckSettled(t) /\ ~mustRebuild[t] /\ t \in clean
    /\ st' = [st EXCEPT ![t] = "verified"]
    /\ cedges' = [cedges EXCEPT ![t] = {}]
    /\ UNCHANGED <<cpending, bpending, bedges, mustRebuild, clean, tokens,
                   builds>>

(* Run the do-file. mustRebuild lets the build start WITHOUT waiting for
   the rest of the speculation (abandoning it) -- aggressive, and safe
   because the do-file re-declares its deps itself. *)
BeginBuild(t) ==
    /\ st[t] = "checking"
    /\ CheckSettled(t) \/ mustRebuild[t]
    /\ tokens > 0
    /\ tokens' = tokens - 1
    /\ st' = [st EXCEPT ![t] = "brun"]
    /\ cpending' = [cpending EXCEPT ![t] = {}]
    /\ cedges' = [cedges EXCEPT ![t] = {}]
    /\ bpending' = [bpending EXCEPT ![t] = Actual[t]]
    /\ UNCHANGED <<bedges, mustRebuild, clean, builds>>

----------------------------------------------------------------------------
(* Build phase: the do-file runs and redo-ifchanges its ACTUAL deps.      *)

BSkipDone(t, d) ==
    /\ st[t] = "brun" /\ d \in bpending[t] /\ st[d] \in DoneSt
    /\ bpending' = [bpending EXCEPT ![t] = @ \ {d}]
    /\ UNCHANGED <<st, cpending, cedges, bedges, mustRebuild, clean, tokens,
                   builds>>

(* An actual dep failed: HARD -- the build fails, token comes back. *)
BFailDep(t, d) ==
    /\ st[t] = "brun" /\ d \in bpending[t] /\ st[d] = "failed"
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ bpending' = [bpending EXCEPT ![t] = {}]
    /\ bedges' = [bedges EXCEPT ![t] = {}]
    /\ tokens' = tokens + 1
    /\ UNCHANGED <<cpending, cedges, mustRebuild, clean, builds>>

(* Discovery: the do-file demands a target nobody has touched. *)
BFresh(t, d) ==
    /\ st[t] = "brun" /\ d \in bpending[t] /\ st[d] = "unreq"
    /\ st' = [st EXCEPT ![d] = "checking"]
    /\ cpending' = [cpending EXCEPT ![d] = Recorded[d]]
    /\ bpending' = [bpending EXCEPT ![t] = @ \ {d}]
    /\ bedges' = [bedges EXCEPT ![t] = @ \cup {d}]
    /\ UNCHANGED <<cedges, mustRebuild, clean, tokens, builds>>

(* Wait on an in-flight actual dep; a cycle here is a REAL dependency
   cycle: HARD error. Same atomic check-and-insert. *)
BWait(t, d) ==
    /\ st[t] = "brun" /\ d \in bpending[t] /\ InFlight(d)
    /\ IF CanReach(d, t)
         THEN /\ st' = [st EXCEPT ![t] = "failed"]
              /\ bpending' = [bpending EXCEPT ![t] = {}]
              /\ bedges' = [bedges EXCEPT ![t] = {}]
              /\ tokens' = tokens + 1
         ELSE /\ bpending' = [bpending EXCEPT ![t] = @ \ {d}]
              /\ bedges' = [bedges EXCEPT ![t] = @ \cup {d}]
              /\ UNCHANGED <<st, tokens>>
    /\ UNCHANGED <<cpending, cedges, mustRebuild, clean, builds>>

(* Blocked in redo-ifchange: the do-file stops executing and its token
   frees a slot (justified by TokenPool's Bound: children's own tokens
   compensate for blocked ancestors). *)
Yield(t) ==
    /\ st[t] = "brun"
    /\ \E d \in bedges[t] : st[d] \notin DoneSt
    /\ st' = [st EXCEPT ![t] = "bwait"]
    /\ tokens' = tokens + 1
    /\ UNCHANGED <<cpending, cedges, bpending, bedges, mustRebuild, clean,
                   builds>>

(* redo-ifchange returned: the do-file resumes executing under a token. *)
Resume(t) ==
    /\ st[t] = "bwait"
    /\ \A d \in bedges[t] : st[d] \in DoneSt
    /\ tokens > 0
    /\ tokens' = tokens - 1
    /\ st' = [st EXCEPT ![t] = "brun"]
    /\ UNCHANGED <<cpending, cedges, bpending, bedges, mustRebuild, clean,
                   builds>>

(* A waited-on actual dep failed: HARD, from either build state. *)
BObserveFailRun(t) ==
    /\ st[t] = "brun"
    /\ \E d \in bedges[t] : st[d] = "failed"
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ bpending' = [bpending EXCEPT ![t] = {}]
    /\ bedges' = [bedges EXCEPT ![t] = {}]
    /\ tokens' = tokens + 1
    /\ UNCHANGED <<cpending, cedges, mustRebuild, clean, builds>>

BObserveFailWait(t) ==
    /\ st[t] = "bwait"
    /\ \E d \in bedges[t] : st[d] = "failed"
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ bpending' = [bpending EXCEPT ![t] = {}]
    /\ bedges' = [bedges EXCEPT ![t] = {}]
    /\ UNCHANGED <<cpending, cedges, mustRebuild, clean, tokens, builds>>

Commit(t) ==
    /\ st[t] = "brun"
    /\ bpending[t] = {}
    /\ \A d \in bedges[t] : st[d] \in DoneSt
    /\ st' = [st EXCEPT ![t] = "built"]
    /\ builds' = [builds EXCEPT ![t] = @ + 1]
    /\ bedges' = [bedges EXCEPT ![t] = {}]
    /\ tokens' = tokens + 1
    /\ UNCHANGED <<cpending, cedges, bpending, mustRebuild, clean>>

(* The do-file itself fails: token still comes back. *)
BuildFail(t) ==
    /\ BuildsCanFail
    /\ st[t] = "brun"
    /\ st' = [st EXCEPT ![t] = "failed"]
    /\ bpending' = [bpending EXCEPT ![t] = {}]
    /\ bedges' = [bedges EXCEPT ![t] = {}]
    /\ tokens' = tokens + 1
    /\ UNCHANGED <<cpending, cedges, mustRebuild, clean, builds>>

----------------------------------------------------------------------------
TStep(t) ==
    \/ \E d \in Recorded[t] :
         CSkipDone(t, d) \/ CSkipFailed(t, d) \/ CFresh(t, d)
         \/ CWait(t, d) \/ CDropFailed(t, d)
    \/ \E d \in Actual[t] :
         BSkipDone(t, d) \/ BFailDep(t, d) \/ BFresh(t, d) \/ BWait(t, d)
    \/ Verify(t) \/ BeginBuild(t)
    \/ Yield(t) \/ Resume(t)
    \/ BObserveFailRun(t) \/ BObserveFailWait(t)
    \/ Commit(t) \/ BuildFail(t)

Quiescent == \A t \in Targets : st[t] \in {"unreq", "built", "verified", "failed"}

Terminating == Quiescent /\ UNCHANGED vars

Next == (\E t \in Targets : TStep(t)) \/ Terminating

Spec == Init /\ [][Next]_vars /\ (\A t \in Targets : WF_vars(TStep(t)))

----------------------------------------------------------------------------
BuildOnce == \A t \in Targets : builds[t] <= 1

(* Executing do-files and free tokens account for exactly J; bwait
   (blocked in ifchange) holds no slot. *)
TokenBound == Cardinality({t \in Targets : st[t] = "brun"}) = J - tokens

(* A committed build saw every ACTUAL dep settled ok first -- speculation
   over stale recorded edges can never corrupt build order. *)
ActualDepsFirst ==
    \A t \in Targets :
        st[t] = "built" => \A d \in Actual[t] : st[d] \in DoneSt

(* A verified target really was clean and saw every RECORDED dep settle
   ok -- verification never rides over a soft failure. *)
VerifiedSoundly ==
    \A t \in Targets :
        st[t] = "verified" =>
            /\ t \in clean
            /\ \A d \in Recorded[t] : st[d] \in DoneSt

EventuallyQuiescent == <>Quiescent

Settles ==
    \A t \in Targets :
        (st[t] \in {"checking", "brun", "bwait"})
            ~> (st[t] \in {"built", "verified", "failed"})

(* Stale recorded cycle, acyclic reality: EVERY interleaving self-heals
   to all-built/verified with no failure. *)
AllDone == <>(\A t \in Targets : st[t] \in DoneSt)

(* Real dependency cycle: EVERY interleaving reports it as an error. *)
AllFail == <>(\A t \in Targets : st[t] = "failed")

============================================================================

# Formal verification of redo-msh's concurrency design

TLA+ specifications of the concurrency protocols in redo-msh, checked with
TLC. The specs verify the **design** (the protocol under all interleavings),
not the Rust code — the code must faithfully implement what is modeled here,
and the "abstractions and assumptions" section lists exactly what is taken on
faith.

## Running

```
./run.sh
```

Requires Java 11+ and `tla2tools.jar` at `~/.local/share/java/tla2tools.jar`
(override with `TLA_TOOLS_JAR`). TLC scratch state and full logs go to
`/tmp/redo-msh-tlc/` (kept off the drvfs mount, which is slow for TLC's many
small state files). Every check finishes in seconds.

Each spec is a `.tla` module plus one or more `.cfg` model configurations.
One configuration — `CycleLock_CycleParallel` — documents a **known gap** and
passes when TLC finds the predicted deadlock.

## The specs

### TokenPool.tla — the jobserver token protocol

Models `src/jobserver.rs` + `run_parallel` in `src/build.rs`: J total tokens,
one implicit "own" token per process, a shared pool of J−1, non-blocking
try-acquire, two levels of nesting (top-level jobs whose do-files block on a
child `redo-ifchange` that runs sub jobs).

Verified (J=2/NTop=2/NSub=2 and J=3/NTop=3/NSub=2; the larger config explores
33,868 distinct states):

- **TokenConservation** — pool tokens are never lost or duplicated on any
  interleaving.
- **Bound** — at most **J do-file bodies execute simultaneously**. This is
  the precise concurrency claim: more than J tokens *exist* across a nested
  tree (each child process brings a fresh own token while its blocked
  ancestor's token idles), but the executing bodies never exceed J. The naive
  invariant "at most J tokens exist" is false by design; TLC confirms the
  compensation is exact, at least to nesting depth 2.
- **Termination** — the try-acquire discipline is deadlock-free; every job
  completes.

### LockSession.tla — per-target build exclusion, with crashes

Models one target, one session, three racing processes running
`ensure` → `build` → `build_inner`: the unlocked runid check, the blocking
per-target file lock, the double-checked runid re-read under the lock, the
atomic clear-deps + `Uncommitted`-marker transaction, the atomic commit. A
crash action can kill any process at any point (the OS releases its file
lock; the database keeps what was committed).

Verified:

- **Mutex** — never two processes past lock acquisition for the same target.
- **AtMostOnce** — the target commits at most once per session; the
  double-check correctly turns lock-waiters into skippers.
- **NoDirtyCommit / MarkerDiscipline / RunidCleanMarker** — a build in
  flight always carries the marker, so a crash mid-build leaves the target
  unconditionally out of date; a committed target never carries it.
- **Progress** (liveness) — unless every process crashes, the target gets
  built. Crashes are modeled as always possible but never forced.

### CycleLock.tla — blocking locks vs. chain-based cycle detection

Models complete `ensure` traversals over a recorded dependency graph: the
runid skip, the chain (`REDO_CHAIN`) cycle check *before* locking, the
blocking lock, the double-check after it, recursion into deps, commit. Error
unwinding releases held locks (RAII), matching the code. A "worker" is any
traversal with its own chain: a top-level `redo` process, a
`build_parallel` thread, or a future parallel `is_ood`/`ensure` worker.

| Config | Scenario | Result |
|---|---|---|
| `CycleLock_Diamond` | 2 workers, diamond DAG `a→{b,c}→d`, starting at `a` and `c` | ✅ terminates; shared dep `d` built exactly once; no lock leaked |
| `CycleLock_CycleSerial` | 1 worker, cycle `a↔b` | ✅ chain check fires; clean error, no deadlock (today's serial behavior is sound) |
| `CycleLock_CycleParallel` | 2 workers entering cycle `a↔b` from different sides | ⚠️ **DEADLOCK** (predicted; see below) |

### ParallelEnsure.tla — the PROPOSED aggressive parallel traversal

The design to implement, checked before writing the Rust. The recorded
dependency edges in the database are treated as a parallelization plan:
`ensure(t)` fans out over **all** of t's recorded deps at once, activating
each dep's own check speculatively and in parallel. Checking is unbounded;
do-file runs ("building") are bounded by the J-token budget. A target
settles only after every dep settles. Cycle handling uses the **waits-for
graph** fix demanded by `CycleLock_CycleParallel`: before t starts waiting
on dep d, one atomic action checks whether t is reachable from d through
live wait edges and fails t with a cycle error if so. Do-file failure is a
nondeterministic build outcome (`BuildsCanFail`), and errors propagate up
the wait edges, releasing tokens and edges on the way.

| Config | Scenario | Result |
|---|---|---|
| `ParallelEnsure_Diamond` | diamond `a→{b,c}→d`, J=2, builds may fail | ✅ all invariants + liveness |
| `ParallelEnsure_DiamondJ1` | same, J=1 (maximum token starvation) | ✅ still settles everything |
| `ParallelEnsure_Wide` | `a→{b,c,d}→e`, J=2 (more work than tokens), 1,953 distinct states | ✅ all invariants + liveness |
| `ParallelEnsure_Cycle` | cycle `a↔b` entered from both sides — **the scenario that deadlocks today** | ✅ every interleaving ends in a clean cycle error on both targets (`CycleErrorsOut`) |
| `ParallelEnsure_Coverage` | asserts "never two builds at once" on the wide graph, J=3 | ⚠️ violated **as predicted** — proves the model reaches genuinely parallel builds; the passing invariants are not vacuous |

Verified on every configuration: `BuildOnce` (at most one build per target
per session), `TokenBound` (running do-files + free tokens = J, including
on failure paths), `DepsSettledFirst` (a target never builds or settles
before all its recorded deps settled — the ordering the serial walk gives
for free, preserved under full concurrency), `EventuallyQuiescent` and
`Settles` (every activated target settles; TLC's deadlock check runs on all
configs, cyclic included).

### Speculation.tla — the full design: stale recorded deps vs. actual deps

ParallelEnsure equates recorded deps with true deps; this spec removes that
assumption and is the authoritative model of the design. **Recorded** deps
(last run's database edges) drive the speculative parallel fan-out;
**actual** deps are what the do-file declares via `redo-ifchange` when it
really runs — including deps discovered mid-build that speculation never
saw, and a fresh database with no recorded edges at all. Do-files hold a
token while executing (`brun`) and release it while blocked in
`redo-ifchange` (`bwait`) — the flat-budget rendering of the own-token
mechanism, justified by TokenPool's verified `Bound`.

The design rule this spec pins down — **failure severity**:

- **Speculative failures are SOFT.** A recorded dep that fails, or a cycle
  among speculative wait edges, only disqualifies the verify path
  (`mustRebuild`): the parent proceeds to run its do-file, whose ifchange
  calls are the ground truth. Consequence, proved by
  `Speculation_StaleCycle`: a **stale recorded cycle self-heals** — every
  interleaving ends all-built, none failed.
- **Actual failures are HARD.** A dep the running do-file demands failing,
  or a cycle closed by a mid-build wait edge, fails the target; errors
  propagate up the wait edges releasing tokens on the way. Proved by
  `Speculation_TrueCycle`: with an **empty database** (pure discovery-time
  detection), a genuinely cyclic project errors out on every interleaving —
  never a deadlock.

| Config | Scenario | Result |
|---|---|---|
| `Speculation_Diamond` | recorded = actual, all clean/dirty subsets, failures on (5,477 distinct states) | ✅ |
| `Speculation_Stale` | dropped dep, kept dep, mid-build discovered dep (5,166 states) | ✅ |
| `Speculation_StaleCycle` | database records a cycle; do-files fixed | ✅ `AllDone` — self-heals |
| `Speculation_TrueCycle` | empty database; do-files genuinely cyclic | ✅ `AllFail` — clean error |

Invariants on all configs: `BuildOnce`, `TokenBound` (executing do-files +
free tokens = J; blocked ones hold no slot), `ActualDepsFirst` (a committed
build saw every **actual** dep settled first — stale speculation can waste
work but never corrupt build order), `VerifiedSoundly` (verification never
rides over a soft failure and only fires on clean targets with all recorded
deps settled).

### Mutation testing — the checks have teeth

First-try green suites deserve suspicion, so each load-bearing mechanism
was deliberately broken in a copy of the model to confirm its check fails
(mutants under `/tmp/redo-msh-tlc/mutations/`, not part of `run.sh`):

| Mutation | Expected catch | Result |
|---|---|---|
| Remove the mid-build cycle check in `BWait` | deadlock | ✅ TLC: "Deadlock reached" |
| Make speculative cycles HARD instead of soft | self-healing (`AllDone`) breaks | ✅ temporal property violated |
| Blocked do-file keeps its token (no `Yield` release) | dependency chain at J=1 deadlocks | ✅ deadlock (control run on the unmutated model passes the same config) |
| `Commit` without awaiting the do-file's deps | ordering breaks | ✅ `ActualDepsFirst` violated |

**The implementation contract** — what the Rust must preserve for these
results to transfer:

1. **Atomic check-and-insert of wait edges.** The cycle-reachability check
   and the wait-edge insert must be one SQLite write transaction. Two
   concurrent inserts that each miss the other's edge recreate the
   deadlock. (In the specs they are one action; mutation 1 shows what
   happens when detection is absent, which is what a lost race degrades
   to.)
2. **Mid-build waits enter the same graph.** A do-file blocked in
   `redo-ifchange` is a wait edge exactly like a speculative check-phase
   wait — one shared waits-for graph. Detection must work with an empty
   database (`Speculation_TrueCycle`); an implementation that only tracks
   waits during the checking phase deadlocks on discovery-time cycles
   (mutation 1).
3. **Failure severity: speculative = soft, actual = hard.** A failure or
   cycle met while speculating over recorded edges must only force the
   rebuild path, never fail the parent — otherwise stale databases invent
   errors (mutation 2 breaks self-healing exactly this way). A failure or
   cycle met by a *running do-file's* ifchange is a real error and
   propagates up the wait edges, releasing tokens on the way.
4. **One claim per target per session** — the in-process task registry plus
   the cross-process per-target lock (the protocol verified in
   LockSession). Wait edges are cleared when a target settles, so
   reachability only ever sees traversals that can still block. A parent
   reads a dep's stamp only after the dep settles (settled state is stable
   for the rest of the session, so the read is race-free).
5. **A blocked do-file must not hold a job slot.** The own-token mechanism
   (or an explicit release-on-ifchange in a thread-based traversal) is
   load-bearing: mutation 4 shows a plain dependency chain deadlocking at
   `-j1` without it.
6. **Wait edges need the same liveness discipline as locks.** A crashed
   process's wait edges must be garbage-collected (scope them to the
   session and tie them to the same holder-liveness rule the target locks
   use). Stale edges from dead traversals cause false cycle errors —
   annoying, not corrupting — but the GC rule should exist from day one.
7. **Eager scheduling.** In the specs, work-conservation holds by
   construction: every launchable step is an enabled action and fairness
   forces it. The implementation inherits that as an obligation — a ready
   target plus a free token must eventually launch. `run_parallel`'s
   current wait-only-on-own-children loop does not satisfy it and needs the
   retry-on-timeout fix as part of this work.
8. Checking (hashing) is unbounded in the model. The implementation may cap
   checker threads separately for I/O reasons without affecting any
   verified property, as long as caps never introduce a wait that depends
   on another checker's completion.

## Gap found: concurrent traversals turn cycle errors into deadlocks

TLC's counterexample (11 states, `/tmp/redo-msh-tlc/CycleLock_CycleParallel.out`):
w1 locks `a` and recurses to `b`; w2 locks `b` and recurses to `a`; final
state `lock = [a ↦ w1, b ↦ w2]` with both workers blocked in lock
acquisition. Neither worker's chain contains the other's path, so the
pre-lock cycle check passes on both sides — the chain mechanism is
path-local and cannot see a cycle entered concurrently from two points.

This bites **today** in two forms, and would become routine with a third:

1. Two concurrent `redo` invocations (two terminals, CI + user) on a cyclic
   graph.
2. One `redo -j2` where two branches of a parallel group reach a cycle from
   different entry points (e.g. `all.do: redo-ifchange x y`; `x` leads to
   `a→b`, `y` leads to `b→a`).
3. A parallelized `is_ood`/`ensure` traversal — the motivating change —
   which makes form 2 the common case rather than the rare one.

Severity today is moderate: a dependency cycle is already a broken build, and
serial execution catches it with a clean error. But the failure mode is a
silent hang instead of an error, and parallelizing the ood traversal without
fixing this would make the hang easy to hit.

### Candidate fixes (to be specified and checked before implementing)

- **Waits-for edges in the database**: before blocking on a target's lock,
  record "worker W (chain C) waits for T held by W₂"; walk the waits-for
  graph and fail with a cycle error if the walk returns to W. This
  generalizes the chain check across workers and processes. (apenwarr redo
  solves the equivalent problem in its lock server.)
- **Try-lock with escalation**: on lock timeout, probe the holder's recorded
  chain and diagnose. Weaker (timeout-based), simpler.
- Static lock ordering is **not** available: redo discovers dependencies
  during execution, so no global order exists up front.

**Status: fix chosen, verified, and implemented.** `ParallelEnsure.tla`
specifies the waits-for-graph design and `ParallelEnsure_Cycle` proves it
turns this exact scenario into a clean error on every interleaving. The
implementation lives in `src/waits.rs` (the shared graph: atomic
check-and-insert in one SQLite write transaction, owner-liveness GC) and
`src/parallel.rs` (the speculative parallel traversal; soft/hard failure
severity per Speculation.tla). The chain check remains only as a fast path
for path-local cycles (it produces the readable `a -> b -> a` message);
`CycleLock_CycleParallel` is kept as-is, deliberately: it documents why the
chain mechanism alone is insufficient and must keep finding the deadlock.

## Abstractions and assumptions

- SQLite transactions (`write_txn`) are atomic actions; the double-check
  read and the marker transaction are modeled as one step (safe: the lock is
  held across both).
- Kernel file locks are released when a process dies (OS guarantee).
- A do-file blocks for the whole duration of its `redo-ifchange` calls. A
  do-file that backgrounds `redo-ifchange` and keeps computing would violate
  TokenPool's `Bound` (but not token conservation).
- Content hashing / out-of-date evaluation is abstracted to worst case:
  everything is out of date at session start. `redo-ifcreate`, `redo-always`,
  force-rebuild, and the overwrite prompt's token release/reacquire are not
  modeled.
- TokenPool models two levels of process nesting; the `Bound` result is
  checked to that depth, conjectured for deeper trees.

- ParallelEnsure equates recorded and actual deps — kept as the simpler
  introduction; Speculation.tla is the authoritative model (distinct
  recorded/actual deps, mid-build discovery, token release while blocked).
  Both model the token budget as one flat counter; the own/pool split and
  its compensation theorem are TokenPool's concern, and Speculation's
  `bwait` state leans on exactly that theorem.
- Out-of-date evaluation is a nondeterministic verdict (`clean` subsets +
  verify-vs-build choice) — a superset of real behaviors, sound for safety.
- Graphs checked are small (2–5 targets) but chosen adversarially: diamond
  (shared dep), wide fan-out with token starvation, stale/true/self cycles.
  The protocol bugs these specs target manifest at exactly this scale.

## Not covered / future work

- **Crash × wait edges**: LockSession verifies crash-safety of the
  lock/marker/runid state, and contract item 6 states the GC rule for wait
  edges; the combination (a crash mid-traversal leaving edges, concurrent
  GC, a new session starting) is stated but not model-checked. Worth a spec
  if the implementation's GC turns out subtler than "same liveness rule as
  locks".
- ~~The under-utilization behavior of `run_parallel`~~ — fixed: the old
  wait-only-on-own-children scheduler is gone; `src/parallel.rs` acquires
  tokens with a try-acquire retry loop, so a token freed anywhere in the
  process tree is observed within one poll interval (the eager-scheduling
  obligation, contract item 7).
- The overwrite prompt's release/spin-reacquire path in `prompt_overwrite`
  (a token-conservation risk worth adding to TokenPool).
- Rust-level concurrency (atomics orderings, channel lifetimes) is out of
  TLA+'s scope; `loom` tests of `run_parallel` and the jobserver's
  compare-exchange loop are the complementary tool.

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

The overwrite prompt (`prompt_overwrite` in `src/build.rs`) is modeled
through the `PromptMode` constant. A top job may stop before its do-file to
ask the human, with the target's kernel lock already held, so other jobs
that need that target block on the lock (holding their tokens) until the
prompter's build commits. Three modes:

| Config | `PromptMode` | Result |
|---|---|---|
| `TokenPool_J2`, `TokenPool_J3` | `none` | ✅ the original model (regression) |
| (`release`, not in `run.sh`) | `release` — what `prompt_overwrite` did before 2026-09-03: release one token to the pool, spin on the pool to get one back | ❌ **deadlock** (10 steps at J=2): the released token is taken by a job that then blocks on the prompter's lock; every other token ends up there too; the prompter's spin never sees a free token. Kept in the spec as the record of why the code holds its token. |
| `TokenPool_PromptHold` | `hold` — the prompter keeps its token while the human thinks | ✅ conservation, `Bound`, `Termination` (J=2, NTop=3, NSub=2; 73,564 states) |

Two smaller facts the release model also pins down: `Jobserver::release`
always writes to the *pool*, even when the worker runs on the process's own
token, so the pool transiently holds J tokens (`TypeOK` is widened for that
mode); and the reacquired token is then run under the `own` label, which
`FinishTop` frees as an own token — numerically a wash, so
`TokenConservation` (stated with the loan term `LentOwn`) still holds. The
deadlock, not the bookkeeping, is the reason to switch to `hold`.

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
| `Speculation_Reversed` | one edge REVERSED between runs (recorded a→b, actual b→a) | ❌ **kept failing on purpose** |

**The severity rule is refuted by `Speculation_Reversed`.** The configs
above never contained a dependency whose *direction* flips between runs — a
routine refactor. Given that shape, `BWait`'s cycle check (which cannot
tell hard edges from speculative `cedges`) hard-fails `b`'s real build
through `a`'s speculative edge in 4 states, violating `NoFailure` on an
acyclic project. "Stale edges can waste work but never invent an error" is
therefore FALSE for this design. The config is kept failing, like
`CycleLock_CycleParallel`, to document the hole; the corrected rules are
specified and verified in `SpeculationMP.tla` below.

### SpeculationMP.tla — the corrected multi-process machine (BUILD THIS)

Speculation.tla also models one **global** claim per target; the shipped
implementation had one claim per target **per process**, and its two
unmodeled waits — a second instance of an in-flight target blocking on the
per-target kernel lock, and `redo-ifchange` draining every task in its
process before returning — composed into a reproducible deadlock on an
acyclic project (stale recorded deps crossing two parallel branches; hangs
10/10 in practice, `SpeculationMP_CrossStale` is that exact shape) and
into fabricated cycle errors from speculative tasks.

`SpeculationMP.tla` is the first-principles replacement, modeling the real
execution shape honestly — multiple processes, per-process registries,
kernel build locks, drain-before-return — under corrected rules:

- **R1 — complete graph.** Every wait-until-settled is an edge in one
  shared by-name graph, inserted atomically with a cycle check. Spawning a
  speculative instance inserts a **creation edge** `ctx → s` (ctx = the
  target whose do-file owns the spawning process), which is what makes the
  eventual drain wait visible. Kernel-lock waits carry no edge and are
  *proven* safe: edges are keyed by target name, so waiter → name plus the
  foreign builder's own name → deps edges bridge the lock wait.
- **R2 — typed edges.** Checker waits and creation edges are SOFT;
  running-do-file demands are HARD.
- **R3 — cycle rules.** Soft insert on any cycle: refuse softly (checker
  rebuilds; speculation that could close a cycle is never started). Hard
  insert on an all-hard cycle: real dependency error. Hard insert on a
  cycle riding a soft edge: the *speculation* yields — evict a soft edge
  (checker → sticky mustRebuild; creation → the speculative instance
  aborts, even mid-do-file), then the demand retries.
- **R4 — quarantine.** A speculative failure or abort settles as `sfail`,
  is reported to no one, and a later real demand *reclaims* and re-runs
  the instance. Demanding a live speculative instance upgrades it
  (creation edge superseded by the demand edge), making it un-abortable.
- **R5 — drain cancellation.** A draining process may ABANDON any of its
  still-speculative instances instead of waiting for them, in any state
  including mid-build (the implementation kills the do-file — crash-safe
  by the LockSession argument: Uncommitted marker, kernel-released locks,
  temp GC). Abandonment settles `sfail` like any other speculative
  outcome. This bounds ifchange return latency: undemanded speculation
  cannot hold the caller hostage (measured: a stale 5s dep no longer
  delays an unrelated 0.07s rebuild). The implementation additionally
  keeps speculation off the process's OWN jobserver token unless a
  checker is actually waiting on its result — surplus parallelism only —
  so speculation can never capture the last token ahead of demanded
  work.
- **R6 — hand-edited targets.** The overwrite guard runs after the kernel
  lock is won, before the do-file. A target in `HandEdited` is only ever
  *asked about* (`prompt` state, lock held; `yes` → build, `no` → hard
  fail) by a demanded instance whose entire lineage — every builder
  process above it up to top — is demanded. Anywhere else (a speculative
  instance; a demanded instance inside a speculative lineage; an orphan
  whose builder is gone) the hand-edit **aborts the lineage**: the nearest
  speculative or orphaned root and its whole process subtree are killed,
  R5-style, settling `sfail`. The reason it must abort rather than
  quietly refuse: a demand can upgrade the lineage *before* the refusal is
  observed, and the refusal would then be reported as a real failure
  without the user ever having been asked. With `sfail`, the demand
  reclaims and re-runs the instance in a demanded lineage, which asks.
  A hand-edited target is never `clean` (its hash cannot match), so it
  cannot slip past the guard through `Verify`. Per-prompt answers are
  nondeterministic (`PromptAnswers`); the session-wide `all`/`quit`
  answers are refinements of that.

| Config | Scenario | Result |
|---|---|---|
| `SpeculationMP_CrossStale` | the reproduced implementation deadlock shape (crossed stale deps, acyclic reality) | ✅ no deadlock, `NoFailure`, all roots done |
| `SpeculationMP_Reversed` | the `Speculation_Reversed` shape that refutes the old design | ✅ `NoFailure`, all roots done |
| `SpeculationMP_TrueCycle` | genuinely cyclic do-files, empty database | ✅ `AllRootsFail` — clean error, never a hang |
| `SpeculationMP_Stale` | dropped/kept/discovered deps on the multi-process machine | ✅ |
| `SpeculationMP_SpecAbort` | a mid-do-file speculative build must abort retryably when its demand cycles into its spawning subtree | ✅ (`sfail` reachable via `brun`, verified by trace) |
| `SpeculationMP_HandEditSpec` | the SpecAbort shape with `s` hand-edited: only ever built speculatively | ✅ `NeverPrompted`, `NoFailure`, root done |
| `SpeculationMP_HandEditYes` | `c` (recorded+actual dep) and `d` (discovered) hand-edited, answers `yes` | ✅ root done, `PromptOnlyDemanded` |
| `SpeculationMP_HandEditNo` | same, answers `no` | ✅ `AllRootsFail` — the refusal is a reported hard failure |
| `SpeculationMP_HandEditLinDrop` | stale `a → b` builds `b` speculatively; `b`'s do-file demands hand-edited `x`; `a` no longer needs `b` | ✅ `NeverPrompted` — the lineage aborts, `a` rebuilds via `c` |
| `SpeculationMP_HandEditLinKeep` | same, but `a` still needs `b`: the lineage is upgraded before or after the abort | ✅ `x` is asked about exactly on the demanded path, root done |

Invariants on all acyclic configs: `NoFailure`, `ActualDepsFirst`,
`CommitOnce`, `LockConsistent`; liveness: `EventuallyQuiescent` plus
`AllRootsDone`/`AllRootsFail`. Tokens stay TokenPool's concern; the model
also encodes the implementation's real checker behavior of abandoning the
check *immediately* on must-rebuild evidence, racing ahead of its own
speculation — the interleavings where the old design died.

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

**Status: first implementation refuted; corrected design verified and
implemented** (`src/waits.rs` typed edges + eviction, `src/parallel.rs`
grades/quarantine/interruptible waits, `src/build.rs` demand loop and
watch propagation; integration tests `crossed_stale_deps_neither_hang_nor_error`
and `speculative_build_of_ancestor_dependent_aborts_quarantined` pin the
two reproduced failure shapes). History of the refutation: the original
waits-for-graph mechanism shipped in `src/waits.rs`
and `src/parallel.rs`, but adversarial review found the verified theorems
did not transfer: Speculation.tla assumes one global claim per target,
while the implementation claims per process — and the two waits that
difference creates (a duplicate instance blocking on the kernel target
lock, and `drain` blocking every ifchange return on all speculative tasks)
never entered the shared graph. Reproduced consequences on an ACYCLIC
project: a deterministic deadlock (crossed stale recorded deps, 10/10
hangs), silently-swallowed fabricated `X -> g -> X` cycle errors from
speculative tasks inheriting `REDO_CHAIN`, and a 64× ifchange-latency
hostage effect from draining stale speculative builds. Separately,
`Speculation_Reversed` refutes the old severity rule inside the spec
itself. The corrected machine — typed edges, creation edges, soft
eviction, speculation quarantine — is specified and fully verified in
`SpeculationMP.tla`, and the implementation now matches it. Two
implementation-level notes beyond the model: a `RealCycle` verdict seen
right after this process aborted a speculative lineage is re-checked for a
bounded window (the model's atomic abort corresponds to the lineage's
processes unwinding and clearing their residual edges), and eviction
delivery is by polling (evicted waiters re-check their edge on a 50ms
interruptible wait; speculative lineages carry their creation edges in
`REDO_SPEC_WATCH` and every blocking primitive polls the list). The chain
check's only remaining role is the readable `a -> b -> a` message for
path-local cycles, and it no longer runs on speculative task threads
(their chain is empty).
`CycleLock_CycleParallel` is kept as-is, deliberately: it documents why
the chain mechanism alone is insufficient and must keep finding the
deadlock.

## Abstractions and assumptions

- SQLite transactions (`write_txn`) are atomic actions; the double-check
  read and the marker transaction are modeled as one step (safe: the lock is
  held across both).
- Kernel file locks are released when a process dies (OS guarantee).
- A do-file blocks for the whole duration of its `redo-ifchange` calls. A
  do-file that backgrounds `redo-ifchange` and keeps computing would violate
  TokenPool's `Bound` (but not token conservation).
- Content hashing / out-of-date evaluation is abstracted to worst case:
  everything is out of date at session start. `redo-ifcreate`, `redo-always`
  and force-rebuild are not modeled. The overwrite prompt is: its token
  behavior in TokenPool (`PromptMode`), its speculation rule in
  SpeculationMP (R6). TokenPool models prompts at the top level only (a
  nested process's prompt is the same shape one level down); the
  `all`/`quit` answers are not distinguished from per-prompt `yes`/`no`.
- SpeculationMP's kills do not all cascade alike: R6's `KillSubtree` kills
  the whole process subtree (as the implementation's abort watch does),
  while `EvictCreation` and `AbandonSpec` settle only the aborted instance
  and let its sub-instances settle on their own. The latter is a sound
  over-approximation when sub-instances *can* settle; a hand-edited target
  under an orphaned demander would reclaim-loop forever, which is exactly
  why R6 cascades.
- A prompt can outlive its demander: a sibling demand's hard failure fails
  the builder while a question is open (`HandEditNo` reaches this). The
  implementation's drain-before-return keeps the child process alive until
  the question is answered; the answer is then stale but harmless.
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
- ~~The overwrite prompt's release/spin-reacquire path in
  `prompt_overwrite`~~ — modeled (TokenPool `PromptMode = "release"`): it
  was a deadlock, not just a conservation risk; the code now holds its
  token across the prompt (`TokenPool_PromptHold`). Still to do: gate
  prompts on a lineage-demanded check that aborts speculative lineages
  (R6), and decide interactivity once at the top level — a nested redo's
  stdin is null and its stderr is a log file, so today's per-process
  `user_present()` never asks below the top level.
- Rust-level concurrency (atomics orderings, channel lifetimes) is out of
  TLA+'s scope; `loom` tests of `run_parallel` and the jobserver's
  compare-exchange loop are the complementary tool.

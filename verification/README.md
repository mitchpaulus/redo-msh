# Formal verification of redo-msh's concurrency design

TLA+ specifications of the concurrency protocols in redo-msh, checked with
TLC. The specs verify the **design** (the protocol under all interleavings),
not the Rust code — the code must faithfully implement what is modeled here.
"Abstractions and assumptions" lists exactly what is taken on faith, and
"What model checking cannot close" lists the only things these specs do not
establish, with the reason each one is out of reach.

## Running

```
./run.sh
```

Requires Java 11+ and `tla2tools.jar` at `~/.local/share/java/tla2tools.jar`
(override with `TLA_TOOLS_JAR`). TLC logs go to `/tmp/redo-msh-tlc/`; each
run passes `-cleanup`, so TLC deletes its (sometimes multi-gigabyte) state
files when the run ends. The suite is 77 checks and takes about
15 minutes, most of it `SharedPaths_Fixed3`
(53,352,011 distinct states); every other check takes seconds
to a minute or two. `REDO_TLC_LONG=1 ./run.sh` adds
`ShallowRecheck_Settled3` (three concurrent runs, 52,602,234 distinct
states, about 10 more minutes).

Each spec is a `.tla` module plus `.cfg` model configurations. Most
configurations must pass. The others document a **known gap in today's
code** or are **coverage checks** proving another check is not vacuous; they pass only when TLC finds *exactly* the predicted
failure — a deadlock, or a violation of the one named invariant (`run.sh`
expects `violation:<Invariant>`; any other outcome is a FAIL). Every gap has
a sibling configuration that turns on the proposed fix through a spec
constant and must pass.

## Gaps found, and where the code fixes them

Each row is a TLC counterexample against a model of redo-msh as it was
before these fixes; those configurations keep modeling that code, as
regression documentation. "✔ repro" means the failure was also reproduced
against the real binary. Every row is now fixed in the code: the last two
columns name the fix (in `src/`) and the integration test that checks it,
where a deterministic test is possible.

| Config | What went wrong | Evidence | Fix in the code | Test |
|---|---|---|---|---|
| `CycleLock_CycleParallel` | two traversals entering a cycle from different sides deadlock instead of erroring | TLC, ✔ repro | a do-file's demands enter the shared waits-for graph, cycle-checked on insert, before any lock wait (`waits::try_demand`, SpeculationMP R1–R3) | `concurrent_cycle_errors_instead_of_deadlocking` |
| `CycleLock_DiamondTwoSessions`, `LockSession_TwoSessions` | two concurrent `redo` runs both build a shared out-of-date dep | TLC, ✔ repro | shallow out-of-date re-check under the lock (`is_ood_shallow`) | `concurrent_runs_build_shared_dependency_once` |
| `ShallowRecheck_Shallow`, `_ShallowSpec` | that shallow re-check, after the run waiting on the lock decided to rebuild before checking a buildable dep: another run built the target over that dep, the dep's source then changed, and the waiting run skips the target and ends "up to date" with it built from a stale dep (**silent staleness**, on `main` too) | TLC, ✔ repro | skip only if every buildable dep has settled in this session (`is_ood_shallow`) | `waiting_run_does_not_skip_over_an_unchecked_dep` |
| `LockSession_CommitWindow` | crash/failed commit between the output rename and the commit transaction ⇒ every later build refuses redo's own output as "modified outside redo" | TLC, ✔ repro | pending hash recorded before the rename; guard accepts and adopts it (`record_pending`, `Verdict::Pending`, `adopt_pending`) | `crash_between_install_and_commit_is_recovered` |
| `LockSession_StaticAdopt` | the same window on a target's **first** build, with only a `default*.do` ⇒ redo records its own output as a **source**, permanently | TLC, ✔ repro | the static-source rule respects the pending hash (`pending_vouches`) | `crash_on_first_build_does_not_create_a_source` |
| `LockSession_RacyStamp` | a source rewritten within the timestamp tick in which redo hashed it keeps its mtime; `current_csum` trusts the cache and the target stays "up to date" over stale input (**silent staleness**) | TLC, ✔ repro | racily-clean rule: a cached hash taken within 2 s of the mtime is re-checked (`stamp_is_racy`, `files.hashed_at`) | `racy_source_rewrite_is_detected` |
| `SharedPaths_TempGc` | a second run's startup `gc_temps` deletes a live build's stdout capture ⇒ that target is committed as "no output" and **deleted** | TLC, ✔ repro | captures named by target key, swept only under the target's lock (`gc_temps`) | `concurrent_run_does_not_delete_stdout_output` |
| `SharedPaths_TmpGc` | `gc_logs` deletes a live build's log temp before its rename ⇒ build fails | TLC | log temps swept only under the target's lock (`gc_logs`) | (a few-syscall window) |
| `SharedPaths_LogRace`, `_LogGc`, `_Foreign` | a lagging follower deletes or tails a log another run replaced ⇒ false "(crashed)", "(log missing)", a run waiting on another run's build | TLC, ✔ repro | session-scoped target logs `t.<key>.<session>.log` (`target_log_path`) | `concurrent_runs_build_shared_dependency_once` |
| `SharedPaths_RunLockRace` | a GC unlinks another run's `run.<s>.lock` between its creation and its lock; a third run's GC then deletes the live run trace ⇒ that run **hangs** at exit | TLC | the run lock is re-checked after locking (`acquire_run_lock`); runs are swept under their own lock (`gc_logs`) | (a create-to-lock window) |
| `TokenPool_PromptDeadlock` | two answered overwrite prompts in a nested `-j2` build: the second spins forever on the token pool | TLC | a prompting job keeps its token (`prompt_overwrite`) | (needs a terminal) |
| `TokenPoolPromptLock` (`release` mode) | the same release-and-spin prompt deadlocks at `-j2` when other jobs block on the prompter's target lock | TLC | the same fix | (needs a terminal) |
| `TokenPool_UnderUtilization` | `run_parallel` sleeps with launchable work while a pool token is free elsewhere (throughput) | TLC | a build waiting for a token re-polls the pool every 50 ms (`acquire_token` in `src/parallel.rs`) | (throughput only) |
| `WaitGraph_StaleEdge` | a crashed run's wait edges plus changed do-files produce a false cycle error | TLC | edges carry their owner process, tied to a kernel liveness lock; dead owners' edges are swept (`waits::gc_dead_edges`, `gc_sweep`) | (needs a crash mid-wait) |
| `Speculation_Reversed`, `SpeculationMP_CrossStale` | a dependency reversed between runs fabricates a cycle error; crossed stale recorded deps across two parallel branches deadlock an acyclic project | TLC, ✔ repro | typed edges, creation edges, soft eviction, quarantine (SpeculationMP R1–R5; `src/waits.rs`, `src/parallel.rs`) | `crossed_stale_deps_neither_hang_nor_error`, `speculative_build_of_ancestor_dependent_aborts_quarantined` |
| `SpeculationMP_HandEdit*` | a speculative build interrupts the human about a hand-edited target nobody demanded, or reports a refusal nobody was asked | TLC | only fully demanded lineages ask; elsewhere the lineage aborts (R6, `abort_unless_lineage_demanded`) | `hand_edit_under_speculation_aborts_instead_of_asking` |

## Accepted limitations

Out of scope by decision. No spec checks these, and no fix is planned:

- **Editing a built output while a build of it runs.** An edit that lands
  while redo is replacing that output can be lost: between the overwrite
  check and the rename, through a file handle a program opened on the old
  output, or by being recorded as redo's own output. Editing outputs is not
  a supported workflow. Edits made *between* builds are still caught by the
  overwrite check, and the database always stays consistent with what is on
  disk.
- **Files created at a phony target's path** are deleted by the next build
  of that target.
- **Read-only outputs** are friction against accidental edits, an
  implementation detail; no verified property depends on them.

## The specs

### TokenPool.tla — the jobserver token protocol

Models `src/jobserver.rs`, the scheduler and `prompt_overwrite` over a job
**tree of arbitrary depth** (`FanOut[k]` children per job at depth k−1; each
non-leaf job's do-file blocks in `redo-ifchange` while a child process runs
its children). J total tokens: one implicit own token per process, a shared
pool of J−1, non-blocking try-acquire, own token first. `Lazy` is the former
`run_parallel` (a process that finds the pool empty sleeps until one of
*its own* jobs completes); without it, the eager scheduler the contract
requires, which `src/parallel.rs` implements. `PromptMode`: `"pool"` (the
former code: release a token to the pool, spin on pool try-acquire), `"hold"` (keep the token), `"source"` (give the token
back where it came from, resume on the process's own token if free, else a
pool token).

| Config | Scenario | Result |
|---|---|---|
| `TokenPool_D2J2` / `_D2J3` | today's scheduler, depth 2 (2×2, J=2; 3×2, J=3) | ✅ (249 / 4,692 states) |
| `TokenPool_D3J2` / `_D3J3` | today's scheduler, depth 3 (1×2×2: a branching process at every nested level) | ✅ (254 / 412 states) |
| `TokenPool_DepthCoverage` | depth 3, J=3 | ⚠️ `NeverTwoDeepExecuting` violated (predicted): nested jobs really run in parallel |
| `TokenPool_Eager` | eager scheduler, depth 3 | ✅ incl. `WorkConserving` |
| `TokenPool_UnderUtilization` | today's scheduler | ⚠️ `WorkConserving` violated (predicted) |
| `TokenPool_PromptDeadlock` | today's prompt path, today's scheduler | ⚠️ **DEADLOCK** (predicted) |
| `TokenPool_PromptHold` | prompt keeps its token, depth 3 | ✅ |
| `TokenPool_PromptSource` / `_PromptSourceJ3` | prompt returns its token to its source | ✅ (50,233 states at J=3) |

Properties: **TokenConservation** (pool tokens never lost or duplicated;
a "pool"-mode prompt accounts for its extra release), **OwnConsistent** /
**OwnUnique** (a process's own token is free iff unheld, and held by at most
one job), **TokenHeld** (every launched, unfinished job holds a token, except
a "source"-mode prompt), **ChildrenOnlyWhileWaiting** (a child process's jobs
are live only while their parent job is blocked), **Bound** (at most J jobs
execute), **ExactBound** (executing jobs + free pool tokens ≤ J),
**SemaphoreMax** (the pool never exceeds J — on Windows a semaphore with
maximum count J, where an over-release fails silently), **Termination**.
All hold on every passing configuration; `TokenPool_PromptDeadlock` holds
every safety property up to its deadlock.

**ExactBound for every depth.** TLC checks depths 2 and 3; the following
argument extends the result to any tree using only the lemmas above, each of
which relates one process to its parent job and the pool (so depth 3
already exercises every step shape). Call a job *anchored* if it is
executing, or waiting at a prompt in `"pool"`/`"hold"` mode. By TokenHeld
every anchored job holds a token. Map each anchored job to a *slot*: follow
its token — if it is a pool token, stop; if it is the top-level process's
own token, stop; otherwise it is the own token of a child process p, whose
parent job (by ChildrenOnlyWhileWaiting) is `waiting` and, by TokenHeld,
holds a token itself — continue from that job. Every job passed through on
the way up is `waiting`, hence not anchored. Two anchored jobs cannot reach
the same slot: their paths would first meet at some job q, entered from two
different own-token holders of q's child process, contradicting OwnUnique
(or at one pool token, which one job holds). So
`#anchored ≤ PoolHeld + 1`. TokenConservation gives
`PoolHeld = J − 1 − pool + (pool-mode prompts)`, and anchored jobs include
every pool-mode prompt, so `#executing + pool ≤ J`. ∎

### TokenPoolPromptLock.tla — the overwrite prompt under lock contention

The same token protocol, two levels deep (`NTop` top jobs, `NSub` sub jobs
each), with what TokenPool leaves out: the overwrite prompt
(`prompt_overwrite` in `src/build.rs`) runs with the target's kernel lock
already held, so other jobs that need that target block on the lock
(holding their tokens) until the prompter's build commits. Three modes:

| Config | `PromptMode` | Result |
|---|---|---|
| `TokenPoolPromptLock_J2`, `_J3` | `none` | ✅ the original model (regression) |
| (`release`, not in `run.sh`) | `release` — what `prompt_overwrite` did before 2026-09-03: release one token to the pool, spin on the pool to get one back | ❌ **deadlock** (10 steps at J=2): the released token is taken by a job that then blocks on the prompter's lock; every other token ends up there too; the prompter's spin never sees a free token. Kept in the spec as the record of why the code holds its token. |
| `TokenPoolPromptLock_PromptHold` | `hold` — the prompter keeps its token while the human thinks | ✅ conservation, `Bound`, `Termination` (J=2, NTop=3, NSub=2; 73,564 states) |

Two smaller facts the release model also pins down: `Jobserver::release`
always writes to the *pool*, even when the worker runs on the process's own
token, so the pool transiently holds J tokens (`TypeOK` is widened for that
mode); and the reacquired token is then run under the `own` label, which
`FinishTop` frees as an own token — numerically a wash, so
`TokenConservation` (stated with the loan term `LentOwn`) still holds. The
deadlock, not the bookkeeping, is the reason to switch to `hold`. Both
models reach the same fix from different deadlocks: TokenPool's from two
nested prompts (`TokenPool_PromptDeadlock`), this one's from lock waiters.

### LockSession.tla — the per-target build protocol, across sessions

One target and racing processes running `is_target` → `ensure` → `build` →
`build_inner`, in **one or more sessions** (a session is one top-level
`redo`). Modeled: djb's static-source rule (`ExactDoFile = FALSE`), the
session skip and **unlocked `is_ood`** computed through the source's **stamp
cache** (`current_csum`'s mtime/size fast path), `mark_verified` without a
lock, the kernel lock, the double-check, **forced** builds (top-level
`redo`: no skip, no static rule), the overwrite guard (a refusal is final),
the marker transaction, `redo-ifchange` stamping the source then the do-file
reading it, **phony** targets (no output: the build removes the target), the
rename, and the commit transaction — rename and commit as separate steps.
**Crashes** anywhere; **source edits** at any moment; with `CoarseTicks`,
writes share a timestamp until the clock ticks. `Always` models
`redo-always`; a `redo-ifcreate` path appearing is a source edit. Content is
modeled by identity (nondeterministic do-files, the worst case); both
"never built" and "built" initial states are explored. The scope is redo's
own correctness, so the model has no hand edits of outputs (see Accepted
limitations).

Fix switches: `RecheckOod` (re-run `is_ood` under the lock),
`HashBeforeRename` (record the hash of `$3` as pending before the rename;
the overwrite guard and the static-source rule accept a file matching it;
the marker transaction adopts an accepted pending hash), `RacyGitRule`
(trust a cached stamp only if it was taken in a later tick than the mtime it
recorded — git's "racily clean" rule).

| Config | Scenario | Result |
|---|---|---|
| `LockSession_OneSession` | today, 1 session, 2 ifchange + 1 forced, a source edit | ✅ (7,331 states) |
| `LockSession_Always` | today, `redo-always`, 2 sessions + forced | ✅ |
| `LockSession_CommitWindow` | today | ⚠️ `NoFalseRefusal` violated (predicted) |
| `LockSession_TwoSessions` | today, 2 sessions | ⚠️ `NoRedundantBuild` violated (predicted) |
| `LockSession_StaticAdopt` | today, only `default*.do` | ⚠️ `NoOwnOutputAsSource` violated (predicted) |
| `LockSession_RacyStamp` | today, coarse ticks, two source edits | ⚠️ `UpToDateIsTrue` violated (predicted) |
| `LockSession_Fixed` | all fixes; 2 sessions, 3 processes (one forced), coarse ticks, a source edit | ✅ every property (58,656 states) |
| `LockSession_FixedRacy` | all fixes, two source edits in one tick | ✅ (15,010 states) |
| `LockSession_FixedDefaultDo` | all fixes, only `default*.do` | ✅ (1,096 states) |
| `LockSession_FixedPhony` | all fixes, phony target | ✅ |
| `LockSession_Coverage` | fixed protocol | ⚠️ `NeverCommitWindowLeftover` violated (predicted): the crash window is explored |

Properties: **Mutex / LockHeld**; **MarkerDiscipline**; **NoRedundantBuild**
(a non-forced build starts only if the target is *truly* out of date);
**NoFalseRefusal** (the guard never refuses — with no hand edits in the
model, any refusal would be of redo's own output); **NoOwnOutputAsSource**;
**DiskDbAgree** (with no build in flight, the database describes what is on
disk); **UpToDateIsTrue** (if `is_ood`, as the code computes it through the
cache, says "up to date", the output was built from the *current* source);
**Termination**.

### SharedPaths.tla — files shared by concurrent runs

The cross-session behavior lives in the files concurrent runs share under
`.redom/`. Sessions take their run lock (`File::create`, then the kernel
lock), create the run trace, run the startup GC (today: **probe, then delete
by path later**), start the log follower, and — for sessions in `Builders`
— lock and build the target (`create_log` temp + rename, `do` event,
stdout capture, capture read, `done` append), append their own `done`, and
exit (unlock, remove their run files). Other sessions are runs with nothing
to build: they only take part in the GC races. Files are *instances* at
paths (a reader keeps the instance it opened; an append by path reaches
whatever is there, or fails). Crashes anywhere (bounded by `MaxCrashes`).
After every session ends, a **janitor** (the next run's startup sweep)
sweeps once, so GC *completeness* is checkable.

Fix switches: `GcByLiveness` (captures/temps swept only when their build no
longer holds the target lock), `SessionScopedLogs` (`t.<key>.<session>.log`,
swept when its session is dead), `GcAtomic` (probe and delete each file
while holding its liveness lock), `InodeCheck` (after locking
`run.<s>.lock`, verify the path still names the locked file, else retry).
`RunLockRaceTwoStepGc` and `RunLockRaceNoInodeCheck` show each of the last
two is necessary on its own.

| Config | Result |
|---|---|
| `SharedPaths_TempGc` | ⚠️ `NoLostOutput` violated (predicted) |
| `SharedPaths_TmpGc` | ⚠️ `NoGcFailedBuild` violated (predicted) |
| `SharedPaths_LogRace` | ⚠️ `NoFalseCrash` violated (predicted) |
| `SharedPaths_LogGc` | ⚠️ `NoLostLog` violated (predicted) |
| `SharedPaths_Foreign` | ⚠️ `FollowsOwnBuild` violated (predicted) |
| `SharedPaths_RunLockRace` / `…TwoStepGc` / `…NoInodeCheck` | ⚠️ `NoLostEvent` violated (predicted) — 3 sessions |
| `SharedPaths_Fixed` | ✅ every property + `Termination`, 2 builders, crashes unbounded (52,165 states) |
| `SharedPaths_Fixed3` | ✅ every invariant, 2 builders + 1 sweeping run, ≤ 1 crash, symmetry over builders (53,352,011 states) |
| `SharedPaths_Coverage` | ⚠️ `JanitorCleans` violated (predicted) with a GC that never deletes |

Properties: **NoLostOutput**, **NoGcFailedBuild**, **NoLostEvent** (every
append to a live run's trace lands), **NoFalseCrash**, **NoLostLog**,
**FollowsOwnBuild**, **CleanExit** (an exiting run leaves none of its files),
**JanitorCleans** (once every run has ended, one sweep removes every file —
nothing leaks forever), **Termination**. Liveness under symmetry is unsound
in TLC, so `Fixed3` checks invariants and `Fixed` checks termination.

### CycleLock.tla — blocking locks vs. chain-based cycle detection

Complete traversals over a dependency graph: the session skip and the
unlocked up-to-date check, the chain (`REDO_CHAIN`) cycle check *before*
locking, the blocking lock, the double-check, recursion into deps while
holding the parent's lock (the **do-file walk**; the code's other walk,
`is_ood` over recorded deps before the parent is locked, holds no parent
lock), commit, RAII unwinding. `SessionOf` says which workers share a
session; runid records *which* session built a target. `RecheckFresh`
selects today's double-check or the fix.

| Config | Scenario | Result |
|---|---|---|
| `CycleLock_Diamond` | 2 workers, 1 session, diamond `a→{b,c}→d` | ✅ `d` built exactly once; no lock leaked |
| `CycleLock_DiamondTwoSessions` | the same race between two `redo` invocations | ⚠️ `BuildOnce` violated (predicted) |
| `CycleLock_DiamondTwoSessionsFix` | ditto, `RecheckFresh` | ✅ |
| `CycleLock_CycleSerial` | 1 worker, cycle `a↔b` | ✅ clean error, no deadlock |
| `CycleLock_CycleParallel` | 2 workers entering `a↔b` from different sides | ⚠️ **DEADLOCK** (predicted; see below). Same with two sessions. |

### ShallowRecheck.tla — the double-check under the lock, with content

The fix for `CycleLock_DiamondTwoSessions` re-checks a target under its lock
against the files as they are now. CycleLock checks that fix with an oracle
(`fresh`; nothing ever changes there) and LockSession with one target over
one source, so neither can see a shallow re-check misjudge a *buildable*
dependency. Here content is exact: a source's content is its version, a
target's content is the record of the dependency contents its do-file read
(which is also what the database records as its edges), so staleness
propagates and `TrueUTD` is the ground truth. Sessions start at any time and
sources are edited at any time. Each session has a task registry (one task
per target). The unlocked checker reads evidence in any order, stops at the
first out-of-date sign, and may start dependency tasks at any point, which
covers main's in-order `is_ood`; with `SpawnFirst` it starts every recorded
buildable dep first, as this branch's checker does. Then lock, the
re-check, and the do-file walk (each dep demanded, settled, read; commit).

`Recheck` picks the double-check: `"none"` (this session's runid only; this
branch before the merge), `"shallow"` (or every recorded edge matches the
disk; main), `"settled"` (shallow, and every buildable dep's runid is this
session; the merged code), `"await"` (under the lock, wait for every
buildable dep to settle in this session, then compare; not implemented).

| Config | Scenario | Result |
|---|---|---|
| `ShallowRecheck_Shallow` | main's re-check, main's checker | ⚠️ `NoStaleRun` violated (predicted): the reproduced trace, step for step |
| `ShallowRecheck_ShallowSpec` | main's re-check, this branch's checker | ⚠️ `NoStaleRun` violated (predicted): starting the dep task does not help; nothing waits for it |
| `ShallowRecheck_Settled` | the merged code, 3 edits | ✅ `NoStaleRun`, `Termination` (537,715 states) |
| `ShallowRecheck_SettledMain` | the merged condition under main's checker | ✅ (134,925 states) |
| `ShallowRecheck_SettledChain` | `T ← D ← E ← es`: staleness two levels down | ✅ (727,375 states) |
| `ShallowRecheck_Settled3` | three concurrent runs (opt-in, `REDO_TLC_LONG`) | ✅ `NoStaleRun` (52,602,234 states) |
| `ShallowRecheck_SettledRedundant` | the merged code | ⚠️ `NoRedundantBuild` violated (predicted): the cost, below |
| `ShallowRecheck_Coverage` | the merged code | ⚠️ `NeverSkipAtRecheck` violated (predicted): it still skips on content, so main's benefit is kept |
| `ShallowRecheck_None` | no content re-check | ⚠️ `NoRedundantBuild` violated (predicted): `LockSession_TwoSessions` again, with content; never stale |
| `ShallowRecheck_Await` / `_AwaitChain` | the alternative | ✅ `NoStaleRun`, `NoRedundantBuild`, `Termination` |

Properties: **NoStaleRun** (a run during which no source was edited ends
with every target it settled truly up to date; edits *during* a run may be
missed by any build system, so they are excluded), **NoRedundantBuild** (a
build starts only on a target that is truly out of date at that moment),
**NeverSkipAtRecheck** (coverage), **Termination**.

**The cost of the merged condition.** It can rebuild a target another run
just brought up to date, in two ways TLC finds: this run's own task for the
dep has not settled yet when the lock frees, or a concurrent run's
`mark_verified` overwrote the dep's runid after this run settled it
(`files.runid` holds one session, the last). Both err toward building: only
session S ever writes runid S, so the evidence can vanish but never appear
falsely. `"await"` removes both by waiting under the lock and using the
session's own task results instead of the shared column. Implementing it
means an in-process wait under a held target lock, which must enter the
waits-for graph as a demand edge first (SpeculationMP R1); the model has
an acyclic graph and no waits graph, so it does not establish that part.

### ParallelEnsure.tla — the PROPOSED aggressive parallel traversal

`ensure(t)` fans out over **all** of t's recorded deps at once; checking is
unbounded, building is bounded by J tokens; a target settles only after
every dep settles. Cycles are handled by the **waits-for graph**: before t
waits on d, one atomic action checks whether t is reachable from d through
live wait edges and fails t if so. Do-file failure is nondeterministic and
propagates up the wait edges.

| Config | Scenario | Result |
|---|---|---|
| `ParallelEnsure_Diamond` | diamond, J=2, builds may fail | ✅ |
| `ParallelEnsure_DiamondJ1` | same, J=1 | ✅ |
| `ParallelEnsure_Wide` | `a→{b,c,d}→e`, J=2 | ✅ |
| `ParallelEnsure_Cycle` | `a↔b` from both sides | ✅ clean cycle error on every interleaving |
| `ParallelEnsure_Coverage` | wide graph, J=3 | ⚠️ `NeverTwoBuilding` violated (predicted) |

Properties: `BuildOnce`, `TokenBound`, `DepsSettledFirst`,
`EventuallyQuiescent`, `Settles`, `CycleErrorsOut`. Its verify-vs-build
verdict is unconstrained (sound for these properties, which hold for every
verdict); staleness is checked in Speculation.

### Speculation.tla — stale recorded deps vs. actual deps

The authoritative model of the design: **recorded** deps drive the
speculative fan-out; **actual** deps are what the running do-file declares,
including mid-build discoveries and an empty database. Out-of-date
evaluation has `is_ood`'s two parts: the target's **own inputs** are clean
(restricted to targets whose recorded deps equal their actual deps — the
determinism assumption made explicit) and **no recorded dep was rebuilt with
different content** (each commit chooses nondeterministically whether its
content changed, so content-hash pruning is explored). **Speculative
failures are soft** (a stale recorded cycle self-heals), **actual failures
are hard** (a real cycle errors out, never deadlocks).

| Config | Scenario | Result |
|---|---|---|
| `Speculation_Diamond` | recorded = actual, all clean/dirty subsets, changed/unchanged rebuilds, failures | ✅ (9,955 states) |
| `Speculation_Stale` | dropped, kept and mid-build-discovered deps | ✅ |
| `Speculation_StaleCycle` | database records a cycle; do-files fixed | ✅ `AllDone` |
| `Speculation_TrueCycle` | empty database; do-files cyclic | ✅ `AllFail` |
| `Speculation_Coverage` | diamond | ⚠️ `NeverVerifiedOverRebuild` violated (predicted): pruning is reachable |
| `Speculation_Reversed` | one edge REVERSED between runs (recorded a→b, actual b→a) | ❌ **kept failing on purpose** |

Properties: `BuildOnce`, `TokenBound`, `ActualDepsFirst`, `VerifiedSoundly`
(a verified target is clean, every recorded dep settled ok, none changed
content), `EventuallyQuiescent`, `Settles`.

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

### WaitGraph.tla — the proposed waits-for graph across sessions and crashes

(Written for the lock-wait graph, `WaitEdge`, since superseded by
SpeculationMP's settle-wait graph. The finding transfers: that graph's
edges are deliberately shared across sessions, so they must be ignored once
their owner is dead, which `src/waits.rs` does by owner liveness lock.)

The single-session specs above cannot see what a crashed run's wait edges
do to the next run. Here the edges live in the shared database, tagged with
their session; each session traverses the graph *its* do-files declare (do-
files can change between runs); a session can crash, releasing its locks
but leaving its edges; `GcByLiveness` (contract item 6) makes reachability
ignore edges of dead sessions.

| Config | Scenario | Result |
|---|---|---|
| `WaitGraph_StaleEdge` | s1 (`a→b`) crashes mid-wait; do-files change to `b→a`; s2 runs; no edge GC | ⚠️ `NoFalseCycle` violated (predicted) |
| `WaitGraph_StaleEdgeGc` | the same with edge GC | ✅ `NoFalseCycle`, `Termination` |
| `WaitGraph_LiveCross` | two live sessions whose do-files disagree, racing | ✅ a real mutual wait is reported, never hung |
| `WaitGraph_TrueCycle` | a cyclic project | ✅ reported (`SomeoneFails`), never hung |
| `WaitGraph_Coverage` | LiveCross | ⚠️ `NeverFails` violated (predicted): the mutual wait is reached |

## Mutation testing — the checks have teeth

Each load-bearing mechanism was broken in a copy of the model to confirm a
check fails (not part of `run.sh`; each row names the one edit):

| Mutation | Expected catch | Result |
|---|---|---|
| Remove the mid-build cycle check in `BWait` | deadlock | ✅ deadlock |
| Make speculative cycles HARD instead of soft | self-healing breaks | ✅ `AllDone` violated |
| Blocked do-file keeps its token (no `Yield` release) | chain at J=1 deadlocks | ✅ deadlock |
| `Commit` without awaiting the do-file's deps | ordering breaks | ✅ `ActualDepsFirst` violated |
| Speculation: old `Verify` guard (no `~DepChanged(t)`) | verifies over a changed dep | ✅ `VerifiedSoundly` violated |
| LockSession: accept the pending hash but do not adopt it in `MarkerTxn` | a second crash re-creates the false refusal | ✅ `NoFalseRefusal` violated |
| LockSession: the do-file reads the source *before* `redo-ifchange` stamps it | silent staleness | ✅ `UpToDateIsTrue` violated |
| SharedPaths: GC never deletes (`JanitorDeletes = FALSE`, kept as `SharedPaths_Coverage`) | leak | ✅ `JanitorCleans` violated |

The coverage configurations play the same role inside `run.sh`: each proves
that a passing property is passing on a model that genuinely reaches the
situation it is about.

## The implementation contract

What the Rust must preserve for these results to transfer:

1. **Atomic check-and-insert of wait edges** (one SQLite write transaction).
2. **Mid-build waits enter the same waits-for graph**; detection must work
   with an empty database.
3. **Failure severity: speculative = soft, actual = hard.**
4. **No redundant claim across sessions:** the double-check under the lock
   re-asks whether the target is out of date, not only whether *this*
   session built it.
5. **A blocked do-file must not hold a job slot.**
6. **Wait edges are ignored or swept once their owner is dead**
   (`WaitGraph_StaleEdge` shows the false cycle otherwise). The graph itself
   is shared across sessions: concurrent runs share the target locks, so a
   cross-session cycle is a real one.
7. **Eager scheduling:** a launchable job plus a free token must eventually
   launch (`TokenPool_UnderUtilization` shows the former lazy scheduler did
   not).
8. Checker threads may be capped for I/O, as long as caps never introduce a
   wait that depends on another checker's completion.
9. **Record the output's hash before installing it:** hash `$3` and record
   it as pending before the rename; the overwrite guard accepts the
   recorded or the pending hash, the marker transaction adopts an accepted
   pending hash, and the static-source rule treats a pending-vouched file as
   generated (`CommitWindow`, `StaticAdopt`).
10. **Racily-clean stamps are re-hashed:** trust a cached stamp only if it
    was taken in a later timestamp tick than the mtime it records
    (`RacyStamp`).
11. **Shared files are deleted only by liveness:** captures and log temps
    while their build no longer holds the target lock; session-scoped logs
    and run files while their session's run lock is free — probed and
    deleted under that lock, with the creator re-checking that the lock
    path still names its locked file.
12. **Never hold-and-wait on the token pool:** a prompting job either keeps
    its token or returns it to its source and may resume on its process's
    own token (`PromptHold`, `PromptSource`).
13. **The double-check needs settled deps:** the shallow re-check of item 4
    may skip a build only when every buildable dep has already settled in
    *this* session; a dep still settling (possibly being rebuilt by this
    session's speculation) sends the target down the build path
    (`ShallowRecheck_Shallow` shows the silent staleness otherwise).

## Gap found: concurrent traversals turn cycle errors into deadlocks

TLC's counterexample (`CycleLock_CycleParallel`): w1 locks `a` and recurses
to `b`; w2 locks `b` and recurses to `a`; both block. Neither worker's chain
contains the other's path, so the pre-lock check passes on both sides — the
chain mechanism is path-local. It bites today with two concurrent `redo`
invocations on a cyclic graph, or one `redo -j2` whose parallel branches
reach a cycle from different entry points, and would become routine with a
parallelized `is_ood`. The chosen fix — the waits-for graph — is verified in
`ParallelEnsure_Cycle`, `Speculation_TrueCycle` and, across sessions and
crashes, `WaitGraph_*`. `CycleLock_CycleParallel` keeps modeling the earlier chain-only code.

**Status: first implementation refuted; corrected design verified and
implemented.** Two waits-for graphs were built in parallel: one recorded
contended *lock* waits (`WaitEdge`, keyed by the lock holder), the other
every *settle* wait of the parallel traversal. The second subsumes the
first and is the one in the code: `src/waits.rs` (typed edges, eviction,
dead-owner GC), `src/parallel.rs` (grades, quarantine, interruptible
waits), and the demand loop in `src/build.rs`, verified as
`SpeculationMP.tla`. A running do-file's `redo-ifchange` inserts its hard
demand edges before it can block on any lock, which is what turns
`CycleLock_CycleParallel`'s deadlock into an error
(`concurrent_cycle_errors_instead_of_deadlocking`); integration tests
`crossed_stale_deps_neither_hang_nor_error` and
`speculative_build_of_ancestor_dependent_aborts_quarantined` pin the two
failure shapes reproduced against the first parallel traversal.


History of the refutation: the original
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

## Abstractions and assumptions

- SQLite transactions are atomic actions; filesystem operations (rename,
  create, delete, append) are separate actions from the transactions around
  them.
- Kernel file locks are released when a process dies. A reader keeps the
  file instance it opened after a rename or delete (POSIX; Windows with
  `FILE_SHARE_DELETE`, which Rust's `std` passes by default).
- Timestamps: with `CoarseTicks`, every write in a tick gets the same mtime
  (the worst case: jiffy granularity on Linux, 2 s on FAT); otherwise each
  write gets a fresh one. Same-size edits are assumed possible.
- A do-file blocks for the whole of its `redo-ifchange` calls, and calls
  `redo-ifchange` on a source before reading it (mutation 7 shows what
  breaks otherwise).
- LockSession and SharedPaths model no edits of built outputs (see
  Accepted limitations) and a non-interactive overwrite guard. The prompt
  itself is modeled for its token behavior (TokenPool, TokenPoolPromptLock:
  `PromptMode`, top level only in the latter) and its speculation rule
  (SpeculationMP R6); the `all`/`quit` answers are not distinguished from
  per-prompt `yes`/`no`.
- Content is modeled by identity — every build is distinct (nondeterministic
  do-files), the worst case for every hash comparison.
- ParallelEnsure, Speculation and SpeculationMP model the token budget as
  one flat counter (SpeculationMP: not at all); TokenPool's `ExactBound`
  (proved above for every depth) justifies it.
- After a history flag records a bad step, a model continues as if the step
  had succeeded; flags are read only by invariants.
- ShallowRecheck has no crashes, failures or cycles, deterministic do-files
  (content is a function of the inputs read), and stamps-and-reads each
  dependency in one step (LockSession covers the window between). A build's
  edges are replaced at its commit: the unlocked checker of a build in
  flight sees its Uncommitted marker first, and the re-check runs only
  under the lock, after the commit.
- Model sizes are small (1–5 targets, 2–3 processes or sessions, 1–2 edits,
  bounded crashes) but chosen adversarially; every counterexample found
  here needed at most three processes.
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

## What model checking cannot close

These are not deferred work; each is outside what a finite model checker
can establish, and the reason is given.

- **Arbitrary system size.** TLC explores finite models, so "for any number
  of processes, sessions, targets and edits" is a hypothesis (the
  small-scope hypothesis: concurrency bugs show up at small scale, and every
  bug here did). Turning it into a theorem requires a mechanical proof
  (TLAPS, installed here as `tlapm`), which has not been written yet. The one place where
  size matters structurally — nesting depth in TokenPool — is closed by the
  hand proof above from TLC-checked lemmas.
- **The Rust code itself.** TLA+ checks the design; whether
  `src/build.rs`, `src/parallel.rs` and `src/waits.rs` implement it is a separate question. The route to closing
  it is trace validation (instrument the code to emit its protocol steps
  and check recorded traces against these specs with TLC) and, for the
  in-process atomics and channels, `loom` tests of `src/parallel.rs`'s
  registry and the jobserver.

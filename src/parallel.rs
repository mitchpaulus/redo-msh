//! The aggressive parallel ensure engine, implementing the design verified
//! in `verification/Speculation.tla` (see `verification/README.md` for the
//! full implementation contract).
//!
//! The recorded dependency edges in the database are treated as a
//! parallelization plan: ensuring a target fans out over **all** of its
//! recorded deps at once, activating each dep's own check speculatively and
//! in parallel. Checking is unbounded (a thread per active target);
//! do-file runs are bounded by the jobserver token budget.
//!
//! Per-process, per-session, each target is claimed exactly once by a task
//! in the [`Registry`] (implementation contract item 4 — the cross-process
//! half of the claim is the per-target kernel lock plus the double-checked
//! runid re-read in `build::build`). A task's lifecycle mirrors the spec:
//!
//! ```text
//! claimed -> checking -> verified              (up to date)
//!                     -> building -> built     (do-file ran, committed)
//!                     -> failed
//! ```
//!
//! Failure severity (the design rule Speculation.tla pins down):
//!
//! * **Speculative failures are SOFT.** A recorded dep that fails, or a
//!   cycle among speculative wait edges, only disqualifies the verify path:
//!   the parent proceeds to run its do-file, whose `redo-ifchange` calls are
//!   the ground truth. A stale recorded cycle therefore self-heals.
//! * **Actual failures are HARD.** A dep demanded by a *running* do-file
//!   failing, or a cycle closed by a mid-build wait edge, fails the target;
//!   the error propagates up the wait edges (each waiter's `redo-ifchange`
//!   exits nonzero), releasing tokens on the way.
//!
//! Tokens: a task holds a token (the process's own token, or one from the
//! shared pool) only while `build::build` runs. A do-file blocked in
//! `redo-ifchange` still holds its parent's token, but the child redo
//! process brings a fresh own token — TokenPool.tla's verified `Bound`
//! shows the compensation is exact, so executing do-file bodies never
//! exceed J. Token acquisition is a try-acquire retry loop, which satisfies
//! the eager-scheduling obligation (contract item 7): a ready target plus a
//! token freed *anywhere* in the process tree eventually launches.

use crate::build::{self, Ctx};
use crate::jobserver::TokenSrc;
use crate::waits::{self, WaitOutcome};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// One claim per target per session, process-wide. Shared by every task
/// thread through `Ctx` (an `Arc`); settled tasks stay in the map as the
/// session's "already done" record for this process.
pub struct Registry {
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    /// Whether the process's implicit own jobserver token is in use.
    own_busy: AtomicBool,
}

impl Registry {
    pub fn new() -> Registry {
        Registry {
            tasks: Mutex::new(HashMap::new()),
            own_busy: AtomicBool::new(false),
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

/// A target's claim and settled outcome. Waiters block on the condvar; the
/// outcome is stable for the rest of the session once set.
struct Task {
    target: String,
    state: Mutex<State>,
    cv: Condvar,
}

enum State {
    Running,
    Done,
    Failed(String),
}

/// Bring one target up to date (build if out of date, else verify).
/// Blocks until the target settles; idempotent within a session.
pub fn ensure(ctx: &Ctx, target: &str) -> Result<()> {
    let handles = activate(ctx, &[target.to_string()], false)?;
    wait_task(&handles[0]).map_err(|m| anyhow!(m))
}

/// Bring a group of targets up to date in parallel, waiting for all of them
/// (even after a failure, so the process never returns while its builds are
/// still mutating the tree). `force` is the top-level `redo` semantic:
/// rebuild unconditionally. Returns the first failure.
pub fn ensure_all(ctx: &Ctx, targets: &[String], force: bool) -> Result<()> {
    let handles = activate(ctx, targets, force)?;
    let mut first_err: Option<String> = None;
    for h in &handles {
        if let Err(m) = wait_task(h) {
            if first_err.is_none() {
                build::jlog(ctx, || {
                    format!("{}: failed; waiting for the rest of the group", h.target)
                });
                first_err = Some(m);
            }
        }
    }
    match first_err {
        Some(m) => Err(anyhow!(m)),
        None => Ok(()),
    }
}

/// Claim (and start, for new claims) a task per target. All claims are
/// inserted under one registry lock *before* any thread starts, so a caller
/// activating several targets — e.g. top-level `redo a b` — can never lose
/// a claim race against speculation running on behalf of its own first
/// target.
fn activate(ctx: &Ctx, targets: &[String], force: bool) -> Result<Vec<Arc<Task>>> {
    let mut out = Vec::with_capacity(targets.len());
    let mut fresh: Vec<Arc<Task>> = Vec::new();
    {
        let mut map = ctx.tasks.tasks.lock().unwrap();
        for t in targets {
            let key = t.to_ascii_lowercase();
            if let Some(h) = map.get(&key) {
                out.push(h.clone());
            } else {
                let h = Arc::new(Task {
                    target: t.clone(),
                    state: Mutex::new(State::Running),
                    cv: Condvar::new(),
                });
                map.insert(key, h.clone());
                fresh.push(h.clone());
                out.push(h);
            }
        }
    }
    for task in fresh {
        match ctx.child_for_thread() {
            Ok(cctx) => {
                let t = task.clone();
                let spawned = thread::Builder::new()
                    .name(format!("redo:{}", task.target))
                    .spawn(move || run_task(cctx, t, force));
                if let Err(e) = spawned {
                    settle(&task, Err(anyhow!("could not spawn build thread: {e}")));
                }
            }
            Err(e) => settle(&task, Err(e)),
        }
    }
    Ok(out)
}

/// Wait for every task in this process to settle. Called before `ifchange`
/// and top-level `redo` return, so speculative work never outlives the
/// process that started it (a killed process's half-done build would be
/// safe — Uncommitted marker, kernel-released lock — but never desirable).
pub fn drain(ctx: &Ctx) {
    loop {
        let pending: Vec<Arc<Task>> = {
            let map = ctx.tasks.tasks.lock().unwrap();
            map.values()
                .filter(|t| matches!(*t.state.lock().unwrap(), State::Running))
                .cloned()
                .collect()
        };
        if pending.is_empty() {
            return;
        }
        for t in pending {
            let _ = wait_task(&t);
        }
    }
}

fn wait_task(task: &Task) -> Result<(), String> {
    let mut st = task.state.lock().unwrap();
    loop {
        match &*st {
            State::Running => st = task.cv.wait(st).unwrap(),
            State::Done => return Ok(()),
            State::Failed(m) => return Err(m.clone()),
        }
    }
}

fn settle(task: &Task, res: Result<()>) {
    let mut st = task.state.lock().unwrap();
    *st = match res {
        Ok(()) => State::Done,
        Err(e) => State::Failed(format!("{e:#}")),
    };
    task.cv.notify_all();
}

fn run_task(ctx: Ctx, task: Arc<Task>, force: bool) {
    let res = task_body(&ctx, &task.target, force);
    // A settled target no longer blocks: its wait edges leave the shared
    // graph on every outcome (verified, built, failed), including error
    // paths that broke out of the checking loop early.
    let _ = waits::clear_waiter(&ctx.conn, &task.target);
    settle(&task, res);
}

fn task_body(ctx: &Ctx, target: &str, force: bool) -> Result<()> {
    // Already built or verified this session (possibly by another process).
    if !force && build::file_runid(&ctx.conn, target)? == Some(ctx.session) {
        build::vlog(ctx, || {
            format!("{target}: skipping: already built or verified earlier in this run")
        });
        return Ok(());
    }
    let must_rebuild = force || check_recorded(ctx, target)?;
    if !must_rebuild {
        build::vlog(ctx, || {
            format!("{target}: up to date; marking it verified for this run")
        });
        return build::mark_verified(ctx, target);
    }
    // Speculative wait edges leave the graph before the build starts (the
    // spec's BeginBuild clears cedges): from here on only this target's
    // do-file — via its redo-ifchange children — inserts edges for it.
    let _ = waits::clear_waiter(&ctx.conn, target);
    let src = acquire_token(ctx, target);
    let r = build::build(ctx, target, force);
    release_token(ctx, src);
    r
}

/// The checking phase: decide whether `target` must rebuild, speculating
/// over its RECORDED deps in parallel. Returns `Ok(true)` for the rebuild
/// path (any evidence of out-of-dateness, any soft failure) and `Ok(false)`
/// only when every recorded dep settled successfully and every recorded
/// check passed — the preconditions the spec's `Verify` demands. `Err` is
/// reserved for real faults (I/O, database), which fail the task.
fn check_recorded(ctx: &Ctx, target: &str) -> Result<bool> {
    let target_abs = ctx.root.dir.join(target);
    let mut must = false;

    let built_csum = match build::files_row(&ctx.conn, target)? {
        Some(c) => c,
        None => {
            build::vlog(ctx, || {
                format!("{target}: OUT OF DATE: redo has no record of ever building it")
            });
            must = true;
            None
        }
    };
    // A previously-produced file that is now gone must be rebuilt. (A phony
    // target has no recorded csum, so its absence is expected.)
    if built_csum.is_some() && !target_abs.exists() {
        build::vlog(ctx, || {
            format!("{target}: OUT OF DATE: the previously built output is missing from disk")
        });
        must = true;
    }

    // Classify the recorded edges. Cheap verdict-only kinds are evaluated
    // inline; content checks are deferred until after the speculative
    // fan-out is launched.
    let mut dofiles: Vec<(String, Option<String>)> = Vec::new();
    let mut ifcreates: Vec<String> = Vec::new();
    let mut sources: Vec<(String, Option<String>)> = Vec::new();
    let mut buildable: Vec<(String, Option<String>)> = Vec::new();
    for (kind, dep, edge_csum) in build::read_deps(&ctx.conn, target)? {
        match kind {
            crate::db::DepKind::Always => {
                build::vlog(ctx, || {
                    format!("{target}: OUT OF DATE: its do-file called redo-always")
                });
                must = true;
            }
            crate::db::DepKind::Uncommitted => {
                build::vlog(ctx, || {
                    format!(
                        "{target}: OUT OF DATE: the last build failed or crashed \
                         before committing"
                    )
                });
                must = true;
            }
            crate::db::DepKind::DoFile => {
                dofiles.push((dep.expect("dofile dep has a path"), edge_csum));
            }
            crate::db::DepKind::IfCreate => {
                ifcreates.push(dep.expect("ifcreate dep has a path"));
            }
            crate::db::DepKind::IfChange => {
                let d = dep.expect("ifchange dep has a path");
                if build::is_target(ctx, &d)? {
                    buildable.push((d, edge_csum));
                } else {
                    sources.push((d, edge_csum));
                }
            }
        }
    }

    // THE AGGRESSIVE STEP: activate every recorded buildable dep NOW, in
    // parallel — even when this target is already known to need a rebuild.
    // The recorded graph is a parallelization plan: the do-file will most
    // likely redo-ifchange these same deps, and by then they are already
    // settling. A stale recorded dep costs wasted speculative work; it can
    // never corrupt build order (Speculation.tla, ActualDepsFirst) because
    // the do-file's own ifchange calls remain the ground truth.
    let dep_names: Vec<String> = buildable.iter().map(|(d, _)| d.clone()).collect();
    let handles = activate(ctx, &dep_names, false)?;

    if must {
        // Abandon the rest of the checking (the spec's BeginBuild under
        // mustRebuild): the activated deps keep settling in the background.
        return Ok(true);
    }

    for (dep, edge_csum) in &dofiles {
        let cur = build::current_csum(ctx, dep)?;
        if cur.as_deref() != edge_csum.as_deref() {
            build::vlog(ctx, || {
                format!(
                    "{target}: OUT OF DATE: do-file {dep} changed (hash was {} at \
                     last build, is now {})",
                    build::hash8(edge_csum.as_deref()),
                    build::hash8(cur.as_deref())
                )
            });
            return Ok(true);
        }
        build::vlog(ctx, || {
            format!(
                "{target}: do-file {dep} unchanged (hash {})",
                build::hash8(cur.as_deref())
            )
        });
    }
    for dep in &ifcreates {
        if ctx.root.dir.join(dep).exists() {
            build::vlog(ctx, || {
                format!(
                    "{target}: OUT OF DATE: {dep} now exists (the target depends on \
                     it NOT existing — at build time it was a missing do-file \
                     candidate or an ifcreate path)"
                )
            });
            return Ok(true);
        }
        build::vlog(ctx, || {
            format!("{target}: ifcreate dependency {dep} is still absent")
        });
    }
    for (dep, edge_csum) in &sources {
        match build::current_csum(ctx, dep)? {
            None => {
                build::vlog(ctx, || {
                    format!("{target}: OUT OF DATE: dependency {dep} no longer exists")
                });
                return Ok(true);
            }
            Some(cur) => {
                if Some(cur.as_str()) != edge_csum.as_deref() {
                    build::vlog(ctx, || {
                        format!(
                            "{target}: OUT OF DATE: dependency {dep} changed (hash \
                             was {} at last build, is now {})",
                            build::hash8(edge_csum.as_deref()),
                            build::hash8(Some(&cur))
                        )
                    });
                    return Ok(true);
                }
                build::vlog(ctx, || {
                    format!("{target}: dependency {dep} unchanged (hash {})", build::hash8(Some(&cur)))
                });
            }
        }
    }

    // Wait for the speculatively activated deps. Every wait is preceded by
    // the atomic cycle-check-and-insert on the shared waits-for graph; a
    // cycle here is SOFT (stale recorded edges must not invent errors), and
    // so is a dep that fails — both just force the rebuild path.
    let mut checked = dofiles.len() + ifcreates.len() + sources.len();
    for ((dep, edge_csum), handle) in buildable.iter().zip(&handles) {
        match waits::try_wait(&ctx.root, &ctx.conn, ctx.session, target, dep)? {
            WaitOutcome::Cycle => {
                build::vlog(ctx, || {
                    format!(
                        "{target}: waiting for recorded dependency {dep} would close \
                         a dependency cycle; treating it as out of date (the do-file \
                         is the ground truth for whether the cycle is real)"
                    )
                });
                return Ok(true);
            }
            WaitOutcome::Inserted => {}
        }
        if let Err(e) = wait_task(handle) {
            build::vlog(ctx, || {
                format!(
                    "{target}: OUT OF DATE: recorded dependency {dep} failed to \
                     build ({e}); rebuilding — its do-file decides whether the \
                     dependency is still real"
                )
            });
            return Ok(true);
        }
        match build::current_csum(ctx, dep)? {
            None => {
                build::vlog(ctx, || {
                    format!("{target}: OUT OF DATE: dependency {dep} no longer exists")
                });
                return Ok(true);
            }
            Some(cur) => {
                if Some(cur.as_str()) != edge_csum.as_deref() {
                    build::vlog(ctx, || {
                        format!(
                            "{target}: OUT OF DATE: dependency {dep} changed (hash \
                             was {} at last build, is now {})",
                            build::hash8(edge_csum.as_deref()),
                            build::hash8(Some(&cur))
                        )
                    });
                    return Ok(true);
                }
                build::vlog(ctx, || {
                    format!("{target}: dependency {dep} unchanged (hash {})", build::hash8(Some(&cur)))
                });
            }
        }
        checked += 1;
    }

    build::vlog(ctx, || {
        format!("{target}: UP TO DATE: all {checked} recorded dependencies are unchanged")
    });
    Ok(false)
}

/// Acquire a do-file token: the process's own token if free, else a shared
/// pool token, else retry. The retry loop (rather than waiting only on this
/// process's own completions) is what makes scheduling eager: a token
/// released by any process in the tree is observed within one interval.
/// Token acquisition never happens while holding a target lock —
/// `build::build` locks after the token is held — which is the discipline
/// TokenPool.tla's deadlock-freedom rests on.
fn acquire_token(ctx: &Ctx, target: &str) -> TokenSrc {
    if !ctx.tasks.own_busy.swap(true, Ordering::AcqRel) {
        build::jlog(ctx, || format!("{target}: build slot: own token"));
        return TokenSrc::Own;
    }
    if ctx.jobs.try_acquire() {
        build::jlog(ctx, || format!("{target}: build slot: pool token"));
        return TokenSrc::Pool;
    }
    build::jlog(ctx, || {
        format!("{target}: no token free; polling until one is released")
    });
    loop {
        thread::sleep(Duration::from_millis(10));
        if !ctx.tasks.own_busy.swap(true, Ordering::AcqRel) {
            build::jlog(ctx, || format!("{target}: build slot: own token (after wait)"));
            return TokenSrc::Own;
        }
        if ctx.jobs.try_acquire() {
            build::jlog(ctx, || format!("{target}: build slot: pool token (after wait)"));
            return TokenSrc::Pool;
        }
    }
}

fn release_token(ctx: &Ctx, src: TokenSrc) {
    match src {
        TokenSrc::Own => ctx.tasks.own_busy.store(false, Ordering::Release),
        TokenSrc::Pool => ctx.jobs.release(),
    }
}

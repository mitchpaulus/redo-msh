//! The aggressive parallel ensure engine, implementing the machine verified
//! in `verification/SpeculationMP.tla` (see `verification/README.md` for
//! the full rules R1–R6; R6, the overwrite prompt's lineage rule, lives in
//! `build::abort_unless_lineage_demanded`).
//!
//! The recorded dependency edges in the database are treated as a
//! parallelization plan: ensuring a target fans out over its recorded deps,
//! activating each dep's own check speculatively and in parallel. Checking
//! is unbounded (a thread per active target); do-file runs are bounded by
//! the jobserver token budget.
//!
//! Per-process, per-session, each target is claimed exactly once by a task
//! in the [`Registry`]; the cross-process half of the claim is the
//! per-target kernel lock plus the double-checked runid re-read in
//! `build::build`. A task carries a **grade** (rule R4): *demanded* (a real
//! `redo-ifchange` or top-level `redo` asked for it) or *speculative*
//! (spawned from recorded edges on spec). The grade decides how failure is
//! reported:
//!
//! * a demanded task's failure is a real error (propagated to the caller);
//! * a speculative task's failure — including an abort — settles as
//!   [`State::SpecFailed`]: quarantined, reported to no one, and RECLAIMED
//!   (re-run) if a real demand arrives later.
//!
//! Every wait a task performs is edge-guarded in the shared waits-for graph
//! (rule R1, `waits.rs`):
//!
//! * spawning a speculative task inserts a **creation edge** `ctx -> dep`
//!   (ctx = this process's `REDO_TARGET`), atomically refused if it would
//!   close a cycle — speculation that could deadlock never starts, and the
//!   refusal just forces the checker onto the rebuild path;
//! * a checker waiting on a dep task first inserts a **checker edge**;
//! * both edge kinds are SOFT and may be *evicted* by a hard demand whose
//!   cycle rides them (rule R3). Waits are therefore interruptible: they
//!   poll their own edge and, for speculative lineages, the watch list
//!   (`Ctx::spec_watch`) — a vanished edge means "yield: stop waiting
//!   (checker) or abort this speculative build (creation)".
//!
//! Tokens: a task holds a token only while `build::build` runs. A do-file
//! blocked in `redo-ifchange` still holds its parent's token, but the child
//! redo process brings a fresh own token — TokenPool.tla's verified `Bound`
//! shows the compensation is exact. Token acquisition is a try-acquire
//! retry loop (eager scheduling: a token freed anywhere in the tree is
//! observed within one interval).

use crate::build::{self, Ctx};
use crate::jobserver::TokenSrc;
use crate::waits::{self, EdgeRef, SoftOutcome};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// How often blocked waits re-check their edge / watch for eviction.
const POLL: Duration = Duration::from_millis(50);

/// The error a speculative lineage unwinds with once it is dead (evicted,
/// abandoned, or aborted from below by rule R6). Typed so that `settle`
/// can quarantine it WHATEVER the task's grade says by then: the grade is
/// a racy flag, and a demand that upgrades the task after its lineage was
/// killed but before it settled would otherwise turn "no longer needed" into
/// a reported failure. Quarantined, the demand reclaims and re-runs it
/// (`ensure_all`) — the model's atomic abort-or-upgrade, restored.
#[derive(Debug)]
pub struct SpecAborted(pub String);

impl std::fmt::Display for SpecAborted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "speculation aborted: {}", self.0)
    }
}

impl std::error::Error for SpecAborted {}

/// One claim per target per session, process-wide. Shared by every task
/// thread through `Ctx` (an `Arc`); settled tasks stay in the map as the
/// session's "already done" record for this process (a quarantined
/// speculative failure is replaced on reclaim).
pub struct Registry {
    tasks: Mutex<HashMap<String, Arc<Task>>>,
    /// Whether the process's implicit own jobserver token is in use.
    own_busy: AtomicBool,
    /// The PROCESS-level abort watch (from `REDO_SPEC_WATCH`): the creation
    /// edges guarding this whole process's speculative lineage. Task threads
    /// build their watch from this base (never from the spawning thread's
    /// extended watch — a sibling speculative task's fate is its own).
    env_watch: Vec<EdgeRef>,
    /// Parks token waiters. A release wakes exactly ONE waiter
    /// (`notify_one`); the wait's timeout covers tokens freed elsewhere in
    /// the process tree, which cannot notify us. Without this, hundreds of
    /// queued tasks each poll every 10ms for the whole build — a
    /// super-linear scheduler churn that measured ~40% on wide builds.
    token_mx: Mutex<()>,
    token_cv: Condvar,
    /// Idle read connections recycled between task threads. Tasks are
    /// mostly short-lived and heavily overlapped in *time* but not in
    /// *concurrency* (a wide scan peaks at a handful of live tasks), so a
    /// small pool replaces hundreds of connection opens — each of which
    /// costs pragmas, page-cache warmup, and file locking.
    conn_pool: Mutex<Vec<rusqlite::Connection>>,
}

/// Idle connections kept for reuse; beyond this they are simply closed.
const CONN_POOL_MAX: usize = 64;

impl Registry {
    pub fn new(env_watch: Vec<EdgeRef>) -> Registry {
        Registry {
            tasks: Mutex::new(HashMap::new()),
            own_busy: AtomicBool::new(false),
            env_watch,
            token_mx: Mutex::new(()),
            token_cv: Condvar::new(),
            conn_pool: Mutex::new(Vec::new()),
        }
    }

    /// Take an idle pooled connection, if any.
    pub(crate) fn take_conn(&self) -> Option<rusqlite::Connection> {
        self.conn_pool.lock().unwrap().pop()
    }

    /// Return a connection to the pool for the next task thread.
    pub(crate) fn return_conn(&self, conn: rusqlite::Connection) {
        let mut pool = self.conn_pool.lock().unwrap();
        if pool.len() < CONN_POOL_MAX {
            pool.push(conn);
        }
    }
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// The creation-edge waiter name this process uses for speculative spawns:
/// its context target, or — for a top-level process, which has no name
/// anyone could wait on — a per-process sentinel that nothing can reach
/// (so it can never be on a cycle) but that still gives the speculation an
/// evictable/abandonable edge.
fn creation_waiter(ctx: &Ctx) -> String {
    match &ctx.target {
        Some(t) => t.clone(),
        None => format!("\u{1}top:{}", waits::owner_id()),
    }
}

/// A target's claim and settled outcome. Waiters block on the condvar; the
/// outcome is stable once set (a `SpecFailed` entry may be superseded by a
/// fresh task in the registry on reclaim, but the old task never changes).
struct Task {
    target: String,
    /// Grade (rule R4): true once any real demand asked for this target.
    /// Shared (via `Ctx::demanded`) with the task's own thread, which uses
    /// it to gate own-token eligibility.
    demanded: Arc<AtomicBool>,
    /// True once a checker blocks on this task's result — it is then on a
    /// critical path and may use the own token even while speculative.
    wanted: Arc<AtomicBool>,
    /// Rule R5: set by the drain to tell a still-speculative task to stop
    /// (its thread polls this and kills its running do-file, if any).
    abandon: Arc<AtomicBool>,
    state: Mutex<State>,
    cv: Condvar,
}

enum State {
    Running,
    Done,
    /// A demanded build failed: a real, reported error.
    Failed(String),
    /// A speculative build failed or was aborted: quarantined (never
    /// reported), reclaimable by a later real demand.
    SpecFailed(String),
}

/// A settled task outcome, as observed by a waiter.
enum Outcome {
    Done,
    Failed(String),
    SpecFailed(String),
}

/// Bring one target up to date (build if out of date, else verify).
/// Blocks until the target settles; idempotent within a session.
pub fn ensure(ctx: &Ctx, target: &str) -> Result<()> {
    ensure_all(ctx, &[target.to_string()], false)
}

/// Bring a group of demanded targets up to date in parallel, waiting for
/// all of them (even after a failure, so the process never returns while
/// its builds are still mutating the tree). `force` is the top-level
/// `redo` semantic: rebuild unconditionally. Returns the first failure.
/// A target whose speculative attempt was quarantined is reclaimed and
/// re-run — a real demand never adopts a speculative failure (rule R4).
pub fn ensure_all(ctx: &Ctx, targets: &[String], force: bool) -> Result<()> {
    let handles = activate_demanded(ctx, targets, force)?;
    let mut first_err: Option<String> = None;
    for (i, mut h) in handles.into_iter().enumerate() {
        let mut reclaims = 0;
        let res = loop {
            match wait_task(&h) {
                Outcome::Done => break Ok(()),
                Outcome::Failed(m) => break Err(m),
                Outcome::SpecFailed(m) => {
                    // No point re-running anything if THIS process's own
                    // lineage is dead: we are unwinding too (rule R6's
                    // abort-from-below lands here first, in the dying
                    // process's own ifchange).
                    if speculation_dead(ctx)? {
                        break Err(m);
                    }
                    reclaims += 1;
                    if reclaims > 2 {
                        break Err(m);
                    }
                    build::jlog(ctx, || {
                        format!(
                            "{}: speculative attempt was quarantined ({m}); \
                             re-running it as a demanded build",
                            targets[i]
                        )
                    });
                    h = reclaim(ctx, &targets[i], force)?;
                }
            }
        };
        if let Err(m) = res {
            if first_err.is_none() {
                build::jlog(ctx, || {
                    format!("{}: failed; waiting for the rest of the group", targets[i])
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

/// Claim (and start, for new claims) a DEMANDED task per target. All
/// claims are inserted under one registry lock *before* any thread starts,
/// so a caller activating several targets — e.g. top-level `redo a b` —
/// can never lose a claim race against speculation running on behalf of
/// its own first target. An existing speculative claim is upgraded in
/// place (its DB creation edge was already superseded by the caller's
/// demand edge); an existing quarantined failure is reclaimed.
fn activate_demanded(ctx: &Ctx, targets: &[String], force: bool) -> Result<Vec<Arc<Task>>> {
    let mut out = Vec::with_capacity(targets.len());
    let mut fresh: Vec<Arc<Task>> = Vec::new();
    {
        let mut map = ctx.tasks.tasks.lock().unwrap();
        for t in targets {
            let key = t.to_ascii_lowercase();
            let reuse = match map.get(&key) {
                Some(h) if matches!(*h.state.lock().unwrap(), State::SpecFailed(_)) => None,
                Some(h) => {
                    // A top-level process inserts no demand edges of its own
                    // (nothing can wait on it), so nothing has superseded
                    // this task's sentinel creation edge yet. Upgrade it
                    // here: rule R6's abort-from-below matches on edge kind,
                    // and this is what makes abort-or-upgrade atomic at the
                    // top level too. The sentinel is never anyone's dep, so
                    // the demand insert cannot see a cycle.
                    if ctx.target.is_none() && !h.demanded.load(Ordering::Acquire) {
                        let waiter = creation_waiter(ctx);
                        let _ = waits::try_demand(&ctx.root, &ctx.conn, &waiter, t)?;
                    }
                    h.demanded.store(true, Ordering::Release);
                    Some(h.clone())
                }
                None => None,
            };
            match reuse {
                Some(h) => out.push(h),
                None => {
                    let h = new_task(t, true);
                    map.insert(key, h.clone());
                    fresh.push(h.clone());
                    out.push(h);
                }
            }
        }
    }
    for task in fresh {
        spawn_task(ctx, task, force, true);
    }
    Ok(out)
}

/// Reclaim a quarantined speculative failure under a real demand: replace
/// the settled task with a fresh demanded one (unless another demander
/// already did).
fn reclaim(ctx: &Ctx, target: &str, force: bool) -> Result<Arc<Task>> {
    let (h, fresh) = {
        let mut map = ctx.tasks.tasks.lock().unwrap();
        let key = target.to_ascii_lowercase();
        match map.get(&key) {
            Some(h) if !matches!(*h.state.lock().unwrap(), State::SpecFailed(_)) => {
                (h.clone(), false)
            }
            _ => {
                let h = new_task(target, true);
                map.insert(key, h.clone());
                (h, true)
            }
        }
    };
    if fresh {
        spawn_task(ctx, h.clone(), force, true);
    }
    Ok(h)
}

/// Claim (and start) SPECULATIVE tasks for recorded deps. Each fresh spawn
/// is guarded by the atomic creation-edge check-and-insert (rules R1/R3):
/// `None` means the spawn was refused because waiting for it could close a
/// cycle — the caller treats that as a soft failure (rebuild path). An
/// already-claimed target is shared as-is (whoever claimed it holds the
/// covering edge).
fn activate_spec(ctx: &Ctx, deps: &[String]) -> Result<Vec<Option<Arc<Task>>>> {
    let mut out: Vec<Option<Arc<Task>>> = Vec::with_capacity(deps.len());
    let mut fresh: Vec<Arc<Task>> = Vec::new();
    {
        let mut map = ctx.tasks.tasks.lock().unwrap();
        // Partition into already-claimed (shared as-is) and fresh; the
        // fresh spawns' creation edges go in as ONE batched transaction.
        let mut fresh_names: Vec<String> = Vec::new();
        let mut slots: Vec<Option<Arc<Task>>> = Vec::with_capacity(deps.len());
        for d in deps {
            match map.get(&d.to_ascii_lowercase()) {
                Some(h) => slots.push(Some(h.clone())),
                None => {
                    fresh_names.push(d.clone());
                    slots.push(None);
                }
            }
        }
        let waiter = creation_waiter(ctx);
        let outcomes =
            waits::try_soft_batch(&ctx.root, &ctx.conn, &waiter, &fresh_names, waits::EdgeKind::Creation)?;
        let mut oc = outcomes.into_iter();
        let mut fresh_it = fresh_names.into_iter();
        for slot in slots {
            match slot {
                Some(h) => out.push(Some(h)),
                None => {
                    let d = fresh_it.next().expect("one name per empty slot");
                    match oc.next().expect("one outcome per fresh name") {
                        SoftOutcome::Cycle => {
                            build::vlog(ctx, || {
                                format!(
                                    "{d}: NOT speculating: waiting for it here could \
                                     close a dependency cycle (creation edge refused)"
                                )
                            });
                            out.push(None);
                        }
                        SoftOutcome::Inserted => {
                            let h = new_task(&d, false);
                            map.insert(d.to_ascii_lowercase(), h.clone());
                            fresh.push(h.clone());
                            out.push(Some(h));
                        }
                    }
                }
            }
        }
    }
    for task in fresh {
        spawn_task(ctx, task, false, false);
    }
    Ok(out)
}

fn new_task(target: &str, demanded: bool) -> Arc<Task> {
    Arc::new(Task {
        target: target.to_string(),
        demanded: Arc::new(AtomicBool::new(demanded)),
        wanted: Arc::new(AtomicBool::new(false)),
        abandon: Arc::new(AtomicBool::new(false)),
        state: Mutex::new(State::Running),
        cv: Condvar::new(),
    })
}

impl Task {
    fn demanded_flag(&self) -> Arc<AtomicBool> {
        self.demanded.clone()
    }
}

/// Start a task's worker thread. Watches are built from the PROCESS-level
/// base (`Registry::env_watch`), never the spawning thread's extended
/// watch: a sibling speculative task's abort must not cascade here. A
/// speculative task additionally gets:
/// * an EMPTY ancestor chain — the chain is do-file call-stack lineage and
///   a speculative traversal is not on that call stack (inheriting it
///   fabricated `X -> g -> X` cycle errors); real cycles are the
///   waits-for graph's job;
/// * its own creation edge appended to its watch, plus the abandon flag
///   (rule R5), so every blocking primitive underneath — including the
///   running do-file's poll-wait — can observe an abort or abandonment.
fn spawn_task(ctx: &Ctx, task: Arc<Task>, force: bool, demanded: bool) {
    match ctx.child_for_thread() {
        Ok(mut cctx) => {
            cctx.spec_watch = ctx.tasks.env_watch.clone();
            cctx.demanded = Some(task.demanded_flag());
            cctx.wanted = Some(task.wanted.clone());
            if !demanded {
                cctx.chain = Vec::new();
                cctx.spec_watch.push(EdgeRef {
                    owner: waits::owner_id().to_string(),
                    waiter: creation_waiter(ctx).to_ascii_lowercase(),
                    dep: task.target.to_ascii_lowercase(),
                });
                cctx.abandon = Some(task.abandon.clone());
            }
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

/// Wait for every task in this process to settle, ABANDONING the ones
/// still speculative (SpeculationMP rule R5): speculation that nobody
/// demanded by the time this process is done cannot hold its return
/// hostage. Abandonment sets the task's flag and deletes its creation
/// edge, so the task's own polls — including the poll-wait on a running
/// do-file, which is killed — unwind it promptly; it settles quarantined
/// (`SpecFailed`) and is re-run whenever a real demand arrives. Called
/// before `ifchange` and top-level `redo` return, so speculative work
/// never outlives the process that started it. Deadlock-free because
/// every remaining in-flight task is demanded and edge-guarded (rule R1).
pub fn drain(ctx: &Ctx) {
    let waiter = creation_waiter(ctx).to_ascii_lowercase();
    loop {
        let pending: Vec<Arc<Task>> = {
            let map = ctx.tasks.tasks.lock().unwrap();
            map.values()
                .filter(|t| matches!(*t.state.lock().unwrap(), State::Running))
                .cloned()
                .collect()
        };
        if pending.is_empty() {
            // Completed speculative tasks' creation edges (and the top
            // sentinel's, if any) no longer guard anything.
            let _ = waits::clear_waiter(&ctx.conn, &waiter);
            return;
        }
        for t in &pending {
            if !t.demanded.load(Ordering::Acquire)
                && !t.abandon.swap(true, Ordering::AcqRel)
            {
                build::jlog(ctx, || {
                    format!(
                        "{}: abandoning speculative task at drain (undemanded; \
                         will be re-run when genuinely needed)",
                        t.target
                    )
                });
                // Delete the creation edge so child processes of this
                // speculative lineage (watching it via REDO_SPEC_WATCH)
                // unwind too. Guarded on kind: an upgrade race must not
                // delete a demand edge.
                let _ = ctx.conn.execute(
                    "DELETE FROM waits WHERE owner = ?1 AND waiter = ?2 \
                     AND dep = ?3 AND kind = 2",
                    rusqlite::params![
                        waits::owner_id(),
                        &waiter,
                        t.target.to_ascii_lowercase()
                    ],
                );
            }
        }
        // Abandoned tasks parked in the token wait wake promptly.
        ctx.tasks.token_cv.notify_all();
        for t in pending {
            let _ = wait_task(&t);
        }
    }
}

fn wait_task(task: &Task) -> Outcome {
    let mut st = task.state.lock().unwrap();
    loop {
        match &*st {
            State::Running => st = task.cv.wait(st).unwrap(),
            State::Done => return Outcome::Done,
            State::Failed(m) => return Outcome::Failed(m.clone()),
            State::SpecFailed(m) => return Outcome::SpecFailed(m.clone()),
        }
    }
}

/// What an edge-guarded, interruptible checker wait observed.
enum WatchedOutcome {
    Done,
    /// The dep failed (hard or quarantined): soft for the checker either
    /// way — it just takes the rebuild path.
    SoftFailed(String),
    /// Our checker edge was evicted by a hard demand (rule R3): stop
    /// waiting and take the rebuild path.
    Evicted,
}

/// Wait on a dep task under our checker edge, polling for eviction and for
/// lineage abort. Errors only for real faults (I/O) or an aborted
/// speculative lineage (which fails this task, quarantined by grade).
fn wait_task_watched(ctx: &Ctx, task: &Task, edge: &EdgeRef) -> Result<WatchedOutcome> {
    // This task's result is now on our critical path: it may use the own
    // token even while speculative (and if it was parked waiting for one,
    // wake it).
    if !task.wanted.swap(true, Ordering::AcqRel) {
        ctx.tasks.token_cv.notify_all();
    }
    loop {
        {
            let mut st = task.state.lock().unwrap();
            loop {
                match &*st {
                    State::Done => return Ok(WatchedOutcome::Done),
                    State::Failed(m) => return Ok(WatchedOutcome::SoftFailed(m.clone())),
                    State::SpecFailed(m) => {
                        return Ok(WatchedOutcome::SoftFailed(m.clone()))
                    }
                    State::Running => {}
                }
                let (g, timeout) = task.cv.wait_timeout(st, POLL).unwrap();
                st = g;
                if timeout.timed_out() {
                    break;
                }
            }
        }
        // Timed out while the dep still runs: poll our edge and our watch.
        if !waits::edge_alive(&ctx.conn, edge)? {
            return Ok(WatchedOutcome::Evicted);
        }
        abort_check(ctx)?;
    }
}

/// Fail fast if this speculative work is dead: its task was abandoned at a
/// drain (rule R5), or a watched creation edge vanished (evicted by a real
/// demand, rule R3). No-op for non-speculative contexts.
pub(crate) fn abort_check(ctx: &Ctx) -> Result<()> {
    if speculation_dead(ctx)? {
        return Err(SpecAborted(
            "no longer needed here (it will be re-run when genuinely needed)".into(),
        )
        .into());
    }
    Ok(())
}

/// The boolean form of [`abort_check`], for callers that need to clean up
/// (kill a child process) before failing.
pub(crate) fn speculation_dead(ctx: &Ctx) -> Result<bool> {
    if let Some(flag) = &ctx.abandon {
        if flag.load(Ordering::Acquire) {
            return Ok(true);
        }
    }
    if !ctx.spec_watch.is_empty() && waits::watch_dead(&ctx.conn, &ctx.spec_watch)? {
        return Ok(true);
    }
    Ok(false)
}

fn settle(task: &Task, res: Result<()>) {
    let mut st = task.state.lock().unwrap();
    *st = match res {
        Ok(()) => State::Done,
        Err(e) => {
            let msg = format!("{e:#}");
            let aborted = e.downcast_ref::<SpecAborted>().is_some();
            if task.demanded.load(Ordering::Acquire) && !aborted {
                State::Failed(msg)
            } else {
                // Rule R4: speculative outcomes are quarantined, whatever
                // the cause (abort, dep failure, do-file error).
                State::SpecFailed(msg)
            }
        }
    };
    task.cv.notify_all();
}

fn run_task(ctx: Ctx, task: Arc<Task>, force: bool) {
    let mut edges_inserted = false;
    let res = task_body(&ctx, &task.target, force, &mut edges_inserted);
    // A settled target no longer blocks: its wait edges leave the shared
    // graph on every outcome (verified, built, failed), including error
    // paths that broke out of the checking loop early. Skipped when this
    // task never inserted any — a DELETE still costs the database write
    // lock, and most tasks (leaves, forced builds) have no edges.
    if edges_inserted {
        let _ = waits::clear_waiter(&ctx.conn, &task.target);
    }
    settle(&task, res);
    // Recycle this thread's connection for the next task.
    let registry = ctx.tasks.clone();
    registry.return_conn(ctx.conn);
}

fn task_body(
    ctx: &Ctx,
    target: &str,
    force: bool,
    edges_inserted: &mut bool,
) -> Result<()> {
    // Already built or verified this session (possibly by another process).
    if !force && build::file_runid(&ctx.conn, target)? == Some(ctx.session) {
        build::vlog(ctx, || {
            format!("{target}: skipping: already built or verified earlier in this run")
        });
        return Ok(());
    }
    let must_rebuild = force || check_recorded(ctx, target, edges_inserted)?;
    if !must_rebuild {
        build::vlog(ctx, || {
            format!("{target}: up to date; marking it verified for this run")
        });
        return build::mark_verified(ctx, target);
    }
    // Checker wait edges leave the graph before the build starts (the
    // spec's MoveToBuild): from here on only this target's do-file — via
    // its redo-ifchange children — inserts edges for it.
    if *edges_inserted {
        let _ = waits::clear_waiter(&ctx.conn, target);
        *edges_inserted = false;
    }
    let src = acquire_token(ctx, target)?;
    let r = build::build(ctx, target, force);
    release_token(ctx, src);
    r
}

/// The checking phase: decide whether `target` must rebuild, speculating
/// over its RECORDED deps in parallel. Returns `Ok(true)` for the rebuild
/// path (any evidence of out-of-dateness, any soft failure, any refused or
/// evicted wait) and `Ok(false)` only when every recorded dep settled
/// successfully, every recorded check passed, and every wait edge survived
/// unevicted — the preconditions the spec's `Verify` demands. `Err` is
/// reserved for real faults (I/O, database, lineage abort), which fail the
/// task.
fn check_recorded(ctx: &Ctx, target: &str, edges_inserted: &mut bool) -> Result<bool> {
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
    // settling. Each fresh spawn is guarded by the creation-edge cycle
    // check (SpeculationMP rule R3): a refused spawn is a soft failure —
    // speculation that could close a cycle never starts.
    let dep_names: Vec<String> = buildable.iter().map(|(d, _)| d.clone()).collect();
    let handles = activate_spec(ctx, &dep_names)?;
    if handles.iter().any(|h| h.is_none()) {
        must = true;
    }

    if must {
        // Abandon the rest of the checking (the spec's MoveToBuild under
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

    // Wait for the speculatively activated deps. Every wait is covered by
    // our checker edges, all inserted up front in ONE batched atomic
    // check-and-insert (inserting an edge earlier than the wait it guards
    // is safe — it only declares the wait sooner); a refusal, an eviction,
    // and a failed dep are all SOFT (stale recorded edges must never
    // invent errors) — they just force the rebuild path.
    let mut checked = dofiles.len() + ifcreates.len() + sources.len();
    let dep_only: Vec<String> = buildable.iter().map(|(d, _)| d.clone()).collect();
    let outcomes =
        waits::try_soft_batch(&ctx.root, &ctx.conn, target, &dep_only, waits::EdgeKind::Checker)?;
    *edges_inserted = outcomes.iter().any(|o| *o == SoftOutcome::Inserted);
    if let Some(pos) = outcomes.iter().position(|o| *o == SoftOutcome::Cycle) {
        let dep = &dep_only[pos];
        build::vlog(ctx, || {
            format!(
                "{target}: waiting for recorded dependency {dep} would close \
                 a dependency cycle; treating it as out of date (the do-file \
                 is the ground truth for whether the cycle is real)"
            )
        });
        return Ok(true);
    }
    let mut inserted: Vec<EdgeRef> = Vec::with_capacity(buildable.len());
    for ((dep, edge_csum), handle) in buildable.iter().zip(&handles) {
        let handle = handle.as_ref().expect("refusals returned early via `must`");
        let edge = EdgeRef {
            owner: waits::owner_id().to_string(),
            waiter: target.to_ascii_lowercase(),
            dep: dep.to_ascii_lowercase(),
        };
        match wait_task_watched(ctx, handle, &edge)? {
            WatchedOutcome::Done => {}
            WatchedOutcome::SoftFailed(e) => {
                build::vlog(ctx, || {
                    format!(
                        "{target}: OUT OF DATE: recorded dependency {dep} failed to \
                         build ({e}); rebuilding — its do-file decides whether the \
                         dependency is still real"
                    )
                });
                return Ok(true);
            }
            WatchedOutcome::Evicted => {
                build::vlog(ctx, || {
                    format!(
                        "{target}: OUT OF DATE: a running do-file's dependency \
                         demand superseded our speculative wait on {dep} (edge \
                         evicted); rebuilding"
                    )
                });
                return Ok(true);
            }
        }
        inserted.push(edge);
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

    // Missed-eviction guard: verifying is only sound if every checker edge
    // we inserted is still standing (an eviction we never observed while
    // between waits means a demand needed us on the rebuild path — match
    // the spec's sticky mustRebuild).
    for e in &inserted {
        if !waits::edge_alive(&ctx.conn, e)? {
            build::vlog(ctx, || {
                format!(
                    "{target}: OUT OF DATE: a checker wait edge was evicted by a \
                     real dependency demand; rebuilding instead of verifying"
                )
            });
            return Ok(true);
        }
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
/// TokenPool.tla's deadlock-freedom rests on. A speculative lineage checks
/// its abort watch while it waits.
fn acquire_token(ctx: &Ctx, target: &str) -> Result<TokenSrc> {
    // Speculation only consumes SURPLUS parallelism: a still-speculative
    // task may take pool tokens but never the process's own token (which
    // the demanded pipeline needs for forward progress). The gate is
    // re-read every attempt — an upgrade mid-wait unlocks the own token.
    let own_ok = |ctx: &Ctx| {
        ctx.demanded.as_ref().map_or(true, |d| d.load(Ordering::Acquire))
            || ctx.wanted.as_ref().map_or(false, |w| w.load(Ordering::Acquire))
    };
    if own_ok(ctx) && !ctx.tasks.own_busy.swap(true, Ordering::AcqRel) {
        build::jlog(ctx, || format!("{target}: build slot: own token"));
        return Ok(TokenSrc::Own);
    }
    if ctx.jobs.try_acquire() {
        build::jlog(ctx, || format!("{target}: build slot: pool token"));
        return Ok(TokenSrc::Pool);
    }
    build::jlog(ctx, || {
        format!("{target}: no token free; polling until one is released")
    });
    // Park until an in-process release wakes us (one waiter per token) or
    // the timeout fires. The timeout is deliberately long: in-process
    // handoff — the hot path — is notification-driven, and the timeout
    // only covers tokens freed by OTHER processes in the tree (whose pipe
    // writes cannot notify us) and eviction/abandon signals, both of which
    // tolerate a poll interval.
    loop {
        {
            let g = ctx.tasks.token_mx.lock().unwrap();
            let _ = ctx
                .tasks
                .token_cv
                .wait_timeout(g, Duration::from_millis(50))
                .unwrap();
        }
        abort_check(ctx)?;
        if own_ok(ctx) && !ctx.tasks.own_busy.swap(true, Ordering::AcqRel) {
            build::jlog(ctx, || format!("{target}: build slot: own token (after wait)"));
            return Ok(TokenSrc::Own);
        }
        if ctx.jobs.try_acquire() {
            build::jlog(ctx, || format!("{target}: build slot: pool token (after wait)"));
            return Ok(TokenSrc::Pool);
        }
    }
}

fn release_token(ctx: &Ctx, src: TokenSrc) {
    match src {
        TokenSrc::Own => ctx.tasks.own_busy.store(false, Ordering::Release),
        TokenSrc::Pool => ctx.jobs.release(),
    }
    ctx.tasks.token_cv.notify_one();
}

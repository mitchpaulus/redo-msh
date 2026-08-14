//! The serial build core (M2).
//!
//! Process model (peer processes): a `redo-msh` invocation runs a target's
//! do-file via `msh`. The do-file calls back with `redo-msh ifchange ...`,
//! which is a *new* `redo-msh` process that builds the dependency in-process
//! (running its own do-file via `msh`) and records the dependency edge. State
//! and session flow between processes through the `REDO_*` environment and the
//! shared SQLite database.
//!
//! Out-of-date logic is intentionally minimal here (M3 adds the real engine):
//! a top-level `redo` forces its targets, and `ifchange` builds a dependency
//! unless it was already built in *this* session (`runid`), which prevents a
//! shared dependency from being built more than once per run.

use crate::db::{self, DepKind};
use crate::dofile;
use crate::jobserver::Jobserver;
use crate::root::Root;
use crate::stamp::{self, Stamp};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::fs;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

/// Per-process build context, derived from the environment (for child
/// `ifchange`/`always` processes) or constructed fresh (top-level `redo`).
pub struct Ctx {
    pub root: Root,
    pub conn: Connection,
    /// Monotonic build counter shared across the whole invocation tree.
    pub session: i64,
    /// Indentation depth for logging.
    pub depth: usize,
    /// Ancestor target chain (root-relative) for cross-process cycle detection.
    pub chain: Vec<String>,
    /// The target whose do-file we are currently inside (`REDO_TARGET`), if any.
    pub target: Option<String>,
    /// The in-process task registry: one claim per target per session (the
    /// parallel ensure engine, see `parallel.rs`). Also replaces the old
    /// per-traversal cycle set — cycles are detected on the shared
    /// waits-for graph (`waits.rs`).
    pub tasks: Arc<crate::parallel::Registry>,
    /// Shared concurrency limiter for parallel builds.
    pub jobs: Arc<Jobserver>,
    /// Per-project interpreter configuration (`redo.toml`).
    pub config: crate::config::Config,
    /// Where this process's trace events (`do`/`waiting`) go: the enclosing
    /// target's log (child processes, from `REDO_LOG_PATH`) or the run trace
    /// (top-level `redo`). `None` (standalone `redo-ifchange` outside any
    /// build) disables log capture entirely — do-files inherit stderr.
    pub log_sink: Option<PathBuf>,
    /// The creation edges guarding this SPECULATIVE lineage (SpeculationMP
    /// rule R3): if any of them disappears from the waits graph, a real
    /// dependency demand has superseded this speculation and every blocking
    /// primitive here must abort (quarantined, retryable — rule R4). Empty
    /// for demanded work. Inherited by child redo processes through
    /// `REDO_SPEC_WATCH`; task threads rebuild it from the process base
    /// plus their own creation edge.
    pub spec_watch: Vec<crate::waits::EdgeRef>,
    /// Rule R5: the abandon flag of the speculative task this thread runs
    /// (set by the drain to cancel it). `None` for demanded work and for
    /// process-level contexts.
    pub abandon: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The live grade of the task this thread runs (shared with the
    /// registry entry, flipped true on upgrade). `None` = demanded from
    /// birth. Speculation may only consume SURPLUS parallelism: while this
    /// is `Some(false)` and nobody waits on the result (`wanted`), the
    /// task must not take the process's own token — stealing the last
    /// token would make the demanded pipeline wait behind speculation,
    /// resurrecting the hostage latency R5 exists to kill.
    pub demanded: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// True once a checker blocks on this task's result: its settle is now
    /// on somebody's critical path, so it may use the own token like
    /// demanded work.
    pub wanted: Option<Arc<std::sync::atomic::AtomicBool>>,
}

const E_ROOT: &str = "REDO_ROOT";
const E_TARGET: &str = "REDO_TARGET";
const E_SESSION: &str = "REDO_SESSION";
const E_DEPTH: &str = "REDO_DEPTH";
const E_CHAIN: &str = "REDO_CHAIN";

impl Ctx {
    /// Build a context for a top-level `redo` invocation: discover the root,
    /// start a fresh session, and initialize the jobserver for `j` parallel
    /// jobs.
    pub fn top_level(j: usize) -> Result<Ctx> {
        crate::winjob::setup();
        let cwd = std::env::current_dir()?;
        let root = Root::discover(&cwd)?;
        let conn = root.open_db()?;
        let session = db::next_runid(&conn)?;
        let config = crate::config::Config::load(&root.dir)?;
        let ctx = Ctx {
            root,
            conn,
            session,
            depth: 0,
            chain: Vec::new(),
            target: None,
            tasks: Arc::new(crate::parallel::Registry::new(Vec::new())),
            jobs: Arc::new(Jobserver::init_top(j.max(1))),
            config,
            log_sink: None, // `redo()` installs the run trace before building.
            spec_watch: Vec::new(),
            abandon: None,
            demanded: None,
            wanted: None,
        };
        jlog(&ctx, || {
            format!(
                "jobserver: created for -j{}: {}",
                j.max(1),
                ctx.jobs.describe()
            )
        });
        Ok(ctx)
    }

    /// Build a context for a child process (`ifchange`/`ifcreate`/`always`),
    /// inheriting session/chain/target from the environment. Falls back to a
    /// fresh top-level context when invoked outside a build.
    pub fn from_env() -> Result<Ctx> {
        let root = match std::env::var_os(E_ROOT) {
            Some(p) => Root {
                dir: PathBuf::from(p),
            },
            None => return Ctx::top_level(1),
        };
        let conn = root.open_db()?;
        let session = match std::env::var(E_SESSION).ok().and_then(|s| s.parse().ok()) {
            Some(s) => s,
            None => db::next_runid(&conn)?,
        };
        let depth = std::env::var(E_DEPTH)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let chain = std::env::var(E_CHAIN)
            .ok()
            .map(|s| s.split('\n').filter(|x| !x.is_empty()).map(String::from).collect())
            .unwrap_or_default();
        let target = std::env::var(E_TARGET).ok().filter(|s| !s.is_empty());
        let config = crate::config::Config::load(&root.dir)?;
        let env_watch = std::env::var(crate::waits::ENV_SPEC_WATCH)
            .map(|s| crate::waits::watch_from_env(&s))
            .unwrap_or_default();
        let ctx = Ctx {
            root,
            conn,
            session,
            depth,
            chain,
            target,
            tasks: Arc::new(crate::parallel::Registry::new(env_watch.clone())),
            jobs: Arc::new(Jobserver::from_env()),
            config,
            log_sink: std::env::var_os(crate::logs::ENV_LOG_PATH).map(PathBuf::from),
            spec_watch: env_watch,
            abandon: None,
            demanded: None,
            wanted: None,
        };
        jlog(&ctx, || match std::env::var(crate::jobserver::ENV) {
            Ok(spec) => format!(
                "jobserver: inherited (REDO_JOBSERVER={spec}): {}",
                ctx.jobs.describe()
            ),
            Err(_) => "jobserver: none in environment: serial (own token only)".to_string(),
        });
        Ok(ctx)
    }

    /// A context for a task worker thread: same root/session/chain/target and
    /// shared registry + jobserver, but its own SQLite connection (rusqlite
    /// connections are not shareable across threads; idle ones are recycled
    /// through the registry's pool — `run_task` returns them).
    pub(crate) fn child_for_thread(&self) -> Result<Ctx> {
        let conn = match self.tasks.take_conn() {
            Some(c) => c,
            None => self.root.open_db()?,
        };
        Ok(Ctx {
            root: self.root.clone(),
            conn,
            session: self.session,
            depth: self.depth,
            chain: self.chain.clone(),
            target: self.target.clone(),
            tasks: self.tasks.clone(),
            jobs: self.jobs.clone(),
            config: self.config.clone(),
            log_sink: self.log_sink.clone(),
            spec_watch: self.spec_watch.clone(),
            abandon: None,   // `spawn_task` sets these for its task
            demanded: None,
            wanted: None,
        })
    }


}

/// Normalize a command-line path (relative to cwd) to a root-relative key.
fn normalize_target(root: &Root, input: &str) -> Result<String> {
    let cwd = std::env::current_dir()?;
    crate::paths::normalize(&root.dir, &cwd, Path::new(input)).ok_or_else(|| {
        anyhow::anyhow!(
            "target {input:?} is outside the project root {}",
            root.dir.display()
        )
    })
}

// ---- verbose decision trace ---------------------------------------------------

/// Whether the decision trace is on (`--verbose` / `REDO_VERBOSE=1`). Read
/// once per process: the flag is set during argument parsing, before any
/// build starts, and reaches child processes through the environment.
fn verbose() -> bool {
    static V: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *V.get_or_init(|| std::env::var("REDO_VERBOSE").as_deref() == Ok("1"))
}

/// Whether jobserver/parallel-scheduler tracing is on (`--debug-jobs` /
/// `REDO_DEBUG_JOBS=1`). Read once per process, like `verbose`: the flag is
/// set during argument parsing and reaches child processes through the
/// environment.
fn debug_jobs() -> bool {
    static D: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *D.get_or_init(|| std::env::var("REDO_DEBUG_JOBS").as_deref() == Ok("1"))
}

/// Emit one decision-trace line (`--verbose`). `msg` is lazy so the
/// non-verbose path pays nothing.
pub(crate) fn vlog(ctx: &Ctx, msg: impl FnOnce() -> String) {
    if !verbose() {
        return;
    }
    trace_line(ctx, &format!("redo-msh -v: {}\n", msg()));
}

/// Emit one jobserver-trace line (`--debug-jobs`): token acquisition,
/// parallel-group launches, and completions.
pub(crate) fn jlog(ctx: &Ctx, msg: impl FnOnce() -> String) {
    if !debug_jobs() {
        return;
    }
    trace_line(ctx, &format!("redo-msh jobs: {}\n", msg()));
}

/// Write one trace line to this process's log sink — each is a single atomic
/// append, so the follower prints it inside the right target's block — or
/// straight to stderr when standalone (no build running).
fn trace_line(ctx: &Ctx, line: &str) {
    match &ctx.log_sink {
        Some(sink) => {
            use std::io::Write;
            if let Ok(mut f) = fs::OpenOptions::new().append(true).open(sink) {
                let _ = f.write_all(line.as_bytes());
            }
        }
        None => eprint!("{line}"),
    }
}

/// Short display form of an optional content hash, for trace lines.
pub(crate) fn hash8(h: Option<&str>) -> String {
    match h {
        Some(h) => format!("{}…", &h[..h.len().min(8)]),
        None => "(none)".to_string(),
    }
}

fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

// ---- database helpers -------------------------------------------------------

fn clear_deps(conn: &Connection, target: &str) -> Result<()> {
    conn.execute("DELETE FROM deps WHERE target = ?1", params![target])?;
    Ok(())
}

fn record_dep(
    conn: &Connection,
    target: &str,
    kind: DepKind,
    dep: Option<&str>,
    stamp: Option<&Stamp>,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO deps(target, kind, dep, csum, mtime, size)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            target,
            kind as i64,
            dep,
            stamp.map(|s| s.csum.as_str()),
            stamp.map(|s| s.mtime),
            stamp.map(|s| s.size),
        ],
    )?;
    Ok(())
}

fn upsert_file(
    conn: &Connection,
    path: &str,
    dofile: Option<&str>,
    built_at: Option<i64>,
    stamp: Option<&Stamp>,
    runid: i64,
) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO files(path, dofile, built_at, mtime, size, csum, runid)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            path,
            dofile,
            built_at,
            stamp.map(|s| s.mtime),
            stamp.map(|s| s.size),
            stamp.map(|s| s.csum.as_str()),
            runid,
        ],
    )?;
    Ok(())
}

/// Whether redo has ever generated this file (a `files` row with a recorded
/// do-file). Mirrors apenwarr's `is_generated` flag.
fn is_generated(conn: &Connection, path: &str) -> Result<bool> {
    let r = conn
        .query_row(
            "SELECT dofile IS NOT NULL FROM files WHERE path = ?1",
            params![path],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    Ok(r.unwrap_or(false))
}

/// Whether an exact `<target>.do` exists (as opposed to a `default.*.do`
/// match somewhere up the tree).
fn exact_dofile_exists(root: &Root, target_rel: &str) -> bool {
    root.dir.join(format!("{target_rel}.do")).is_file()
}

/// Whether `path` should be treated as a buildable target. Mirrors apenwarr
/// redo's static-file rule (builder.py `_start_self`, from djb's notes): a
/// file that exists on disk and was *not* generated by redo is a source when
/// only a `default.*.do` would match — `default.csv.do` must not try to
/// rebuild a hand-written `data.csv`.
///
/// Deliberate deviation from apenwarr (which treats these as static too): an
/// exact `<target>.do` states unambiguous intent that the file is a target,
/// so it stays buildable; the overwrite guard in `build_inner` then refuses
/// or prompts before clobbering the user's file.
pub(crate) fn is_target(ctx: &Ctx, path_rel: &str) -> Result<bool> {
    let abs = ctx.root.dir.join(path_rel);
    let exists_as_file = fs::metadata(&abs).map(|m| !m.is_dir()).unwrap_or(false);
    if exists_as_file && !is_generated(&ctx.conn, path_rel)? {
        let exact = exact_dofile_exists(&ctx.root, path_rel);
        vlog(ctx, || {
            if exact {
                format!(
                    "{path_rel}: buildable target: it exists on disk and redo never \
                     built it, but the exact do-file {path_rel}.do states it is a target"
                )
            } else {
                format!(
                    "{path_rel}: static source: it exists on disk, redo has no record \
                     of building it, and only a default.*.do could match (djb's \
                     static-file rule)"
                )
            }
        });
        return Ok(exact);
    }
    let found = dofile::find(&ctx.root, path_rel).0;
    vlog(ctx, || match &found {
        Some(df) => format!("{path_rel}: buildable target (do-file: {})", df.dofile_rel),
        None => format!("{path_rel}: not buildable: no do-file matches"),
    });
    Ok(found.is_some())
}

pub(crate) fn file_runid(conn: &Connection, path: &str) -> Result<Option<i64>> {
    let r = conn
        .query_row("SELECT runid FROM files WHERE path = ?1", params![path], |row| {
            row.get::<_, Option<i64>>(0)
        })
        .optional()?;
    Ok(r.flatten())
}

// ---- filesystem commit helpers ---------------------------------------------

fn fsync_file(path: &Path) -> Result<()> {
    if let Ok(f) = fs::File::open(path) {
        let _ = f.sync_all();
    }
    Ok(())
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) {
    if let Ok(f) = fs::File::open(dir) {
        let _ = f.sync_all();
    }
}
#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) {}

/// Number of rename attempts. On Windows a virus scanner or indexer can hold a
/// transient handle on the freshly written temp file, causing a sharing
/// violation; we retry with backoff. Unix rename(2) is atomic and never needs
/// retrying.
const RENAME_RETRIES: usize = if cfg!(windows) { 10 } else { 1 };

fn is_transient_io(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::AlreadyExists
    )
}

/// Atomically replace `dst` with `src` (same volume by construction).
///
/// On Windows we first try the native POSIX-semantics rename
/// (`SetFileInformationByHandle` + `FileRenameInfoEx`), which is atomic and can
/// replace a destination even if another process holds it open. If that fails
/// (older Windows / non-NTFS) we fall back to `std::fs::rename` (`MoveFileEx`)
/// with retry. On Unix, `rename(2)` is atomic and needs no retry.
pub(crate) fn atomic_rename(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        if win_posix_rename(src, dst).is_ok() {
            return Ok(());
        }
    }
    let mut delay = std::time::Duration::from_millis(2);
    for attempt in 0..RENAME_RETRIES {
        match fs::rename(src, dst) {
            Ok(()) => return Ok(()),
            Err(e) if attempt + 1 < RENAME_RETRIES && is_transient_io(&e) => {
                thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => {
                return Err(anyhow::Error::new(e)
                    .context(format!("renaming {} -> {}", src.display(), dst.display())));
            }
        }
    }
    unreachable!("rename loop always returns")
}

/// Native Windows atomic replace using `SetFileInformationByHandle` with
/// `FileRenameInfoEx` and POSIX semantics (Win10 1607+, NTFS/ReFS).
#[cfg(windows)]
fn win_posix_rename(src: &Path, dst: &Path) -> std::io::Result<()> {
    use core::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        FileRenameInfoEx, SetFileInformationByHandle, FILE_RENAME_INFO,
    };

    const DELETE: u32 = 0x0001_0000;
    const GENERIC_WRITE: u32 = 0x4000_0000;
    const FILE_SHARE_ALL: u32 = 0x0000_0007; // READ | WRITE | DELETE
    const REPLACE_IF_EXISTS: u32 = 0x0000_0001;
    const POSIX_SEMANTICS: u32 = 0x0000_0002;

    // The handle needs DELETE access to be renamed.
    let file = fs::OpenOptions::new()
        .access_mode(DELETE | GENERIC_WRITE)
        .share_mode(FILE_SHARE_ALL)
        .open(src)?;

    // FILE_RENAME_INFO is a header followed by the variable-length destination
    // name; allocate a buffer big enough for both.
    let name: Vec<u16> = dst.as_os_str().encode_wide().collect();
    let name_bytes = name.len() * core::mem::size_of::<u16>();
    let total = core::mem::size_of::<FILE_RENAME_INFO>() + name_bytes;
    let mut buf = vec![0u8; total];

    // SAFETY: `buf` is large enough for the header plus the name; we write only
    // within it and pass its true length as the buffer size.
    unsafe {
        let info = buf.as_mut_ptr() as *mut FILE_RENAME_INFO;
        (*info).Anonymous.Flags = REPLACE_IF_EXISTS | POSIX_SEMANTICS;
        (*info).RootDirectory = std::ptr::null_mut();
        (*info).FileNameLength = name_bytes as u32;
        let name_dst = core::ptr::addr_of_mut!((*info).FileName) as *mut u16;
        core::ptr::copy_nonoverlapping(name.as_ptr(), name_dst, name.len());

        let ok = SetFileInformationByHandle(
            file.as_raw_handle() as _,
            FileRenameInfoEx,
            info as *const c_void,
            total as u32,
        );
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Removes a set of temp files on drop, on every control-flow path (early
/// return, `?`, or panic). Crash-only design: a SIGKILL won't run this, but the
/// startup temp GC sweeps anything left behind.
struct TempGuard {
    paths: Vec<PathBuf>,
}
impl Drop for TempGuard {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = fs::remove_file(p);
        }
    }
}

/// Remove leftover `<target>.redo-tmp.*` files (e.g. from a previously crashed
/// build of this target) in the directory where the temp would be created.
fn clean_target_temps(dodir: &Path, arg_target: &str) {
    let tmp_path = Path::new(arg_target);
    let dir = match tmp_path.parent() {
        Some(p) if !p.as_os_str().is_empty() => dodir.join(p),
        _ => dodir.to_path_buf(),
    };
    let base = match tmp_path.file_name().and_then(|s| s.to_str()) {
        Some(b) => b,
        None => return,
    };
    let prefix = format!("{base}.redo-tmp.");
    if let Ok(entries) = fs::read_dir(&dir) {
        for e in entries.flatten() {
            if let Some(name) = e.file_name().to_str() {
                if name.starts_with(&prefix) {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Sweep stale stdout-capture temp files left by crashed builds.
fn gc_temps(root: &Root) {
    let tmp = root.redo_dir().join("tmp");
    if let Ok(entries) = fs::read_dir(&tmp) {
        for e in entries.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }
}

/// Refuse to silently overwrite an existing target file the user may own.
/// A normal rebuild of an unmodified output is NOT a problem — redo verifies
/// the file still matches the content hash it recorded after building it, and
/// overwrites freely. Only two situations stop the build, each explained with
/// its evidence: the file no longer matches what redo recorded (edited
/// outside redo), or redo has no record of ever building it. With `--yes`
/// (env `REDO_YES=1`) we overwrite; interactively we ask (y/n/all/quit, one
/// question on the terminal at a time); otherwise we fail and tell the user
/// how to resolve it. An `all`/`quit` answer settles the question for the
/// rest of the run: this process via `OVERWRITE_MODE`, child redo processes
/// via `REDO_YES`/`REDO_NO` in the do-file environment.
fn check_not_hand_edited(ctx: &Ctx, target_rel: &str, target_abs: &Path) -> Result<()> {
    if !target_abs.exists() {
        return Ok(());
    }
    let problem = match overwrite_problem(ctx, target_rel, target_abs)? {
        Some(p) => p,
        None => return Ok(()),
    };
    if overwrite_all() {
        let why = if std::env::var("REDO_YES").as_deref() == Ok("1") {
            "--yes"
        } else {
            "answered 'all'"
        };
        eprintln!("redo-msh: {problem}\nredo-msh: overwriting {target_rel} ({why})");
        return Ok(());
    }
    if !overwrite_none() && user_present() {
        match prompt_overwrite(ctx, target_rel, &problem)? {
            Answer::Yes => return Ok(()),
            Answer::All => {
                OVERWRITE_MODE.store(OW_ALL, Ordering::Relaxed);
                return Ok(());
            }
            Answer::Quit => OVERWRITE_MODE.store(OW_NONE, Ordering::Relaxed),
            Answer::No => {}
        }
    }
    bail!(
        "{problem}\n\
         Refusing to overwrite it. Rerun with --yes to rebuild over it, \
         or move the file out of the way."
    )
}

/// Session-wide overwrite decision, shared by every worker thread in this
/// process. `all`/`quit` prompt answers land here so later targets stop
/// asking; the corresponding env vars carry the decision both down from
/// `--yes` and across to child redo processes.
const OW_ASK: u8 = 0;
const OW_ALL: u8 = 1;
const OW_NONE: u8 = 2;
static OVERWRITE_MODE: AtomicU8 = AtomicU8::new(OW_ASK);

fn overwrite_all() -> bool {
    OVERWRITE_MODE.load(Ordering::Relaxed) == OW_ALL
        || std::env::var("REDO_YES").as_deref() == Ok("1")
}

fn overwrite_none() -> bool {
    OVERWRITE_MODE.load(Ordering::Relaxed) == OW_NONE
        || std::env::var("REDO_NO").as_deref() == Ok("1")
}

/// Whether a human is plausibly attached: stdin or stderr is the terminal.
/// The conversation itself runs on the console device (`/dev/tty`,
/// `CONIN$`/`CONOUT$`), so a nested redo whose stderr is captured into a
/// target log can still ask — but a run with every standard stream detached
/// (CI, tests, cron) must fail closed rather than read keystrokes from
/// whatever terminal happens to be attached to the process.
fn user_present() -> bool {
    std::io::stdin().is_terminal() || std::io::stderr().is_terminal()
}

/// Why the on-disk file is not redo's to overwrite (None: it is ours).
/// States the evidence, not just the verdict: what redo recorded, what is on
/// disk now, and the timestamps that support the conclusion.
fn overwrite_problem(
    ctx: &Ctx,
    target_rel: &str,
    target_abs: &Path,
) -> Result<Option<String>> {
    let mtime_ago = fs::metadata(target_abs)
        .map(|m| ago(stamp::mtime_nanos(&m)))
        .unwrap_or_else(|_| "at an unknown time".to_string());
    if is_generated(&ctx.conn, target_rel)? {
        // Generated: only a content mismatch against the recorded post-build
        // hash is a problem; phony targets (no recorded hash) are exempt.
        let recorded = match files_row(&ctx.conn, target_rel)? {
            Some(Some(csum)) => csum,
            _ => return Ok(None),
        };
        let current = stamp::hash_file(target_abs)?;
        if current == recorded {
            return Ok(None);
        }
        let built = match files_built_at(&ctx.conn, target_rel)? {
            Some(t) => ago(t),
            None => "previously".to_string(),
        };
        Ok(Some(format!(
            "{target_rel} was modified outside redo since redo last built it:\n\
             - redo built it {built} and recorded content hash {}...\n\
             - the file on disk hashes {}... and was last modified {mtime_ago}",
            &recorded[..recorded.len().min(8)],
            &current[..current.len().min(8)],
        )))
    } else {
        Ok(Some(format!(
            "{target_rel} exists (last modified {mtime_ago}) but redo has no record \
             of ever building it:\n\
             there is no entry for this path in .redom/redo-msh.db, so either it \
             is a hand-written\n\
             source file, or the database was recreated after an older redo built it"
        )))
    }
}

/// Human-readable age of a nanoseconds-since-epoch timestamp, for evidence in
/// overwrite refusals. Relative ages avoid timezone ambiguity in the output.
fn ago(nanos: i64) -> String {
    let delta_s = (now_nanos() - nanos) / 1_000_000_000;
    if delta_s < 0 {
        return "in the future (clock skew?)".to_string();
    }
    if delta_s < 2 {
        return "just now".to_string();
    }
    if delta_s < 120 {
        return format!("{delta_s} seconds ago");
    }
    let minutes = delta_s / 60;
    if minutes < 120 {
        return format!("{minutes} minutes ago");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours} hours ago");
    }
    format!("{} days ago", hours / 24)
}

/// What the user chose at an overwrite prompt. `All`/`Quit` settle the
/// question for the rest of the run.
enum Answer {
    Yes,
    No,
    All,
    Quit,
}

/// The console device, for talking to the user directly even when the
/// standard streams are redirected (a nested redo's stderr goes to its
/// target's log). Returns (read half, write half).
#[cfg(unix)]
fn open_console() -> std::io::Result<(fs::File, fs::File)> {
    let r = fs::OpenOptions::new().read(true).write(true).open("/dev/tty")?;
    let w = r.try_clone()?;
    Ok((r, w))
}

/// Windows has no /dev/tty; the console devices are CONIN$ (keyboard) and
/// CONOUT$ (screen). CONIN$ must be opened with both read and write access
/// or the console refuses reads.
#[cfg(windows)]
fn open_console() -> std::io::Result<(fs::File, fs::File)> {
    let r = fs::OpenOptions::new().read(true).write(true).open("CONIN$")?;
    let w = fs::OpenOptions::new().read(true).write(true).open("CONOUT$")?;
    Ok((r, w))
}

#[cfg(not(any(unix, windows)))]
fn open_console() -> std::io::Result<(fs::File, fs::File)> {
    Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "no console"))
}

/// Ask on the console whether to overwrite a target redo does not own
/// (hand-edited, or never generated by redo).
///
/// Parallel-build protocol: prompts are serialized by a kernel file lock
/// (`lock_prompt`) so exactly one question owns the terminal at a time, and
/// the full problem text is written *after* the lock is won so the question
/// on screen is always the one the next keystroke answers. While blocked on
/// the human, this worker's jobserver token is returned to the pool — other
/// jobs keep the build saturated — and taken back before the build resumes.
fn prompt_overwrite(ctx: &Ctx, target_rel: &str, problem: &str) -> Result<Answer> {
    use std::io::{BufRead, Write};
    let (rin, mut wout) = match open_console() {
        Ok(t) => t,
        Err(_) => return Ok(Answer::No),
    };
    ctx.jobs.release();
    let answer = (|| {
        let _serial = crate::lock::lock_prompt(&ctx.root)?;
        write!(
            wout,
            "\nredo-msh: {problem}\n\
             Overwrite {target_rel}? [y]es / [n]o / [a]ll this run / [q]uit asking: "
        )?;
        wout.flush()?;
        let mut line = String::new();
        std::io::BufReader::new(rin).read_line(&mut line)?;
        Ok(match line.trim().to_ascii_lowercase().as_str() {
            "y" | "yes" => Answer::Yes,
            "a" | "all" => Answer::All,
            "q" | "quit" => Answer::Quit,
            _ => Answer::No,
        })
    })();
    // Take a token back before resuming, on the answer and error paths alike.
    // The pool is try-only, so spin: any running job's completion frees one.
    while !ctx.jobs.try_acquire() {
        thread::sleep(std::time::Duration::from_millis(50));
    }
    answer
}

// ---- the build operation ----------------------------------------------------

/// Build `target_rel`, acquiring the per-target lock for build exclusion.
///
/// The chain check below is a fast path for path-local cycles (an ancestor
/// do-file re-demanding a target it is inside of), kept for its readable
/// error message; the authoritative cycle detection is the shared waits-for
/// graph, whose atomic check-and-insert runs in `build_parallel` before any
/// wait can start (`waits.rs`). After acquiring the lock we re-check whether
/// the target was built this session by whoever held the lock before us —
/// the double-checked rebuild that makes concurrent builds safe. `force`
/// (top-level `redo`) skips that check and always rebuilds.
pub fn build(ctx: &Ctx, target_rel: &str, force: bool) -> Result<()> {
    if ctx.chain.iter().any(|t| t.eq_ignore_ascii_case(target_rel)) {
        bail!(
            "dependency cycle detected: {} -> {}",
            ctx.chain.join(" -> "),
            target_rel
        );
    }
    // The same bound the log follower enforces on its trace stack: check it
    // on both sides of the fence.
    if ctx.chain.len() >= crate::logs::DEPTH_MAX {
        bail!(
            "dependency chain deeper than {} at {target_rel}",
            crate::logs::DEPTH_MAX
        );
    }
    // Try first so a contended lock can be announced in the live log (the
    // user otherwise sees an unexplained stall) before we block on it.
    // Kernel-lock waits carry no wait-graph edge (SpeculationMP proves the
    // by-name edges bridge them), so a demanded build may block outright —
    // but a SPECULATIVE lineage must stay interruptible: it polls the lock
    // and its abort watch, so an eviction reaches it even here.
    let _lock = match crate::lock::try_lock_target(&ctx.root, target_rel)? {
        Some(lock) => lock,
        None => {
            emit_event(ctx, &crate::logs::Event::Waiting {
                session: ctx.session,
                target: target_rel.to_string(),
            });
            if ctx.spec_watch.is_empty() && ctx.abandon.is_none() {
                crate::lock::lock_target(&ctx.root, target_rel)?
            } else {
                loop {
                    crate::parallel::abort_check(ctx)?;
                    if let Some(lock) = crate::lock::try_lock_target(&ctx.root, target_rel)? {
                        break lock;
                    }
                    thread::sleep(std::time::Duration::from_millis(25));
                }
            }
        }
    };
    if !force && file_runid(&ctx.conn, target_rel)? == Some(ctx.session) {
        // Built by another process while we waited for the lock.
        vlog(ctx, || {
            format!(
                "{target_rel}: skipping: another process built it while we waited \
                 for its lock"
            )
        });
        return Ok(());
    }
    build_inner(ctx, target_rel, force)
}

/// Run the target's do-file and commit its output. Assumes the per-target lock
/// is held and a build has been determined necessary.
fn build_inner(ctx: &Ctx, target_rel: &str, force: bool) -> Result<()> {
    let target_abs = ctx.root.dir.join(target_rel);

    // An existing file that redo never generated is a source when only a
    // default.*.do matches (djb's rule, apenwarr builder.py `_start_self`):
    // record it as static and do nothing. Unambiguous intent overrides the
    // rule, loudly: an exact `<target>.do` keeps it a target, and so does
    // naming it explicitly at the command line (`force`) — otherwise a
    // rebuilt database silently turns every existing default-matched target
    // into a source and an explicit `redo X` into a confusing no-op. In both
    // cases the overwrite guard below refuses/prompts before clobbering a
    // file that may really be a source; `--yes` rebuilds and re-records the
    // target as generated.
    let exists = target_abs.exists();
    let exists_as_file = fs::metadata(&target_abs).map(|m| !m.is_dir()).unwrap_or(false);
    if exists_as_file
        && !force
        && !is_generated(&ctx.conn, target_rel)?
        && !exact_dofile_exists(&ctx.root, target_rel)
    {
        vlog(ctx, || {
            format!(
                "{target_rel}: NOT building: recording it as a static source (it \
                 exists, redo never built it, and no exact {target_rel}.do exists)"
            )
        });
        let stamp = stamp::stamp_file(&target_abs)?;
        upsert_file(&ctx.conn, target_rel, None, None, stamp.as_ref(), ctx.session)?;
        return Ok(());
    }

    let (dofile_opt, absent) = dofile::find(&ctx.root, target_rel);
    let df = match dofile_opt {
        Some(d) => d,
        None => {
            // No rule, but the file exists (e.g. a generated file whose
            // do-file was removed, or a directory): it becomes static.
            if exists {
                vlog(ctx, || {
                    format!(
                        "{target_rel}: NOT building: no do-file matches any more, but \
                         the file exists — re-recording it as a static source"
                    )
                });
                let stamp = stamp::stamp_file(&target_abs)?;
                upsert_file(&ctx.conn, target_rel, None, None, stamp.as_ref(), ctx.session)?;
                return Ok(());
            }
            bail!("no .do file found to build target {target_rel:?}")
        }
    };

    vlog(ctx, || {
        format!(
            "{target_rel}: building with do-file {}{}",
            df.dofile_rel,
            if force { " (forced: named at the command line)" } else { "" }
        )
    });

    // Refuse to silently clobber a target that was modified by hand since we
    // last built it (or that redo never generated at all). Checked before any
    // database mutation, so a refusal leaves no trace of an attempted build.
    check_not_hand_edited(ctx, target_rel, &target_abs)?;

    // Reset dependency edges (the do-file re-declares them as it runs) and
    // mark the build as in flight, atomically. The marker is removed only
    // after a successful commit, so a failed (or crashed) build leaves the
    // target unconditionally out of date — otherwise a later run would trust
    // a leftover output file from an earlier success plus the fresh edges
    // recorded below, and accept the target as up to date. One transaction so
    // a crash can never land between the clear and the marker, which would
    // leave the target looking never-attempted. (Stamp computed outside: no
    // file hashing while holding the write lock.)
    let df_stamp = stamp::stamp_file(&df.dofile_abs)?;
    db::write_txn(&ctx.conn, |conn| {
        clear_deps(conn, target_rel)?;
        record_dep(conn, target_rel, DepKind::Uncommitted, None, None)?;
        record_dep(conn, target_rel, DepKind::DoFile, Some(&df.dofile_rel), df_stamp.as_ref())?;
        for a in &absent {
            record_dep(conn, target_rel, DepKind::IfCreate, Some(a), None)?;
        }
        Ok(())
    })?;

    let before_t = fs::metadata(&target_abs).ok();

    // Temp output ($3), beside the target for same-volume atomic rename. Sweep
    // any stale temps for this target from a previously crashed build.
    clean_target_temps(&df.dodir_abs, &df.arg_target);
    let pid = std::process::id();
    let tmp3_rel = format!("{}.redo-tmp.{}", df.arg_target, pid);
    let tmp3_abs = df.dodir_abs.join(&tmp3_rel);

    // Capture file for the do-file's stdout (output detection). Its stderr
    // goes to the target's log, streamed live by the top-level's follower.
    let tmp_dir = ctx.root.redo_dir().join("tmp");
    fs::create_dir_all(&tmp_dir)?;
    let capture_path = tmp_dir.join(format!("stdout.{}.{}", pid, sanitize(target_rel)));

    // All temps are removed on every exit path (return, `?`, panic).
    let _guard = TempGuard {
        paths: vec![tmp3_abs.clone(), capture_path.clone()],
    };
    let capture_file = fs::File::create(&capture_path)
        .with_context(|| format!("creating stdout capture {}", capture_path.display()))?;

    // Writer protocol for the live log (see logs.rs). Order is load-bearing:
    // the build lock is already held (I2); the log file exists before the
    // `do` event is appended (I3); the `done` terminator — guaranteed on
    // every exit path by `DoneGuard` — is written before build() releases
    // the lock (I4, drop order: the guard lives in this frame, the lock in
    // build()'s). With no sink (standalone use outside any build) do-files
    // simply inherit our stderr.
    let (errlog, mut done_guard, log_path) = match &ctx.log_sink {
        Some(sink) => {
            let log_path = crate::logs::target_log_path(&ctx.root, target_rel);
            crate::logs::create_log(&log_path)?;
            crate::logs::event_append(sink, &crate::logs::Event::Do {
                session: ctx.session,
                target: target_rel.to_string(),
            });
            let guard = crate::logs::DoneGuard::new(
                log_path.clone(),
                ctx.session,
                target_rel.to_string(),
            );
            // Append mode, so the do-file's writes interleave atomically with
            // event lines appended by child `redo-ifchange` processes. A
            // plain write handle at offset 0 would silently overwrite them.
            let f = fs::OpenOptions::new()
                .append(true)
                .open(&log_path)
                .with_context(|| format!("opening log {}", log_path.display()))?;
            (std::process::Stdio::from(f), Some(guard), Some(log_path))
        }
        None => (std::process::Stdio::inherit(), None, None),
    };

    // Child environment.
    let mut child_chain = ctx.chain.clone();
    child_chain.push(target_rel.to_string());

    // Resolve the interpreter for this do-file (built-in `msh` unless redo.toml
    // says otherwise) and build `<interp...> <dofile> $1 $2 $3`. The config
    // guarantees a non-empty command.
    let interp = ctx.config.interpreter(&df.dofile_rel);
    let (interp0, interp_rest) = interp.split_first().expect("interpreter is non-empty");
    let mut command = Command::new(interp0);
    command
        .args(interp_rest)
        .arg(&df.dofile_abs)
        .arg(&df.arg_target)
        .arg(&df.arg_base)
        .arg(&tmp3_rel)
        .current_dir(&df.dodir_abs)
        // Never hand the terminal to a do-file: with stdout/stderr already
        // redirected, a null stdin leaves no tty on any child stream, so
        // shells that do job control (mshell probes stdin/stdout/stderr for
        // a control terminal) skip it entirely. N parallel children sharing
        // one tty otherwise race each other's tcsetpgrp restores. Prompts
        // stay in the redo process, which keeps its tty.
        .stdin(std::process::Stdio::null())
        .stdout(capture_file)
        .stderr(errlog)
        .env(E_ROOT, &ctx.root.dir)
        .env(E_TARGET, target_rel)
        .env(E_SESSION, ctx.session.to_string())
        .env(E_DEPTH, (ctx.depth + 1).to_string())
        .env(E_CHAIN, child_chain.join("\n"));
    // A speculative lineage's abort watch travels to child redo processes:
    // every blocking primitive below this do-file polls it (SpeculationMP
    // rule R3's eviction has to reach the whole subtree).
    if ctx.spec_watch.is_empty() {
        command.env_remove(crate::waits::ENV_SPEC_WATCH);
    } else {
        command.env(
            crate::waits::ENV_SPEC_WATCH,
            crate::waits::watch_to_env(&ctx.spec_watch),
        );
    }
    // Carry a session-wide interactive decision ('a'/'q' at a prompt) into
    // child redo processes this do-file spawns; --yes already travels in the
    // inherited environment.
    if OVERWRITE_MODE.load(Ordering::Relaxed) == OW_ALL {
        command.env("REDO_YES", "1");
    } else if OVERWRITE_MODE.load(Ordering::Relaxed) == OW_NONE {
        command.env("REDO_NO", "1");
    }
    // Child `redo-ifchange` processes append their trace events to this
    // target's log by path (never by inherited fd — a do-file redirecting its
    // own stderr must not receive event lines).
    match &log_path {
        Some(p) => {
            command.env(crate::logs::ENV_LOG_PATH, p);
        }
        None => {
            command.env_remove(crate::logs::ENV_LOG_PATH);
        }
    }

    // Make the redo command family resolvable from inside the do-file by
    // prepending our own directory (where `redo`, `redo-ifchange`, ... ship
    // alongside `redo-msh`) to the child's PATH.
    let child_path = child_path();
    if let Some(path) = &child_path {
        command.env("PATH", path);
    }

    // Speculative work must stay cancellable even while its do-file runs
    // (SpeculationMP rules R3/R5): instead of blocking on the child, poll
    // it alongside the abort/abandon signals and KILL it when the
    // speculation is no longer wanted. Killing mid-build is crash-safe by
    // design (Uncommitted marker recorded above, temps swept by the guard
    // and the startup GC, kernel-released locks); any nested redo
    // processes watching our lineage unwind via REDO_SPEC_WATCH.
    let mut child = command
        .spawn()
        .map_err(|e| interpreter_spawn_error(e, &interp, &df.dofile_abs, child_path.as_deref()))?;
    let speculative = ctx.abandon.is_some() || !ctx.spec_watch.is_empty();
    let status = if !speculative {
        child.wait()?
    } else {
        loop {
            if let Some(st) = child.try_wait()? {
                break st;
            }
            if crate::parallel::speculation_dead(ctx)? {
                let _ = child.kill();
                let _ = child.wait();
                if let Some(g) = done_guard.as_mut() {
                    g.record_exit(1);
                }
                bail!(
                    "speculation aborted: the build of {target_rel} is no longer \
                     needed here (it will be re-run when genuinely needed)"
                );
            }
            thread::sleep(std::time::Duration::from_millis(25));
        }
    };
    if let Some(g) = done_guard.as_mut() {
        // Signal death has no code; classify it as failed (exit 1).
        g.record_exit(status.code().unwrap_or(1));
    }

    let after_t = fs::metadata(&target_abs).ok();
    let cap_size = fs::metadata(&capture_path).map(|m| m.len()).unwrap_or(0);
    let tmp3_exists = tmp3_abs.exists();

    // The do-file must not write the target ($1) directly.
    let modified_directly = match (&before_t, &after_t) {
        (_, None) => false,
        (None, Some(at)) => !at.is_dir(),
        (Some(bt), Some(at)) => !at.is_dir() && bt.modified().ok() != at.modified().ok(),
    };
    if modified_directly {
        bail!("do-file for {target_rel} modified the target directly; write to $3 or stdout, not $1");
    }
    if !status.success() {
        bail!(
            "do-file for {target_rel} failed (exit {})",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        );
    }
    if tmp3_exists && cap_size > 0 {
        bail!("do-file for {target_rel} wrote to both stdout and $3; choose one (status messages go to stderr)");
    }

    // Commit the output.
    if cap_size > 0 && !tmp3_exists {
        fs::copy(&capture_path, &tmp3_abs)
            .with_context(|| format!("copying stdout to {}", tmp3_abs.display()))?;
    }

    if tmp3_abs.exists() {
        fsync_file(&tmp3_abs)?;
        atomic_rename(&tmp3_abs, &target_abs)?;
        if let Some(parent) = target_abs.parent() {
            fsync_dir(parent);
        }
    } else {
        // No output at all: a phony/virtual target. Remove any stale file.
        let _ = fs::remove_file(&target_abs);
    }

    // Record the post-build stamp and drop the in-flight marker recorded
    // before the run, atomically: together they are the "target is built"
    // commit point.
    let stamp = stamp::stamp_file(&target_abs)?;
    db::write_txn(&ctx.conn, |conn| {
        upsert_file(
            conn,
            target_rel,
            Some(&df.dofile_rel),
            Some(now_nanos()),
            stamp.as_ref(),
            ctx.session,
        )?;
        conn.execute(
            "DELETE FROM deps WHERE target = ?1 AND kind = ?2",
            params![target_rel, DepKind::Uncommitted as i64],
        )?;
        Ok(())
    })?;
    // Terminate the log stream: `done` with exit 0. Every early return above
    // writes it too, via the guard's Drop, with the failing exit code.
    if let Some(g) = done_guard {
        g.commit_success();
    }
    Ok(())
}

/// Append a trace event to this process's sink, if it has one. Events are
/// advisory display data; a missing sink (standalone use) is not an error.
fn emit_event(ctx: &Ctx, event: &crate::logs::Event) {
    if let Some(sink) = &ctx.log_sink {
        crate::logs::event_append(sink, event);
    }
}

/// `redo-msh ifchange <deps...>`: bring each dependency up to date (build it if
/// it is out of date and has a do-file) and record the dependency edge from the
/// current `REDO_TARGET`.
pub fn ifchange(ctx: &Ctx, inputs: &[String]) -> Result<()> {
    // Classify all inputs first.
    let mut items: Vec<(String, PathBuf, bool)> = Vec::with_capacity(inputs.len());
    for input in inputs {
        let dep_rel = normalize_target(&ctx.root, input)?;
        let dep_abs = ctx.root.dir.join(&dep_rel);
        let buildable = is_target(ctx, &dep_rel)?;
        if !buildable && !dep_abs.exists() {
            bail!("no .do file and no source file for dependency {dep_rel:?}");
        }
        items.push((dep_rel, dep_abs, buildable));
    }

    // Source files: stamp/refresh now (cheap, serial).
    for (dep_rel, dep_abs, buildable) in &items {
        if !buildable {
            vlog(ctx, || {
                format!("{dep_rel}: source file (nothing builds it); refreshing its stamp")
            });
            let stamp = stamp::stamp_file(dep_abs)?;
            upsert_file(&ctx.conn, dep_rel, None, None, stamp.as_ref(), ctx.session)?;
        }
    }

    // Buildable targets: bring up to date, in parallel under the jobserver.
    let buildables: Vec<String> = items
        .iter()
        .filter(|(_, _, b)| *b)
        .map(|(d, _, _)| d.clone())
        .collect();
    build_parallel(ctx, &buildables)?;

    // Record the edges from the parent target (serial, on our connection).
    // Stamps (which may hash file contents) are computed before the
    // transaction so the write lock is held only for the inserts.
    if let Some(parent) = &ctx.target {
        let mut stamped = Vec::with_capacity(items.len());
        for (dep_rel, dep_abs, _) in &items {
            let stamp = stamp::stamp_file(dep_abs)?;
            vlog(ctx, || {
                format!(
                    "{parent}: recorded dependency on {dep_rel} (hash {})",
                    hash8(stamp.as_ref().map(|s| s.csum.as_str()))
                )
            });
            stamped.push((dep_rel, stamp));
        }
        db::write_txn(&ctx.conn, |conn| {
            for (dep_rel, stamp) in &stamped {
                record_dep(conn, parent, DepKind::IfChange, Some(dep_rel), stamp.as_ref())?;
            }
            Ok(())
        })?;
    } else {
        vlog(ctx, || {
            "not inside a do-file (no REDO_TARGET): dependencies were brought up to \
             date but no edges were recorded"
                .to_string()
        });
    }
    Ok(())
}

/// Bring multiple buildable targets up to date, in parallel, through the
/// task registry (`parallel.rs`). This is the mid-build (`redo-ifchange`)
/// entry point: before waiting on any dep, the parent's HARD demand edges
/// enter the shared waits-for graph via the atomic check-and-insert of
/// SpeculationMP rule R3 — an all-hard cycle is a real dependency cycle
/// (error on every interleaving, never a deadlock); a cycle riding a soft
/// edge makes the SPECULATION yield (`try_demand` evicts it) and the
/// demand retries. The demand insert also supersedes this process's own
/// creation edge for the dep, upgrading a speculative task in flight to a
/// demanded one.
fn build_parallel(ctx: &Ctx, deps: &[String]) -> Result<()> {
    if deps.is_empty() {
        return Ok(());
    }
    jlog(ctx, || {
        format!("parallel group of {}: {}", deps.len(), deps.join(", "))
    });
    if let Some(parent) = &ctx.target {
        // Happy path first: every cycle-free demand edge goes in as one
        // batched transaction; only deps whose edge hit a cycle take the
        // per-dep evict/retry loop.
        let inserted = crate::waits::try_demand_batch(&ctx.root, &ctx.conn, parent, deps)?;
        for (d, ok) in deps.iter().zip(inserted) {
            if !ok {
                demand_edge(ctx, parent, d)?;
            }
        }
    }
    let result = crate::parallel::ensure_all(ctx, deps, false);
    // Wait for speculative work this process started before returning to the
    // do-file; then this ifchange no longer blocks on anything. Safe: every
    // in-flight task is edge-covered (demand or creation edge), so a
    // cross-branch cycle is resolved by eviction, never by hanging here.
    crate::parallel::drain(ctx);
    if let Some(parent) = &ctx.target {
        let _ = crate::waits::clear_waiter(&ctx.conn, parent);
    }
    result
}

/// Insert the hard demand edge `parent -> dep`, looping over evictions
/// (rule R3). A `RealCycle` is trusted only after dead-owner GC and — if we
/// just aborted a speculative lineage — after a brief stabilization window:
/// the aborted lineage's residual hard edges dissolve as its processes
/// notice the abort and unwind, and the model's atomic abort corresponds to
/// that whole unwind.
fn demand_edge(ctx: &Ctx, parent: &str, dep: &str) -> Result<()> {
    use crate::waits::DemandOutcome;
    let mut gc_done = false;
    let mut aborted_lineage = false;
    let mut stabilize = 0u32;
    loop {
        crate::parallel::abort_check(ctx)?;
        match crate::waits::try_demand(&ctx.root, &ctx.conn, parent, dep)? {
            DemandOutcome::Inserted => return Ok(()),
            DemandOutcome::Evicted { aborted_lineage: a } => {
                jlog(ctx, || {
                    format!(
                        "{parent}: demand on {dep} evicted a speculative {} to \
                         avoid a wait cycle; retrying",
                        if a { "build (aborted)" } else { "checker wait" }
                    )
                });
                if a {
                    aborted_lineage = true;
                }
            }
            DemandOutcome::RealCycle => {
                if !gc_done {
                    gc_done = true;
                    crate::waits::gc_dead_edges(&ctx.root, &ctx.conn)?;
                    continue;
                }
                if aborted_lineage && stabilize < 400 {
                    stabilize += 1;
                    thread::sleep(std::time::Duration::from_millis(25));
                    continue;
                }
                let _ = crate::waits::clear_waiter(&ctx.conn, parent);
                let path = if ctx.chain.is_empty() {
                    parent.to_string()
                } else {
                    ctx.chain.join(" -> ")
                };
                bail!("dependency cycle detected: {path} -> {dep}");
            }
        }
    }
}

/// Bring a buildable `target` up to date: build it if out of date, otherwise
/// mark it verified for this session. Idempotent within a session. The whole
/// traversal — speculative parallel checking over recorded deps, token-
/// bounded builds, cycle handling on the shared waits-for graph — lives in
/// `parallel.rs`.
pub fn ensure(ctx: &Ctx, target: &str) -> Result<()> {
    crate::parallel::ensure(ctx, target)
}

/// Mark a target as verified-up-to-date this session and refresh its stamp
/// cache, so later `ensure` calls in the same session short-circuit.
pub(crate) fn mark_verified(ctx: &Ctx, target: &str) -> Result<()> {
    let _ = current_csum(ctx, target)?; // refresh mtime/size/csum cache
    ctx.conn.execute(
        "UPDATE files SET runid = ?1 WHERE path = ?2",
        params![ctx.session, target],
    )?;
    Ok(())
}

/// The current content hash of a file, using the **guarded fast path**:
/// if `(mtime, size)` match the cached row and the mtime carries sub-second
/// precision (proving a fine-resolution filesystem, so same-second collisions
/// are nanosecond-width), reuse the cached hash without re-reading the file.
/// Otherwise re-hash and refresh the cache. On a coarse filesystem (FAT: whole-
/// second mtimes) the sub-second test fails, so we always hash — exactly the
/// "always-hash on coarse FS" rule, derived from the data itself.
///
/// Returns `Ok(None)` if the file does not exist.
pub(crate) fn current_csum(ctx: &Ctx, path_rel: &str) -> Result<Option<String>> {
    let abs = ctx.root.dir.join(path_rel);
    let meta = match fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("stat {path_rel}"))),
    };
    let mtime = stamp::mtime_nanos(&meta);
    let size = if meta.is_dir() { 0 } else { meta.len() as i64 };

    let cached = files_stamp(&ctx.conn, path_rel)?;
    if let Some((rm, rs, rc)) = &cached {
        if *rm == mtime && *rs == size && stat_is_trustworthy(mtime) {
            vlog(ctx, || {
                format!(
                    "{path_rel}: mtime and size match the cache and the mtime has \
                     sub-second precision; trusting cached hash {} without re-reading",
                    hash8(Some(rc))
                )
            });
            return Ok(Some(rc.clone()));
        }
    }

    let csum = if meta.is_dir() {
        stamp::DIR_CSUM.to_string()
    } else {
        stamp::hash_file(&abs)?
    };
    vlog(ctx, || {
        let why = match &cached {
            None => "no cached stamp".to_string(),
            Some((rm, _, _)) if *rm != mtime => "mtime changed".to_string(),
            Some((_, rs, _)) if *rs != size => "size changed".to_string(),
            Some(_) => "whole-second mtime cannot be trusted".to_string(),
        };
        format!("{path_rel}: hashed the file contents ({why}): {}", hash8(Some(&csum)))
    });
    // Refresh the cache for an existing row (do not invent rows here).
    ctx.conn.execute(
        "UPDATE files SET mtime = ?1, size = ?2, csum = ?3 WHERE path = ?4",
        params![mtime, size, csum, path_rel],
    )?;
    Ok(Some(csum))
}

/// Whether a `(mtime, size)` match may be trusted to mean "unchanged" without
/// re-hashing: only when the mtime has sub-second precision.
fn stat_is_trustworthy(mtime_nanos: i64) -> bool {
    mtime_nanos % 1_000_000_000 != 0
}

/// Fetch `(mtime, size, csum)` for a file row, only if all are present.
fn files_stamp(conn: &Connection, path: &str) -> Result<Option<(i64, i64, String)>> {
    let row = conn
        .query_row(
            "SELECT mtime, size, csum FROM files WHERE path = ?1",
            params![path],
            |r| {
                Ok((
                    r.get::<_, Option<i64>>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    Ok(match row {
        Some((Some(m), Some(s), Some(c))) => Some((m, s, c)),
        _ => None,
    })
}

/// When redo last committed a build of this target (nanoseconds since epoch),
/// if recorded. Evidence for overwrite refusals.
fn files_built_at(conn: &Connection, path: &str) -> Result<Option<i64>> {
    let row = conn
        .query_row(
            "SELECT built_at FROM files WHERE path = ?1",
            params![path],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?;
    Ok(row.flatten())
}

/// Fetch a file row's recorded csum if the row exists. Returns
/// `Some(None)` for a phony target (row exists, csum NULL).
pub(crate) fn files_row(conn: &Connection, path: &str) -> Result<Option<Option<String>>> {
    let row = conn
        .query_row(
            "SELECT csum FROM files WHERE path = ?1",
            params![path],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?;
    Ok(row)
}

/// All dependency edges of a target as `(kind, dep, csum)`.
pub(crate) fn read_deps(
    conn: &Connection,
    target: &str,
) -> Result<Vec<(DepKind, Option<String>, Option<String>)>> {
    let mut stmt = conn.prepare("SELECT kind, dep, csum FROM deps WHERE target = ?1")?;
    let rows = stmt.query_map(params![target], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (k, dep, csum) = row?;
        if let Some(kind) = DepKind::from_i64(k) {
            out.push((kind, dep, csum));
        }
    }
    Ok(out)
}

/// `redo-msh ifcreate <paths...>`: record that the target depends on these
/// paths *not* existing.
pub fn ifcreate(ctx: &Ctx, inputs: &[String]) -> Result<()> {
    let parent = match &ctx.target {
        Some(p) => p,
        None => bail!("redo-msh ifcreate must be called from within a do-file"),
    };
    for input in inputs {
        let dep_rel = normalize_target(&ctx.root, input)?;
        if ctx.root.dir.join(&dep_rel).exists() {
            bail!("redo-msh ifcreate: {dep_rel:?} already exists");
        }
        record_dep(&ctx.conn, parent, DepKind::IfCreate, Some(&dep_rel), None)?;
    }
    Ok(())
}

/// `redo-msh always`: mark the current target as always out of date.
pub fn always(ctx: &Ctx) -> Result<()> {
    let parent = match &ctx.target {
        Some(p) => p,
        None => bail!("redo-msh always must be called from within a do-file"),
    };
    record_dep(&ctx.conn, parent, DepKind::Always, None, None)?;
    Ok(())
}

/// `redo-msh <targets...>`: force-build each target with `j` parallel jobs.
///
/// The top-level owns the live-log plumbing: it creates the run trace (locked
/// exclusively for our whole lifetime, so a concurrent run's GC can prove it
/// live), sweeps logs from dead runs, and runs the follower thread — the sole
/// writer to the terminal while we build (I1). Engine errors are returned to
/// `main` and printed only after the follower has drained.
pub fn redo(targets: &[String], j: usize) -> Result<()> {
    let mut ctx = Ctx::top_level(j)?;
    gc_temps(&ctx.root); // sweep temp files left by any crashed build
    // Sweep wait edges (and liveness locks) left by dead processes, so a
    // crashed traversal can never fabricate a cycle error in this run.
    crate::waits::gc_sweep(&ctx.root, &ctx.conn);

    // The liveness lock goes on a sentinel file that is never read or
    // written, NOT on the run log: Windows kernel locks are mandatory, so a
    // lock held on the log itself would block the follower's reads and every
    // event append, silently emptying the live display and hanging the join.
    let run_log = crate::logs::run_log_path(&ctx.root, ctx.session);
    let run_lock_path = crate::logs::run_lock_path(&ctx.root, ctx.session);
    fs::create_dir_all(crate::logs::logs_dir(&ctx.root))?;
    let run_lock = fs::File::create(&run_lock_path)
        .with_context(|| format!("creating run lock {}", run_lock_path.display()))?;
    fs2::FileExt::lock_exclusive(&run_lock)
        .with_context(|| format!("locking run lock {}", run_lock_path.display()))?;
    fs::File::create(&run_log)
        .with_context(|| format!("creating run log {}", run_log.display()))?;
    crate::logs::gc_logs(&ctx.root, ctx.session);
    ctx.log_sink = Some(run_log.clone());
    let follower = crate::logs::follow_start(ctx.root.clone(), ctx.session);

    let result = redo_build(&ctx, targets);

    // Terminate the run trace so the follower's root frame pops; join before
    // anyone else may print (I1). The run log itself is ours to remove — the
    // follower only deletes target logs.
    crate::logs::event_append(&run_log, &crate::logs::Event::Done {
        session: ctx.session,
        exit: if result.is_ok() { 0 } else { 1 },
        target: crate::logs::RUN_TARGET.to_string(),
    });
    follower.join();
    drop(run_lock);
    // The lock sentinel is bookkeeping, not a log: remove it regardless.
    let _ = fs::remove_file(&run_lock_path);
    if !crate::logs::keep_logs() {
        let _ = fs::remove_file(&run_log);
    }
    result
}

/// The build phase of a top-level `redo`, separated so `redo()` can run its
/// log epilogue on every outcome.
fn redo_build(ctx: &Ctx, targets: &[String]) -> Result<()> {
    // Force-build the requested targets in parallel through the task
    // registry. Dedup (case-folded, first occurrence wins): forcing the same
    // target twice in one session would rebuild it twice, and the follower
    // could consume the first build's log while the second is live at the
    // same path.
    let mut seen = HashSet::new();
    let mut rels: Vec<String> = Vec::with_capacity(targets.len());
    for t in targets {
        let rel = normalize_target(&ctx.root, t)?;
        if seen.insert(rel.to_ascii_lowercase()) {
            rels.push(rel);
        }
    }
    let result = crate::parallel::ensure_all(ctx, &rels, true);
    // Speculative dep tasks may still be settling; the run is not over (and
    // the log follower must not be terminated) until they are.
    crate::parallel::drain(ctx);
    result
}

// ---- introspection commands -------------------------------------------------

/// Open the database read-only-ish for an introspection command, without
/// starting a new session (no runid bump).
fn open_for_query() -> Result<(Root, Connection)> {
    let cwd = std::env::current_dir()?;
    let root = Root::discover(&cwd)?;
    let conn = root.open_db()?;
    Ok((root, conn))
}

/// `redo-msh sources`: list known source files (no do-file).
pub fn cmd_sources() -> Result<()> {
    let (_root, conn) = open_for_query()?;
    let mut stmt = conn.prepare("SELECT path FROM files WHERE dofile IS NULL ORDER BY path")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        println!("{}", row?);
    }
    Ok(())
}

/// `redo-msh targets`: list known generated targets.
pub fn cmd_targets() -> Result<()> {
    let (_root, conn) = open_for_query()?;
    let mut stmt = conn.prepare("SELECT path FROM files WHERE dofile IS NOT NULL ORDER BY path")?;
    for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
        println!("{}", row?);
    }
    Ok(())
}

/// `redo-msh ood`: list targets that are currently out of date (read-only; does
/// not build anything).
pub fn cmd_ood() -> Result<()> {
    let (root, conn) = open_for_query()?;
    let mut stmt = conn.prepare("SELECT path FROM files WHERE dofile IS NOT NULL ORDER BY path")?;
    let targets: Vec<String> = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    for t in targets {
        if is_ood_static(&root, &conn, &t)? {
            println!("{t}");
        }
    }
    Ok(())
}

/// `redo-msh tree [targets...]`: print the dependency tree recorded by the
/// last run. With no arguments, prints every root (a generated target no other
/// target depends on).
///
/// Two honest limits, both consequences of redo's design: dependencies are
/// discovered *while do-files run*, so the tree reflects the previous build,
/// not a plan for the next one; and the database does not record how deps
/// were grouped into `redo-ifchange` calls, so a node's fan-out is an upper
/// bound on the parallel width `-j` can achieve there (a do-file that calls
/// `redo-ifchange` once per dep gets no parallelism at all).
pub fn cmd_tree(args: &[String]) -> Result<()> {
    let (root, conn) = open_for_query()?;
    let roots: Vec<String> = if args.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT path FROM files WHERE dofile IS NOT NULL
             AND path NOT IN (SELECT dep FROM deps WHERE dep IS NOT NULL)
             ORDER BY path",
        )?;
        let roots = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<rusqlite::Result<_>>()?;
        roots
    } else {
        args.iter()
            .map(|a| normalize_target(&root, a))
            .collect::<Result<_>>()?
    };
    if roots.is_empty() {
        eprintln!("redo-msh: no recorded targets; run a build first");
        return Ok(());
    }
    // Case-folded to match the database's NOCASE identity; a target expands
    // once, later occurrences reference it instead of repeating its subtree
    // (this also terminates on any stale cyclic edges).
    let mut expanded: HashSet<String> = HashSet::new();
    for (i, t) in roots.iter().enumerate() {
        if i > 0 {
            println!();
        }
        if files_row(&conn, t)?.is_none() && read_deps(&conn, t)?.is_empty() {
            println!("{t} (nothing recorded: never built by redo)");
            continue;
        }
        expanded.insert(t.to_ascii_lowercase());
        println!("{}", tree_node_line(&conn, t)?);
        print_tree(&conn, t, "", &mut expanded)?;
    }
    Ok(())
}

/// One node's display line: the target, its recorded do-file, and its
/// ifchange fan-out (the upper bound on parallel width at this node).
fn tree_node_line(conn: &Connection, target: &str) -> Result<String> {
    let dofile: Option<String> = conn
        .query_row(
            "SELECT dofile FROM files WHERE path = ?1",
            params![target],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let width: i64 = conn.query_row(
        "SELECT COUNT(*) FROM deps WHERE target = ?1 AND kind = ?2",
        params![target, DepKind::IfChange as i64],
        |r| r.get(0),
    )?;
    let mut s = target.to_string();
    if let Some(df) = dofile {
        s.push_str(&format!(" [{df}]"));
    }
    if width > 1 {
        s.push_str(&format!(" ({width} deps: parallel width up to {width})"));
    }
    Ok(s)
}

/// Recursively print `target`'s recorded dependency edges beneath `prefix`.
fn print_tree(
    conn: &Connection,
    target: &str,
    prefix: &str,
    expanded: &mut HashSet<String>,
) -> Result<()> {
    // (display label, Some(dep) if the child is a generated target to recurse
    // into). The do-file edge is shown on the node line, not as a child.
    let mut children: Vec<(String, Option<String>)> = Vec::new();
    for (kind, dep, _csum) in read_deps(conn, target)? {
        match kind {
            DepKind::DoFile => {}
            DepKind::Always => {
                children.push(("(always: rebuilt every run)".to_string(), None));
            }
            DepKind::Uncommitted => {
                children.push((
                    "(last build failed or crashed before committing)".to_string(),
                    None,
                ));
            }
            DepKind::IfCreate => {
                let d = dep.expect("ifcreate dep has a path");
                children.push((format!("{d} (must stay absent)"), None));
            }
            DepKind::IfChange => {
                let d = dep.expect("ifchange dep has a path");
                if is_generated(conn, &d)? {
                    children.push((d.clone(), Some(d)));
                } else {
                    children.push((format!("{d} (source)"), None));
                }
            }
        }
    }
    let last = children.len().saturating_sub(1);
    for (i, (label, recurse)) in children.into_iter().enumerate() {
        let (branch, cont) = if i == last {
            ("└── ", "    ")
        } else {
            ("├── ", "│   ")
        };
        match recurse {
            Some(dep) if expanded.insert(dep.to_ascii_lowercase()) => {
                println!("{prefix}{branch}{}", tree_node_line(conn, &dep)?);
                print_tree(conn, &dep, &format!("{prefix}{cont}"), expanded)?;
            }
            Some(_) => println!("{prefix}{branch}{label} (shown above)"),
            None => println!("{prefix}{branch}{label}"),
        }
    }
    Ok(())
}

/// Read-only out-of-date check: like `is_ood` but never builds dependencies
/// (used by `redo-msh ood`). Compares recorded edge hashes to current on-disk
/// content without bringing dependencies up to date first.
fn is_ood_static(root: &Root, conn: &Connection, target: &str) -> Result<bool> {
    // This command runs standalone (no build, no follower), so verbose
    // reasons go straight to stderr; the target list stays clean on stdout.
    let why = |msg: &str| {
        if verbose() {
            eprintln!("redo-msh -v: {target}: {msg}");
        }
    };
    let built_csum = match files_row(conn, target)? {
        Some(c) => c,
        None => {
            why("OUT OF DATE: redo has no record of ever building it");
            return Ok(true);
        }
    };
    let target_abs = root.dir.join(target);
    if built_csum.is_some() && !target_abs.exists() {
        why("OUT OF DATE: the previously built output is missing from disk");
        return Ok(true);
    }
    for (kind, dep, edge_csum) in read_deps(conn, target)? {
        match kind {
            DepKind::Always => {
                why("OUT OF DATE: its do-file called redo-always");
                return Ok(true);
            }
            DepKind::Uncommitted => {
                why("OUT OF DATE: the last build failed or crashed before committing");
                return Ok(true);
            }
            DepKind::IfCreate => {
                let dep = dep.expect("ifcreate path");
                if root.dir.join(&dep).exists() {
                    why(&format!("OUT OF DATE: {dep} now exists (ifcreate dependency)"));
                    return Ok(true);
                }
                why(&format!("ifcreate dependency {dep} is still absent"));
            }
            DepKind::DoFile | DepKind::IfChange => {
                let dep = dep.expect("dep path");
                let cur = stamp::stamp_file(&root.dir.join(&dep))?.map(|s| s.csum);
                if cur.as_deref() != edge_csum.as_deref() {
                    why(&format!(
                        "OUT OF DATE: dependency {dep} changed (hash was {} at last \
                         build, is now {})",
                        hash8(edge_csum.as_deref()),
                        hash8(cur.as_deref())
                    ));
                    return Ok(true);
                }
                why(&format!("dependency {dep} unchanged (hash {})", hash8(cur.as_deref())));
            }
        }
    }
    why("UP TO DATE: all recorded dependencies are unchanged");
    Ok(false)
}

/// Build a detailed error for a failed interpreter spawn. The bare OS error
/// ("program not found") names neither the missing program nor where we
/// looked, which reads as if the do-file itself were the problem. Spell out
/// exactly what we tried to run and the PATH we used to find it.
fn interpreter_spawn_error(
    err: std::io::Error,
    interp: &[String],
    dofile_abs: &Path,
    child_path: Option<&std::ffi::OsStr>,
) -> anyhow::Error {
    let interp0 = &interp[0];
    let mut msg = format!(
        "cannot run do-file {}\n\n\
         redo-msh does not execute do-files directly: every do-file is run by an\n\
         interpreter ('msh' by default, or whatever redo.toml configures). Here that\n\
         interpreter is '{}' (full command: {} {}), and spawning it failed:\n\n  {}\n",
        dofile_abs.display(),
        interp0,
        interp.join(" "),
        dofile_abs.display(),
        err,
    );
    if err.kind() == std::io::ErrorKind::NotFound {
        msg.push_str(&format!(
            "\n'{}' was not found in any directory on PATH. PATH as used for this spawn:\n",
            interp0
        ));
        // Prefer the PATH we actually set on the child (used for lookup on
        // Windows); fall back to our own environment's PATH.
        let path_os = child_path
            .map(|p| p.to_os_string())
            .or_else(|| std::env::var_os("PATH"));
        match path_os {
            Some(p) => {
                for dir in std::env::split_paths(&p) {
                    msg.push_str(&format!("  {}\n", dir.display()));
                }
            }
            None => msg.push_str("  (PATH is not set)\n"),
        }
        msg.push_str(&format!(
            "\nTo fix: install '{}' and make sure its directory is on PATH, or set a\n\
             different interpreter in redo.toml at the project root.",
            interp0
        ));
    }
    anyhow::anyhow!(msg)
}

/// The child do-file's `PATH` with the running executable's own directory
/// prepended, so bare `redo`/`redo-ifchange`/... (which ship beside `redo-msh`)
/// resolve without a system-wide install. Returns `None` if we can't locate
/// our own exe, in which case the child inherits the unmodified `PATH`.
fn child_path() -> Option<std::ffi::OsString> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.to_path_buf();
    let mut paths = vec![dir];
    if let Some(p) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(paths).ok()
}

/// `redo-stamp`: drain stdin and exit successfully.
///
/// In apenwarr redo, `redo-stamp` opts a target into content-based change
/// detection by hashing the bytes piped to it. redo-msh already content-hashes
/// every committed output by default, so that behavior is the baseline and this
/// is effectively a no-op. We still consume stdin to EOF so the upstream
/// writer (`... | redo-stamp`) never takes a broken pipe. (Targets with
/// nondeterministic output that relied on a stable stamp will simply rebuild
/// their dependents — correct, just not optimized.)
pub fn stamp() -> Result<()> {
    let mut sink = std::io::sink();
    let _ = std::io::copy(&mut std::io::stdin().lock(), &mut sink);
    Ok(())
}

/// Make a path safe to embed in a temp filename.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarse_mtime_is_never_trusted() {
        // Whole-second mtimes (FAT-style coarse resolution) force a re-hash.
        assert!(!stat_is_trustworthy(0));
        assert!(!stat_is_trustworthy(1_700_000_000_000_000_000));
        // Sub-second precision proves a fine-resolution FS; stat may be trusted.
        assert!(stat_is_trustworthy(1_700_000_000_000_000_001));
        assert!(stat_is_trustworthy(1_700_000_000_123_456_789));
    }
}

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
use crate::jobserver::{Jobserver, TokenSrc};
use crate::root::Root;
use crate::stamp::{self, Stamp};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Arc};
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
    /// Targets currently being ensured in *this* process, for in-process cycle
    /// detection during out-of-date traversal.
    pub active: RefCell<HashSet<String>>,
    /// Shared concurrency limiter for parallel builds.
    pub jobs: Arc<Jobserver>,
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
        let cwd = std::env::current_dir()?;
        let root = Root::discover(&cwd)?;
        let conn = root.open_db()?;
        let session = db::next_runid(&conn)?;
        Ok(Ctx {
            root,
            conn,
            session,
            depth: 0,
            chain: Vec::new(),
            target: None,
            active: RefCell::new(HashSet::new()),
            jobs: Arc::new(Jobserver::init_top(j.max(1))),
        })
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
        Ok(Ctx {
            root,
            conn,
            session,
            depth,
            chain,
            target,
            active: RefCell::new(HashSet::new()),
            jobs: Arc::new(Jobserver::from_env()),
        })
    }

    /// A context for building one dependency on a worker thread: same
    /// root/session/chain/target and shared jobserver, but its own SQLite
    /// connection (rusqlite connections are not shareable across threads) and a
    /// fresh in-process cycle set.
    fn child_for_thread(&self) -> Result<Ctx> {
        Ok(Ctx {
            root: self.root.clone(),
            conn: self.root.open_db()?,
            session: self.session,
            depth: self.depth,
            chain: self.chain.clone(),
            target: self.target.clone(),
            active: RefCell::new(HashSet::new()),
            jobs: self.jobs.clone(),
        })
    }

    fn log(&self, msg: &str) {
        eprintln!("redo-msh: {}{}", "  ".repeat(self.depth), msg);
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

fn file_runid(conn: &Connection, path: &str) -> Result<Option<i64>> {
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

/// Atomically replace `dst` with `src` (same volume by construction).
fn atomic_rename(src: &Path, dst: &Path) -> Result<()> {
    // std::fs::rename replaces an existing destination on both Unix (rename(2))
    // and Windows (MoveFileEx). Windows-specific hardening lands in M6.
    fs::rename(src, dst).with_context(|| {
        format!("renaming {} -> {}", src.display(), dst.display())
    })
}

// ---- the build operation ----------------------------------------------------

/// Build `target_rel`, acquiring the per-target lock for build exclusion.
///
/// The cross-process cycle check runs *before* locking (so a cycle errors
/// instead of deadlocking). After acquiring the lock we re-check whether the
/// target was built this session by whoever held the lock before us — the
/// double-checked rebuild that makes concurrent builds safe. `force` (top-level
/// `redo`) skips that check and always rebuilds.
pub fn build(ctx: &Ctx, target_rel: &str, force: bool) -> Result<()> {
    if ctx.chain.iter().any(|t| t == target_rel) {
        bail!(
            "dependency cycle detected: {} -> {}",
            ctx.chain.join(" -> "),
            target_rel
        );
    }
    let _lock = crate::lock::lock_target(&ctx.root, target_rel)?;
    if !force && file_runid(&ctx.conn, target_rel)? == Some(ctx.session) {
        // Built by another process while we waited for the lock.
        return Ok(());
    }
    build_inner(ctx, target_rel)
}

/// Run the target's do-file and commit its output. Assumes the per-target lock
/// is held and a build has been determined necessary.
fn build_inner(ctx: &Ctx, target_rel: &str) -> Result<()> {
    let (dofile_opt, absent) = dofile::find(&ctx.root, target_rel);
    let df = match dofile_opt {
        Some(d) => d,
        None => bail!("no .do file found to build target {target_rel:?}"),
    };
    ctx.log(&format!("redo {target_rel}"));

    // Reset dependency edges; the do-file re-declares them as it runs.
    clear_deps(&ctx.conn, target_rel)?;
    let df_stamp = stamp::stamp_file(&df.dofile_abs)?;
    record_dep(&ctx.conn, target_rel, DepKind::DoFile, Some(&df.dofile_rel), df_stamp.as_ref())?;
    for a in &absent {
        record_dep(&ctx.conn, target_rel, DepKind::IfCreate, Some(a), None)?;
    }

    let target_abs = ctx.root.dir.join(target_rel);
    let before_t = fs::metadata(&target_abs).ok();

    // Temp output ($3), beside the target for same-volume atomic rename.
    let pid = std::process::id();
    let tmp3_rel = format!("{}.redo-tmp.{}", df.arg_target, pid);
    let tmp3_abs = df.dodir_abs.join(&tmp3_rel);
    let _ = fs::remove_file(&tmp3_abs);

    // Separate capture file for the do-file's stdout.
    let tmp_dir = ctx.root.redo_dir().join("tmp");
    fs::create_dir_all(&tmp_dir)?;
    let capture_path = tmp_dir.join(format!("stdout.{}.{}", pid, sanitize(target_rel)));
    let capture_file = fs::File::create(&capture_path)
        .with_context(|| format!("creating stdout capture {}", capture_path.display()))?;

    // Child environment.
    let mut child_chain = ctx.chain.clone();
    child_chain.push(target_rel.to_string());
    let status = Command::new("msh")
        .arg(&df.dofile_abs)
        .arg(&df.arg_target)
        .arg(&df.arg_base)
        .arg(&tmp3_rel)
        .current_dir(&df.dodir_abs)
        .stdout(capture_file)
        .env(E_ROOT, &ctx.root.dir)
        .env(E_TARGET, target_rel)
        .env(E_SESSION, ctx.session.to_string())
        .env(E_DEPTH, (ctx.depth + 1).to_string())
        .env(E_CHAIN, child_chain.join("\n"))
        .status()
        .with_context(|| format!("running do-file {}", df.dofile_abs.display()))?;

    let after_t = fs::metadata(&target_abs).ok();
    let cap_size = fs::metadata(&capture_path).map(|m| m.len()).unwrap_or(0);
    let tmp3_exists = tmp3_abs.exists();

    let cleanup = || {
        let _ = fs::remove_file(&tmp3_abs);
        let _ = fs::remove_file(&capture_path);
    };

    // The do-file must not write the target ($1) directly.
    let modified_directly = match (&before_t, &after_t) {
        (_, None) => false,
        (None, Some(at)) => !at.is_dir(),
        (Some(bt), Some(at)) => !at.is_dir() && bt.modified().ok() != at.modified().ok(),
    };
    if modified_directly {
        cleanup();
        bail!("do-file for {target_rel} modified the target directly; write to $3 or stdout, not $1");
    }
    if !status.success() {
        cleanup();
        bail!(
            "do-file for {target_rel} failed (exit {})",
            status.code().map(|c| c.to_string()).unwrap_or_else(|| "signal".into())
        );
    }
    if tmp3_exists && cap_size > 0 {
        cleanup();
        bail!("do-file for {target_rel} wrote to both stdout and $3; choose one (status messages go to stderr)");
    }

    // Commit the output.
    if cap_size > 0 && !tmp3_exists {
        fs::copy(&capture_path, &tmp3_abs)
            .with_context(|| format!("copying stdout to {}", tmp3_abs.display()))?;
    }
    let _ = fs::remove_file(&capture_path);

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

    // Record the post-build stamp; this is the "target is built" marker.
    let stamp = stamp::stamp_file(&target_abs)?;
    upsert_file(
        &ctx.conn,
        target_rel,
        Some(&df.dofile_rel),
        Some(now_nanos()),
        stamp.as_ref(),
        ctx.session,
    )?;
    Ok(())
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
        let buildable = dofile::find(&ctx.root, &dep_rel).0.is_some();
        if !buildable && !dep_abs.exists() {
            bail!("no .do file and no source file for dependency {dep_rel:?}");
        }
        items.push((dep_rel, dep_abs, buildable));
    }

    // Source files: stamp/refresh now (cheap, serial).
    for (dep_rel, dep_abs, buildable) in &items {
        if !buildable {
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
    if let Some(parent) = &ctx.target {
        for (dep_rel, dep_abs, _) in &items {
            let stamp = stamp::stamp_file(dep_abs)?;
            record_dep(&ctx.conn, parent, DepKind::IfChange, Some(dep_rel), stamp.as_ref())?;
        }
    }
    Ok(())
}

/// Bring multiple buildable targets up to date, in parallel, bounded by the
/// jobserver. Uses the process's own token for one job (guaranteeing progress)
/// and try-acquires extra pool tokens for the rest; completions are awaited via
/// a channel, and each pool token is returned as its job finishes.
fn build_parallel(ctx: &Ctx, deps: &[String]) -> Result<()> {
    if deps.len() <= 1 {
        // No parallelism opportunity: run inline, consuming no extra tokens.
        for d in deps {
            ensure(ctx, d)?;
        }
        return Ok(());
    }

    let (tx, rx) = mpsc::channel::<(Result<()>, TokenSrc)>();
    let n = deps.len();
    let mut idx = 0;
    let mut running = 0usize;
    let mut own_in_use = false;
    let mut first_err: Option<anyhow::Error> = None;

    loop {
        // Launch as many jobs as we have tokens for (stop launching on error).
        while idx < n && first_err.is_none() {
            let src = if !own_in_use {
                own_in_use = true;
                TokenSrc::Own
            } else if ctx.jobs.try_acquire() {
                TokenSrc::Pool
            } else {
                break;
            };
            let cctx = ctx.child_for_thread()?;
            let dep = deps[idx].clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let r = ensure(&cctx, &dep);
                let _ = tx.send((r, src));
            });
            idx += 1;
            running += 1;
        }

        if running == 0 {
            break;
        }
        // Wait for one job to finish and reclaim its token.
        let (res, src) = rx.recv().expect("build worker channel closed");
        running -= 1;
        match src {
            TokenSrc::Own => own_in_use = false,
            TokenSrc::Pool => ctx.jobs.release(),
        }
        if let Err(e) = res {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }

    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Bring a buildable `target` up to date: build it if out of date, otherwise
/// mark it verified for this session. Idempotent within a session.
pub fn ensure(ctx: &Ctx, target: &str) -> Result<()> {
    // Already built or verified this session.
    if file_runid(&ctx.conn, target)? == Some(ctx.session) {
        return Ok(());
    }
    // In-process cycle guard for the out-of-date traversal (the cross-process
    // chain guard lives in build()).
    if !ctx.active.borrow_mut().insert(target.to_string()) {
        bail!("dependency cycle detected involving {target}");
    }
    let result = (|| {
        if is_ood(ctx, target)? {
            build(ctx, target, false)
        } else {
            mark_verified(ctx, target)
        }
    })();
    ctx.active.borrow_mut().remove(target);
    result
}

/// Decide whether a buildable `target` is out of date.
fn is_ood(ctx: &Ctx, target: &str) -> Result<bool> {
    let target_abs = ctx.root.dir.join(target);
    let row = files_row(&ctx.conn, target)?;
    let built_csum = match row {
        Some(c) => c,
        None => return Ok(true), // never built
    };
    // A previously-produced file that is now gone must be rebuilt. (A phony
    // target has no recorded csum, so its absence is expected.)
    if built_csum.is_some() && !target_abs.exists() {
        return Ok(true);
    }

    for (kind, dep, edge_csum) in read_deps(&ctx.conn, target)? {
        match kind {
            DepKind::Always => return Ok(true),
            DepKind::DoFile => {
                let dep = dep.expect("dofile dep has a path");
                if current_csum(ctx, &dep)?.as_deref() != edge_csum.as_deref() {
                    return Ok(true);
                }
            }
            DepKind::IfCreate => {
                let dep = dep.expect("ifcreate dep has a path");
                if ctx.root.dir.join(&dep).exists() {
                    return Ok(true);
                }
            }
            DepKind::IfChange => {
                let dep = dep.expect("ifchange dep has a path");
                // Bring the dependency up to date first (it may be a target).
                let (df, _) = dofile::find(&ctx.root, &dep);
                if df.is_some() {
                    ensure(ctx, &dep)?;
                }
                match current_csum(ctx, &dep)? {
                    None => return Ok(true), // dependency disappeared
                    Some(cur) => {
                        if Some(cur.as_str()) != edge_csum.as_deref() {
                            return Ok(true);
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Mark a target as verified-up-to-date this session and refresh its stamp
/// cache, so later `ensure` calls in the same session short-circuit.
fn mark_verified(ctx: &Ctx, target: &str) -> Result<()> {
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
fn current_csum(ctx: &Ctx, path_rel: &str) -> Result<Option<String>> {
    let abs = ctx.root.dir.join(path_rel);
    let meta = match fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("stat {path_rel}"))),
    };
    let mtime = stamp::mtime_nanos(&meta);
    let size = if meta.is_dir() { 0 } else { meta.len() as i64 };

    if let Some((rm, rs, rc)) = files_stamp(&ctx.conn, path_rel)? {
        if rm == mtime && rs == size && stat_is_trustworthy(mtime) {
            return Ok(Some(rc));
        }
    }

    let csum = if meta.is_dir() {
        stamp::DIR_CSUM.to_string()
    } else {
        stamp::hash_file(&abs)?
    };
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

/// Fetch a file row's recorded csum if the row exists. Returns
/// `Some(None)` for a phony target (row exists, csum NULL).
fn files_row(conn: &Connection, path: &str) -> Result<Option<Option<String>>> {
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
fn read_deps(
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
pub fn redo(targets: &[String], j: usize) -> Result<()> {
    let ctx = Ctx::top_level(j)?;
    // Force-build the requested targets in parallel under the jobserver. We
    // mark them verified-this-session via a forced build, so reuse the parallel
    // scheduler but bypass the up-to-date check by clearing their session mark.
    if targets.len() <= 1 {
        for input in targets {
            let target_rel = normalize_target(&ctx.root, input)?;
            build(&ctx, &target_rel, true)?;
        }
        return Ok(());
    }
    // Parallel forced top-level builds.
    let rels: Vec<String> = targets
        .iter()
        .map(|t| normalize_target(&ctx.root, t))
        .collect::<Result<_>>()?;
    build_parallel_forced(&ctx, &rels)?;
    Ok(())
}

/// Like `build_parallel` but forces each target (top-level `redo`).
fn build_parallel_forced(ctx: &Ctx, targets: &[String]) -> Result<()> {
    let (tx, rx) = mpsc::channel::<(Result<()>, TokenSrc)>();
    let n = targets.len();
    let (mut idx, mut running, mut own_in_use) = (0usize, 0usize, false);
    let mut first_err: Option<anyhow::Error> = None;
    loop {
        while idx < n && first_err.is_none() {
            let src = if !own_in_use {
                own_in_use = true;
                TokenSrc::Own
            } else if ctx.jobs.try_acquire() {
                TokenSrc::Pool
            } else {
                break;
            };
            let cctx = ctx.child_for_thread()?;
            let target = targets[idx].clone();
            let tx = tx.clone();
            thread::spawn(move || {
                let r = build(&cctx, &target, true);
                let _ = tx.send((r, src));
            });
            idx += 1;
            running += 1;
        }
        if running == 0 {
            break;
        }
        let (res, src) = rx.recv().expect("build worker channel closed");
        running -= 1;
        match src {
            TokenSrc::Own => own_in_use = false,
            TokenSrc::Pool => ctx.jobs.release(),
        }
        if let Err(e) = res {
            if first_err.is_none() {
                first_err = Some(e);
            }
        }
    }
    match first_err {
        Some(e) => Err(e),
        None => Ok(()),
    }
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

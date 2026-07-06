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
use std::io::IsTerminal;
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
    /// Per-project interpreter configuration (`redo.toml`).
    pub config: crate::config::Config,
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
        crate::logs::init_top();
        let cwd = std::env::current_dir()?;
        let root = Root::discover(&cwd)?;
        let conn = root.open_db()?;
        let session = db::next_runid(&conn)?;
        let config = crate::config::Config::load(&root.dir)?;
        Ok(Ctx {
            root,
            conn,
            session,
            depth: 0,
            chain: Vec::new(),
            target: None,
            active: RefCell::new(HashSet::new()),
            jobs: Arc::new(Jobserver::init_top(j.max(1))),
            config,
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
        let config = crate::config::Config::load(&root.dir)?;
        Ok(Ctx {
            root,
            conn,
            session,
            depth,
            chain,
            target,
            active: RefCell::new(HashSet::new()),
            jobs: Arc::new(Jobserver::from_env()),
            config,
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
            config: self.config.clone(),
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

/// Whether `path` should be treated as a buildable target. Mirrors apenwarr
/// redo's static-file rule (builder.py `_start_self`, from djb's notes): a
/// file that exists on disk and was *not* generated by redo is a source, even
/// when a `default.*.do` would match — `default.csv.do` must not try to
/// rebuild a hand-written `data.csv`.
fn is_target(ctx: &Ctx, path_rel: &str) -> Result<bool> {
    let abs = ctx.root.dir.join(path_rel);
    let exists_as_file = fs::metadata(&abs).map(|m| !m.is_dir()).unwrap_or(false);
    if exists_as_file && !is_generated(&ctx.conn, path_rel)? {
        return Ok(false);
    }
    Ok(dofile::find(&ctx.root, path_rel).0.is_some())
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
fn atomic_rename(src: &Path, dst: &Path) -> Result<()> {
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

/// Refuse to silently overwrite a target whose on-disk content differs from
/// what we recorded after building it (i.e. it was edited by hand). With
/// `--yes` (env `REDO_YES=1`) we overwrite; on a tty we ask; otherwise we fail.
fn check_not_hand_edited(ctx: &Ctx, target_rel: &str, target_abs: &Path) -> Result<()> {
    if !target_abs.exists() {
        return Ok(());
    }
    // Only generated files (a row with a recorded content hash) can be "edited
    // since built"; sources and phony targets are exempt.
    let recorded = match files_row(&ctx.conn, target_rel)? {
        Some(Some(c)) => c,
        _ => return Ok(()),
    };
    let current = stamp::hash_file(target_abs)?;
    if current == recorded {
        return Ok(());
    }
    // Hand-edited.
    if std::env::var("REDO_YES").as_deref() == Ok("1") {
        eprintln!("redo-msh: {target_rel} was modified by hand; overwriting (--yes)");
        return Ok(());
    }
    if std::io::stderr().is_terminal() && prompt_overwrite(target_rel)? {
        return Ok(());
    }
    bail!(
        "{target_rel} was modified by hand since it was last built; refusing to overwrite. \
         Pass --yes to overwrite, or move your changes out of the way."
    )
}

/// Ask on the controlling terminal whether to overwrite a hand-edited target.
fn prompt_overwrite(target_rel: &str) -> Result<bool> {
    use std::io::{BufRead, Write};
    #[cfg(unix)]
    let tty = fs::OpenOptions::new().read(true).write(true).open("/dev/tty");
    #[cfg(not(unix))]
    let tty: std::io::Result<fs::File> = Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "no controlling tty",
    ));
    let mut tty = match tty {
        Ok(f) => f,
        Err(_) => return Ok(false),
    };
    write!(tty, "redo-msh: {target_rel} was modified by hand. Overwrite? [y/N] ")?;
    tty.flush()?;
    let mut reader = std::io::BufReader::new(tty);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let ans = line.trim().to_ascii_lowercase();
    Ok(ans == "y" || ans == "yes")
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
    if ctx.chain.iter().any(|t| t.eq_ignore_ascii_case(target_rel)) {
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
    let target_abs = ctx.root.dir.join(target_rel);

    // An existing file that redo never generated is a source, even when a
    // do-file matches (djb's rule, apenwarr builder.py `_start_self`): record
    // it as static and do nothing.
    let exists = target_abs.exists();
    let exists_as_file = fs::metadata(&target_abs).map(|m| !m.is_dir()).unwrap_or(false);
    if exists_as_file && !is_generated(&ctx.conn, target_rel)? {
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
                let stamp = stamp::stamp_file(&target_abs)?;
                upsert_file(&ctx.conn, target_rel, None, None, stamp.as_ref(), ctx.session)?;
                return Ok(());
            }
            bail!("no .do file found to build target {target_rel:?}")
        }
    };

    // Reset dependency edges; the do-file re-declares them as it runs.
    clear_deps(&ctx.conn, target_rel)?;
    // Mark the build as in flight. The marker is removed only after a
    // successful commit, so a failed (or crashed) build leaves the target
    // unconditionally out of date — otherwise a later run would trust a
    // leftover output file from an earlier success plus the fresh edges
    // recorded below, and accept the target as up to date.
    record_dep(&ctx.conn, target_rel, DepKind::Uncommitted, None, None)?;
    let df_stamp = stamp::stamp_file(&df.dofile_abs)?;
    record_dep(&ctx.conn, target_rel, DepKind::DoFile, Some(&df.dofile_rel), df_stamp.as_ref())?;
    for a in &absent {
        record_dep(&ctx.conn, target_rel, DepKind::IfCreate, Some(a), None)?;
    }

    let before_t = fs::metadata(&target_abs).ok();

    // Refuse to silently clobber a target that was modified by hand since we
    // last built it.
    check_not_hand_edited(ctx, target_rel, &target_abs)?;

    // Temp output ($3), beside the target for same-volume atomic rename. Sweep
    // any stale temps for this target from a previously crashed build.
    clean_target_temps(&df.dodir_abs, &df.arg_target);
    let pid = std::process::id();
    let tmp3_rel = format!("{}.redo-tmp.{}", df.arg_target, pid);
    let tmp3_abs = df.dodir_abs.join(&tmp3_rel);

    // Separate capture files for the do-file's stdout (output detection) and
    // stderr (diagnostics, emitted as one contiguous log block at completion).
    let tmp_dir = ctx.root.redo_dir().join("tmp");
    fs::create_dir_all(&tmp_dir)?;
    let capture_path = tmp_dir.join(format!("stdout.{}.{}", pid, sanitize(target_rel)));
    let errlog_path = tmp_dir.join(format!("stderr.{}.{}", pid, sanitize(target_rel)));

    // All temps are removed on every exit path (return, `?`, panic).
    let _guard = TempGuard {
        paths: vec![tmp3_abs.clone(), capture_path.clone(), errlog_path.clone()],
    };
    let capture_file = fs::File::create(&capture_path)
        .with_context(|| format!("creating stdout capture {}", capture_path.display()))?;
    let errlog_file = fs::File::create(&errlog_path)
        .with_context(|| format!("creating stderr capture {}", errlog_path.display()))?;

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
        .stdout(capture_file)
        .stderr(errlog_file)
        .env(E_ROOT, &ctx.root.dir)
        .env(E_TARGET, target_rel)
        .env(E_SESSION, ctx.session.to_string())
        .env(E_DEPTH, (ctx.depth + 1).to_string())
        .env(E_CHAIN, child_chain.join("\n"));

    // Make the redo command family resolvable from inside the do-file by
    // prepending our own directory (where `redo`, `redo-ifchange`, ... ship
    // alongside `redo-msh`) to the child's PATH.
    if let Some(path) = child_path() {
        command.env("PATH", path);
    }

    let status = command
        .status()
        .with_context(|| format!("running do-file {}", df.dofile_abs.display()))?;

    // Emit this target's log block (header + captured stderr) as one
    // non-interleaved unit, streamed in completion order and indented by depth.
    let header = format!(
        "{}redo  {}{}",
        "  ".repeat(ctx.depth),
        target_rel,
        if status.success() { "" } else { "  (failed)" }
    );
    crate::logs::emit_block(&ctx.root.redo_dir(), &header, &errlog_path);

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
    // Successful commit: drop the in-flight marker recorded before the run.
    ctx.conn.execute(
        "DELETE FROM deps WHERE target = ?1 AND kind = ?2",
        params![target_rel, DepKind::Uncommitted as i64],
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
        let buildable = is_target(ctx, &dep_rel)?;
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
    // chain guard lives in build()). Fold case to match NOCASE identity.
    let active_key = target.to_ascii_lowercase();
    if !ctx.active.borrow_mut().insert(active_key.clone()) {
        bail!("dependency cycle detected involving {target}");
    }
    let result = (|| {
        if is_ood(ctx, target)? {
            build(ctx, target, false)
        } else {
            mark_verified(ctx, target)
        }
    })();
    ctx.active.borrow_mut().remove(&active_key);
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
            // Last build never committed (failed or crashed).
            DepKind::Uncommitted => return Ok(true),
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
                if is_target(ctx, &dep)? {
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
    gc_temps(&ctx.root); // sweep temp files left by any crashed build
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

/// Read-only out-of-date check: like `is_ood` but never builds dependencies
/// (used by `redo-msh ood`). Compares recorded edge hashes to current on-disk
/// content without bringing dependencies up to date first.
fn is_ood_static(root: &Root, conn: &Connection, target: &str) -> Result<bool> {
    let built_csum = match files_row(conn, target)? {
        Some(c) => c,
        None => return Ok(true),
    };
    let target_abs = root.dir.join(target);
    if built_csum.is_some() && !target_abs.exists() {
        return Ok(true);
    }
    for (kind, dep, edge_csum) in read_deps(conn, target)? {
        match kind {
            DepKind::Always | DepKind::Uncommitted => return Ok(true),
            DepKind::IfCreate => {
                if root.dir.join(dep.expect("ifcreate path")).exists() {
                    return Ok(true);
                }
            }
            DepKind::DoFile | DepKind::IfChange => {
                let dep = dep.expect("dep path");
                let cur = stamp::stamp_file(&root.dir.join(&dep))?.map(|s| s.csum);
                if cur.as_deref() != edge_csum.as_deref() {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
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

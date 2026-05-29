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
use crate::root::Root;
use crate::stamp::{self, Stamp};
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
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
    /// Ancestor target chain (root-relative) for cycle detection.
    pub chain: Vec<String>,
    /// The target whose do-file we are currently inside (`REDO_TARGET`), if any.
    pub target: Option<String>,
}

const E_ROOT: &str = "REDO_ROOT";
const E_TARGET: &str = "REDO_TARGET";
const E_SESSION: &str = "REDO_SESSION";
const E_DEPTH: &str = "REDO_DEPTH";
const E_CHAIN: &str = "REDO_CHAIN";

impl Ctx {
    /// Build a context for a top-level `redo` invocation: discover the root and
    /// start a fresh session.
    pub fn top_level() -> Result<Ctx> {
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
            None => return Ctx::top_level(),
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

/// Build `target_rel` (root-relative) by running its do-file. Always rebuilds
/// (the caller decides whether a build is warranted).
pub fn build(ctx: &Ctx, target_rel: &str) -> Result<()> {
    if ctx.chain.iter().any(|t| t == target_rel) {
        bail!(
            "dependency cycle detected: {} -> {}",
            ctx.chain.join(" -> "),
            target_rel
        );
    }

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

/// `redo-msh ifchange <deps...>`: ensure each dep is built (if it has a
/// do-file) and record the dependency edge from the current `REDO_TARGET`.
pub fn ifchange(ctx: &Ctx, inputs: &[String]) -> Result<()> {
    for input in inputs {
        let dep_rel = normalize_target(&ctx.root, input)?;
        let dep_abs = ctx.root.dir.join(&dep_rel);
        let (dofile_opt, _absent) = dofile::find(&ctx.root, &dep_rel);

        if dofile_opt.is_some() {
            // Buildable target: build unless already built this session.
            if file_runid(&ctx.conn, &dep_rel)? != Some(ctx.session) {
                build(ctx, &dep_rel)?;
            }
        } else {
            // Source file: must exist; record/refresh its stamp.
            if !dep_abs.exists() {
                bail!("no .do file and no source file for dependency {dep_rel:?}");
            }
            let stamp = stamp::stamp_file(&dep_abs)?;
            upsert_file(&ctx.conn, &dep_rel, None, None, stamp.as_ref(), ctx.session)?;
        }

        // Record the edge from the parent target, if we are inside a build.
        if let Some(parent) = &ctx.target {
            let stamp = stamp::stamp_file(&dep_abs)?;
            record_dep(&ctx.conn, parent, DepKind::IfChange, Some(&dep_rel), stamp.as_ref())?;
        }
    }
    Ok(())
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

/// `redo-msh <targets...>`: force-build each target.
pub fn redo(targets: &[String]) -> Result<()> {
    let ctx = Ctx::top_level()?;
    for input in targets {
        let target_rel = normalize_target(&ctx.root, input)?;
        build(&ctx, &target_rel)?;
    }
    Ok(())
}

/// Make a path safe to embed in a temp filename.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

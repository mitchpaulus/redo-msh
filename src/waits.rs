//! The shared waits-for graph: cross-worker, cross-process cycle detection.
//!
//! This implements the mechanism specified and model-checked in
//! `verification/ParallelEnsure.tla` and `verification/Speculation.tla`. The
//! chain check (`REDO_CHAIN`) is path-local and cannot see a cycle entered
//! concurrently from two points (`CycleLock_CycleParallel` finds the
//! resulting deadlock); this graph generalizes it across threads and
//! processes.
//!
//! An edge `waiter -> dep` means "some traversal of `waiter` is about to
//! block until `dep` settles". Before the wait starts, [`try_wait`] runs the
//! cycle-reachability check (is `waiter` reachable from `dep` through live
//! edges?) and the edge insert as ONE SQLite write transaction — the
//! atomicity the verified design requires (implementation contract item 1:
//! two concurrent inserts that each miss the other's edge would recreate the
//! deadlock). Speculative checking-phase waits and mid-build
//! `redo-ifchange` waits enter the same graph (contract item 2); what
//! differs is only how the *caller* treats a detected cycle: soft
//! (speculation abandons verification and rebuilds) or hard (a running
//! do-file's dependency cycle is a real error).
//!
//! Liveness (contract item 6): every edge records an `owner` — this process,
//! identified by pid plus a nonce — and each owner holds a kernel file lock
//! (`.redom/locks/w.<owner>.lock`) for its whole lifetime. The kernel
//! releases the lock when the process dies, exactly the rule the per-target
//! build locks rely on, so a free (or missing) liveness lock proves the
//! owner is gone and its edges are garbage. Edges are GC'd on that evidence
//! at top-level session start and — to avoid false cycle errors — whenever a
//! cycle check first comes back positive.

use crate::db;
use crate::root::Root;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

/// What [`try_wait`] decided, atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitOutcome {
    /// The edge is recorded; waiting on the dep cannot deadlock.
    Inserted,
    /// Waiting would close a cycle (through live edges of live owners); no
    /// edge was inserted. Severity is the caller's call: soft while
    /// speculating over recorded deps, hard from a running do-file.
    Cycle,
}

/// This process's edge-owner identity: pid plus a startup nonce (pids are
/// reused; the nonce makes the liveness lock name unambiguous).
fn owner_id() -> &'static str {
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(|| {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("{}-{:x}", std::process::id(), nanos)
    })
}

fn live_lock_path(root: &Root, owner: &str) -> PathBuf {
    root.locks_dir().join(format!("w.{owner}.lock"))
}

/// Take (once per process) the kernel lock that proves this owner is alive.
/// The handle is held in a process-lifetime static; the OS releases the lock
/// on death. Called lazily before the first edge insert.
fn ensure_liveness(root: &Root) -> Result<()> {
    static HELD: OnceLock<Option<File>> = OnceLock::new();
    let held = HELD.get_or_init(|| acquire_liveness(root).ok());
    if held.is_some() {
        Ok(())
    } else {
        bail!(
            "cannot establish the wait-edge liveness lock in {}",
            root.locks_dir().display()
        )
    }
}

fn acquire_liveness(root: &Root) -> Result<File> {
    let dir = root.locks_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating lock dir {}", dir.display()))?;
    let path = live_lock_path(root, owner_id());
    // The GC only removes liveness files it can lock (i.e. whose owner is
    // dead), and holds the lock across the unlink — so the only race window
    // is between our create and our lock. Detect it (the path vanished: we
    // locked an unlinked inode) and retry.
    for _ in 0..10 {
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening liveness lock {}", path.display()))?;
        fs2::FileExt::lock_exclusive(&f)
            .with_context(|| format!("locking liveness lock {}", path.display()))?;
        if path.exists() {
            return Ok(f);
        }
    }
    bail!("could not establish liveness lock at {}", path.display())
}

/// Whether the owner of `owner` is still alive: its liveness lock file
/// exists and is held. A missing file or a winnable lock proves death (our
/// own lock is held on a separate file description, so probing ourselves
/// correctly reports "alive").
fn owner_alive(root: &Root, owner: &str) -> bool {
    let path = live_lock_path(root, owner);
    let f = match OpenOptions::new().read(true).write(true).open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return false,
        Err(_) => return true, // can't tell: claim alive, the safe answer
    };
    match fs2::FileExt::try_lock_exclusive(&f) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&f);
            false
        }
        Err(_) => true,
    }
}

/// Reachability over the live waits-for graph: can `from` reach `to` by
/// following `waiter -> dep` edges? Both arguments must be pre-lowercased
/// (edges are stored lowercased so the CTE compares BINARY-exactly).
fn reachable(conn: &Connection, from: &str, to: &str) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "WITH RECURSIVE reach(t) AS (
             VALUES(?1)
             UNION
             SELECT w.dep FROM waits w JOIN reach r ON w.waiter = r.t
         )
         SELECT EXISTS(SELECT 1 FROM reach WHERE t = ?2)",
    )?;
    Ok(stmt.query_row(params![from, to], |r| r.get::<_, bool>(0))?)
}

/// Atomically check-and-insert the wait edge `waiter -> dep` for this
/// process. Returns [`WaitOutcome::Cycle`] — without inserting — if the wait
/// would close a cycle through live edges. On a positive cycle check the
/// dead owners' edges are collected once and the check retried, so a
/// crashed traversal's leftovers cannot fabricate a cycle error.
pub fn try_wait(
    root: &Root,
    conn: &Connection,
    session: i64,
    waiter: &str,
    dep: &str,
) -> Result<WaitOutcome> {
    ensure_liveness(root)?;
    let w = waiter.to_ascii_lowercase();
    let d = dep.to_ascii_lowercase();
    for attempt in 0..2 {
        let inserted = db::write_txn(conn, |c| {
            if reachable(c, &d, &w)? {
                Ok(false)
            } else {
                c.execute(
                    "INSERT OR IGNORE INTO waits(session, waiter, dep, owner)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![session, w, d, owner_id()],
                )?;
                Ok(true)
            }
        })?;
        if inserted {
            return Ok(WaitOutcome::Inserted);
        }
        if attempt == 0 {
            // The cycle may ride on edges of a dead process: GC and retry.
            gc_dead_edges(root, conn)?;
        }
    }
    Ok(WaitOutcome::Cycle)
}

/// Drop every edge this process recorded for `waiter`. Called when the
/// traversal of `waiter` settles (verified, about to build, failed) — a
/// settled target no longer blocks, so its edges must leave the graph.
pub fn clear_waiter(conn: &Connection, waiter: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM waits WHERE waiter = ?1 AND owner = ?2",
        params![waiter.to_ascii_lowercase(), owner_id()],
    )?;
    Ok(())
}

/// Delete edges whose owner is provably dead (liveness lock free/missing).
fn gc_dead_edges(root: &Root, conn: &Connection) -> Result<()> {
    let owners: Vec<String> = {
        let mut stmt = conn.prepare("SELECT DISTINCT owner FROM waits")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    for o in owners {
        if o != owner_id() && !owner_alive(root, &o) {
            conn.execute("DELETE FROM waits WHERE owner = ?1", params![&o])?;
        }
    }
    Ok(())
}

/// Session-start sweep (top-level `redo`): GC dead owners' edges and remove
/// their liveness lock files. A file is removed only while holding its lock
/// (winnable ⇒ owner dead), held across the unlink so a concurrent starter
/// that just created the same path detects the race and recreates it.
pub fn gc_sweep(root: &Root, conn: &Connection) {
    let _ = gc_dead_edges(root, conn);
    let dir = root.locks_dir();
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let name = match name.to_str() {
            Some(n) => n,
            None => continue,
        };
        let owner = match name.strip_prefix("w.").and_then(|n| n.strip_suffix(".lock")) {
            Some(o) => o,
            None => continue,
        };
        if owner == owner_id() {
            continue;
        }
        let f = match OpenOptions::new().read(true).write(true).open(e.path()) {
            Ok(f) => f,
            Err(_) => continue,
        };
        if fs2::FileExt::try_lock_exclusive(&f).is_ok() {
            let _ = conn.execute("DELETE FROM waits WHERE owner = ?1", params![owner]);
            let _ = fs::remove_file(e.path());
            let _ = fs2::FileExt::unlock(&f);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(tag: &str) -> Root {
        let dir = std::env::temp_dir().join(format!(
            "redo-msh-waits-{}-{tag}",
            std::process::id()
        ));
        std::fs::create_dir_all(dir.join(".redom")).unwrap();
        Root { dir }
    }

    #[test]
    fn cycle_is_detected_and_edge_not_inserted() {
        let root = test_root("cycle");
        let conn = db::open(&root.db_path()).unwrap();

        assert_eq!(
            try_wait(&root, &conn, 1, "a", "b").unwrap(),
            WaitOutcome::Inserted
        );
        // b -> a closes the cycle: refused, atomically.
        assert_eq!(
            try_wait(&root, &conn, 1, "b", "a").unwrap(),
            WaitOutcome::Cycle
        );
        // Case-insensitive identity: A/B are the same nodes.
        assert_eq!(
            try_wait(&root, &conn, 1, "B", "A").unwrap(),
            WaitOutcome::Cycle
        );
        // Self-wait is a cycle.
        assert_eq!(
            try_wait(&root, &conn, 1, "c", "c").unwrap(),
            WaitOutcome::Cycle
        );

        // Transitive: a -> b, b -> c, then c -> a is refused.
        assert_eq!(
            try_wait(&root, &conn, 1, "b", "c").unwrap(),
            WaitOutcome::Inserted
        );
        assert_eq!(
            try_wait(&root, &conn, 1, "c", "a").unwrap(),
            WaitOutcome::Cycle
        );

        // Settling b unblocks: after b's edges leave, c -> a is still a
        // cycle through a -> b? No: reachability from a now stops at b.
        clear_waiter(&conn, "b").unwrap();
        assert_eq!(
            try_wait(&root, &conn, 1, "c", "a").unwrap(),
            WaitOutcome::Inserted
        );

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    #[test]
    fn dead_owner_edges_are_not_cycles() {
        let root = test_root("dead");
        let conn = db::open(&root.db_path()).unwrap();

        // Plant an edge from a fabricated dead owner (no liveness lock).
        conn.execute(
            "INSERT INTO waits(session, waiter, dep, owner) VALUES (1, 'a', 'b', 'dead-1')",
            [],
        )
        .unwrap();
        // b -> a would close a cycle over the dead edge; the GC-retry must
        // clear it and let the wait proceed.
        assert_eq!(
            try_wait(&root, &conn, 1, "b", "a").unwrap(),
            WaitOutcome::Inserted
        );
        let dead_left: i64 = conn
            .query_row(
                "SELECT count(*) FROM waits WHERE owner = 'dead-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dead_left, 0, "dead owner's edges must be GC'd");

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }
}

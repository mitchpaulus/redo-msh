//! The shared waits-for graph: cross-worker, cross-process cycle handling,
//! implementing the machine verified in `verification/SpeculationMP.tla`
//! (see `verification/README.md`).
//!
//! An edge `waiter -> dep` means "some traversal of `waiter` is (about to
//! be) blocked until `dep` settles". Rule R1: EVERY such wait is an edge in
//! this one graph, inserted atomically with a cycle check in one SQLite
//! write transaction. Rule R2 types the edges:
//!
//! * **Demand** (hard): a running do-file's `redo-ifchange` — ground truth.
//! * **Checker** (soft): the speculative checking phase waiting on a
//!   recorded dep.
//! * **Creation** (soft): inserted when a process speculatively spawns a
//!   task for `dep` on behalf of its context target (`REDO_TARGET`). This
//!   is the edge that makes the process's eventual drain wait visible —
//!   the wait the first implementation left out, which produced a
//!   reproducible deadlock on acyclic projects.
//!
//! Rule R3, the cycle rules:
//! * soft insert, any cycle → refuse softly (the checker rebuilds; the
//!   speculation is never started);
//! * hard insert, all-hard cycle → a real dependency cycle, an error;
//! * hard insert, cycle riding a soft edge → the SPECULATION yields:
//!   [`try_demand`] evicts one soft edge on the cycle (checker edges
//!   preferred — cheaper disruption) in the same transaction and reports
//!   `Evicted`; the caller retries. Evicted waiters notice their edge is
//!   gone through interruptible waits (`parallel.rs`) and take the soft
//!   path; an evicted creation edge aborts its speculative lineage, whose
//!   blocked primitives all poll the watch list (rule R4's quarantine is
//!   `parallel.rs`'s concern).
//!
//! Kernel target-lock waits deliberately carry no edge: edges are keyed by
//! target NAME, so a waiter's `w -> name` edge plus the foreign builder's
//! own `name -> deps` edges bridge the lock wait (proven safe by the
//! model).
//!
//! Liveness: every edge records an `owner` — this process, identified by
//! pid plus a nonce — and each owner holds a kernel file lock
//! (`.redom/locks/w.<owner>.lock`) for its whole lifetime. The kernel
//! releases the lock when the process dies, so a free (or missing)
//! liveness lock proves the owner is gone and its edges are garbage.
//! Edges are GC'd on that evidence at top-level session start and whenever
//! a cycle check first comes back positive.

use crate::db;
use crate::root::Root;
use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Edge types (rule R2). Stored as the integer in `waits.kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum EdgeKind {
    /// A running do-file's `redo-ifchange` wait: HARD, ground truth.
    Demand = 0,
    /// A checking-phase wait on a recorded dep: SOFT.
    Checker = 1,
    /// A speculative task spawn on behalf of the process's context target:
    /// SOFT; makes the eventual drain wait visible.
    Creation = 2,
}

/// What a SOFT check-and-insert decided, atomically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftOutcome {
    /// The edge is recorded; the wait (or spawn) may proceed.
    Inserted,
    /// The edge would close a cycle: refused, nothing inserted. The caller
    /// takes the rebuild path (checker) or skips the spawn (creation).
    Cycle,
}

/// One step of the HARD check-and-insert (rule R3). The caller loops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DemandOutcome {
    /// The demand edge is recorded (superseding any creation edge for the
    /// same dep — the "upgrade" that makes the task un-abortable).
    Inserted,
    /// A cycle made only of hard edges: a REAL dependency cycle.
    RealCycle,
    /// A cycle riding soft edges: one soft edge was evicted in the same
    /// transaction. Retry. If the evicted edge was a creation edge, the
    /// speculative instance's own outgoing edges were dropped with it.
    Evicted {
        /// Whether the eviction aborted a speculative lineage (creation
        /// edge): its residual edges may take a moment to dissolve as the
        /// lineage's processes notice and exit, so a `RealCycle` seen
        /// shortly after should be re-checked briefly before being trusted.
        aborted_lineage: bool,
    },
}

/// Identifies one edge, for liveness watches (`REDO_SPEC_WATCH`) and
/// interruptible waits. `waiter`/`dep` are stored lowercased.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeRef {
    pub owner: String,
    pub waiter: String,
    pub dep: String,
}

/// This process's edge-owner identity: pid plus a startup nonce (pids are
/// reused; the nonce makes the liveness lock name unambiguous).
pub fn owner_id() -> &'static str {
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
/// The handle is held in a process-lifetime static; the OS releases the
/// lock on death. Called lazily before the first edge insert. A transient
/// failure is retried on the next call rather than cached forever.
fn ensure_liveness(root: &Root) -> Result<()> {
    static HELD: Mutex<Option<File>> = Mutex::new(None);
    let mut held = HELD.lock().unwrap();
    if held.is_some() {
        return Ok(());
    }
    match acquire_liveness(root) {
        Ok(f) => {
            *held = Some(f);
            Ok(())
        }
        Err(e) => Err(e.context(format!(
            "cannot establish the wait-edge liveness lock in {}",
            root.locks_dir().display()
        ))),
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

/// Reachability `from ~> to` over the graph, following `waiter -> dep`
/// edges. `hard_only` restricts to demand edges (the all-hard cycle test).
/// Both arguments must be pre-lowercased (edges are stored lowercased so
/// the CTE compares BINARY-exactly).
fn reachable(conn: &Connection, from: &str, to: &str, hard_only: bool) -> Result<bool> {
    let sql = if hard_only {
        "WITH RECURSIVE reach(t) AS (
             VALUES(?1)
             UNION
             SELECT w.dep FROM waits w JOIN reach r ON w.waiter = r.t
              WHERE w.kind = 0
         )
         SELECT EXISTS(SELECT 1 FROM reach WHERE t = ?2)"
    } else {
        "WITH RECURSIVE reach(t) AS (
             VALUES(?1)
             UNION
             SELECT w.dep FROM waits w JOIN reach r ON w.waiter = r.t
         )
         SELECT EXISTS(SELECT 1 FROM reach WHERE t = ?2)"
    };
    let mut stmt = conn.prepare_cached(sql)?;
    Ok(stmt.query_row(params![from, to], |r| r.get::<_, bool>(0))?)
}

/// Atomic SOFT check-and-insert (checker wait or creation edge). Returns
/// [`SoftOutcome::Cycle`] — without inserting — if the edge would close a
/// cycle through live edges. On a positive cycle check the dead owners'
/// edges are collected once and the check retried, so a crashed
/// traversal's leftovers cannot force needless rebuilds.
pub fn try_soft(
    root: &Root,
    conn: &Connection,
    waiter: &str,
    dep: &str,
    kind: EdgeKind,
) -> Result<SoftOutcome> {
    debug_assert!(kind != EdgeKind::Demand);
    ensure_liveness(root)?;
    let w = waiter.to_ascii_lowercase();
    let d = dep.to_ascii_lowercase();
    for attempt in 0..2 {
        let inserted = db::write_txn(conn, |c| {
            if reachable(c, &d, &w, false)? {
                Ok(false)
            } else {
                // OR IGNORE: a demand edge already at this key outranks us;
                // a same-kind edge from an earlier spawn is simply shared.
                c.execute(
                    "INSERT OR IGNORE INTO waits(waiter, dep, owner, kind)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![w, d, owner_id(), kind as i64],
                )?;
                Ok(true)
            }
        })?;
        if inserted {
            return Ok(SoftOutcome::Inserted);
        }
        if attempt == 0 {
            gc_dead_edges(root, conn)?;
        }
    }
    Ok(SoftOutcome::Cycle)
}

/// Batched form of [`try_soft`]: many edges for ONE waiter name, checked
/// and inserted sequentially inside a single write transaction (each
/// check sees the batch's earlier inserts, so the semantics equal N
/// sequential `try_soft` calls — at one transaction's cost instead of N).
/// A sentinel waiter (`\x01...`, used by top-level processes for creation
/// edges) is unreachable by construction, so its checks are skipped.
pub fn try_soft_batch(
    root: &Root,
    conn: &Connection,
    waiter: &str,
    deps: &[String],
    kind: EdgeKind,
) -> Result<Vec<SoftOutcome>> {
    debug_assert!(kind != EdgeKind::Demand);
    if deps.is_empty() {
        return Ok(Vec::new());
    }
    ensure_liveness(root)?;
    let w = waiter.to_ascii_lowercase();
    let unreachable_waiter = w.starts_with('\u{1}');
    for attempt in 0..2 {
        let outcomes = db::write_txn(conn, |c| {
            let mut out = Vec::with_capacity(deps.len());
            for dep in deps {
                let d = dep.to_ascii_lowercase();
                if !unreachable_waiter && reachable(c, &d, &w, false)? {
                    out.push(SoftOutcome::Cycle);
                } else {
                    c.execute(
                        "INSERT OR IGNORE INTO waits(waiter, dep, owner, kind)
                         VALUES (?1, ?2, ?3, ?4)",
                        params![w, d, owner_id(), kind as i64],
                    )?;
                    out.push(SoftOutcome::Inserted);
                }
            }
            Ok(out)
        })?;
        if attempt == 1 || outcomes.iter().all(|o| *o == SoftOutcome::Inserted) {
            return Ok(outcomes);
        }
        // Some cycle may ride a dead process's leftovers: GC once, retry.
        gc_dead_edges(root, conn)?;
    }
    unreachable!("the retry loop always returns on its second pass")
}

/// Batched happy path of the HARD check-and-insert: every dep whose edge
/// closes no cycle is inserted in ONE transaction; the ones that do hit a
/// cycle are reported back for the caller's per-dep [`try_demand`]
/// eviction loop. Insertions use OR REPLACE (the creation-edge upgrade).
pub fn try_demand_batch(
    root: &Root,
    conn: &Connection,
    waiter: &str,
    deps: &[String],
) -> Result<Vec<bool>> {
    if deps.is_empty() {
        return Ok(Vec::new());
    }
    ensure_liveness(root)?;
    let w = waiter.to_ascii_lowercase();
    db::write_txn(conn, |c| {
        let mut inserted = Vec::with_capacity(deps.len());
        for dep in deps {
            let d = dep.to_ascii_lowercase();
            if reachable(c, &d, &w, false)? {
                inserted.push(false);
            } else {
                c.execute(
                    "INSERT OR REPLACE INTO waits(waiter, dep, owner, kind)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![w, d, owner_id(), EdgeKind::Demand as i64],
                )?;
                inserted.push(true);
            }
        }
        Ok(inserted)
    })
}

/// One atomic step of the HARD check-and-insert for `waiter -> dep` (rule
/// R3). The caller loops on `Evicted`. Dead-owner GC is the caller's
/// concern (`gc_dead_edges` on the first `RealCycle`), because only the
/// caller knows whether a `RealCycle` should also wait out a dissolving
/// aborted lineage.
pub fn try_demand(
    root: &Root,
    conn: &Connection,
    waiter: &str,
    dep: &str,
) -> Result<DemandOutcome> {
    ensure_liveness(root)?;
    let w = waiter.to_ascii_lowercase();
    let d = dep.to_ascii_lowercase();
    db::write_txn(conn, |c| {
        if !reachable(c, &d, &w, false)? {
            // OR REPLACE: supersedes a creation edge at the same key — the
            // upgrade that makes the speculative task un-abortable. Its
            // watch sees the row still present (kind changed) and stays
            // alive.
            c.execute(
                "INSERT OR REPLACE INTO waits(waiter, dep, owner, kind)
                 VALUES (?1, ?2, ?3, ?4)",
                params![w, d, owner_id(), EdgeKind::Demand as i64],
            )?;
            return Ok(DemandOutcome::Inserted);
        }
        if reachable(c, &d, &w, true)? {
            return Ok(DemandOutcome::RealCycle);
        }
        // A cycle that rides at least one soft edge: evict one soft edge
        // lying on a d ~> w path (checker edges first — evicting a checker
        // only costs that checker a rebuild; evicting a creation edge
        // aborts a speculative build). Selection and deletion are inside
        // this same transaction, keeping check-and-evict atomic.
        let cand = c
            .query_row(
                "SELECT rowid, waiter, dep, kind FROM waits
                 WHERE kind IN (1, 2)
                   AND waiter IN (
                     WITH RECURSIVE fwd(t) AS (
                         VALUES(?1)
                         UNION
                         SELECT w2.dep FROM waits w2 JOIN fwd f ON w2.waiter = f.t
                     ) SELECT t FROM fwd)
                   AND dep IN (
                     WITH RECURSIVE rev(t) AS (
                         VALUES(?2)
                         UNION
                         SELECT w3.waiter FROM waits w3 JOIN rev r ON w3.dep = r.t
                     ) SELECT t FROM rev)
                 ORDER BY kind ASC LIMIT 1",
                params![d, w],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;
        match cand {
            None => Ok(DemandOutcome::RealCycle), // defensive: no soft edge on any path
            Some((rowid, _ew, edep, kind)) => {
                c.execute("DELETE FROM waits WHERE rowid = ?1", params![rowid])?;
                let aborted = kind == EdgeKind::Creation as i64;
                if aborted {
                    // Aborting the speculative instance `edep`: drop its own
                    // outgoing waits too, cutting every residual path through
                    // it at once. Its processes notice via their watch/edge
                    // polls and unwind; anything they still hold dissolves as
                    // they exit.
                    c.execute("DELETE FROM waits WHERE waiter = ?1", params![edep])?;
                }
                Ok(DemandOutcome::Evicted {
                    aborted_lineage: aborted,
                })
            }
        }
    })
}

/// Abort a speculative lineage from BELOW (SpeculationMP rule R6): delete
/// the creation edge guarding it, exactly as an R3 eviction would, so every
/// blocked primitive watching that edge unwinds. Atomic against the upgrade
/// that supersedes a creation edge with a demand edge (`try_demand`'s
/// `INSERT OR REPLACE`): the delete matches on kind, so it returns `false`
/// — nothing aborted — when the lineage was already demanded, and the
/// caller must then treat its work as demanded after all.
pub fn abort_creation(root: &Root, conn: &Connection, e: &EdgeRef) -> Result<bool> {
    ensure_liveness(root)?;
    db::write_txn(conn, |c| {
        let n = c.execute(
            "DELETE FROM waits WHERE owner = ?1 AND waiter = ?2 AND dep = ?3 AND kind = ?4",
            params![e.owner, e.waiter, e.dep, EdgeKind::Creation as i64],
        )?;
        if n == 0 {
            return Ok(false);
        }
        // Same as the eviction: the aborted instance's own waits go too.
        c.execute("DELETE FROM waits WHERE waiter = ?1", params![e.dep])?;
        Ok(true)
    })
}

/// Whether an edge is still present (any kind — an upgraded creation edge
/// is alive as a demand edge). The poll behind interruptible waits and
/// speculation watches.
pub fn edge_alive(conn: &Connection, e: &EdgeRef) -> Result<bool> {
    let mut stmt = conn.prepare_cached(
        "SELECT 1 FROM waits WHERE owner = ?1 AND waiter = ?2 AND dep = ?3",
    )?;
    Ok(stmt
        .query_row(params![e.owner, e.waiter, e.dep], |_| Ok(()))
        .optional()?
        .is_some())
}

/// Whether any edge in a speculation watch list has disappeared — the
/// abort signal for a speculative lineage (an empty list is never dead).
pub fn watch_dead(conn: &Connection, watch: &[EdgeRef]) -> Result<bool> {
    for e in watch {
        if !edge_alive(conn, e)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Drop every edge this process recorded for `waiter`. Called when the
/// traversal of `waiter` settles (verified, about to build, failed) and
/// when an ifchange group finishes — a settled target no longer blocks, so
/// its edges must leave the graph.
pub fn clear_waiter(conn: &Connection, waiter: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM waits WHERE waiter = ?1 AND owner = ?2",
        params![waiter.to_ascii_lowercase(), owner_id()],
    )?;
    Ok(())
}

/// Delete edges whose owner is provably dead (liveness lock free/missing).
pub fn gc_dead_edges(root: &Root, conn: &Connection) -> Result<()> {
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

// ---- speculation watch serialization (REDO_SPEC_WATCH) ---------------------

/// Environment variable carrying the creation edges guarding this process's
/// speculative lineage; any of them disappearing means "abort". Records are
/// separated by \x1e, fields by \x1f.
pub const ENV_SPEC_WATCH: &str = "REDO_SPEC_WATCH";

pub fn watch_to_env(watch: &[EdgeRef]) -> String {
    watch
        .iter()
        .map(|e| format!("{}\x1f{}\x1f{}", e.owner, e.waiter, e.dep))
        .collect::<Vec<_>>()
        .join("\x1e")
}

pub fn watch_from_env(s: &str) -> Vec<EdgeRef> {
    s.split('\x1e')
        .filter(|r| !r.is_empty())
        .filter_map(|r| {
            let mut it = r.split('\x1f');
            Some(EdgeRef {
                owner: it.next()?.to_string(),
                waiter: it.next()?.to_string(),
                dep: it.next()?.to_string(),
            })
        })
        .collect()
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
    fn soft_cycle_is_refused_and_edge_not_inserted() {
        let root = test_root("softcycle");
        let conn = db::open(&root.db_path()).unwrap();

        assert_eq!(
            try_soft(&root, &conn, "a", "b", EdgeKind::Checker).unwrap(),
            SoftOutcome::Inserted
        );
        // b -> a closes the cycle: refused, atomically.
        assert_eq!(
            try_soft(&root, &conn, "b", "a", EdgeKind::Checker).unwrap(),
            SoftOutcome::Cycle
        );
        // Case-insensitive identity: A/B are the same nodes.
        assert_eq!(
            try_soft(&root, &conn, "B", "A", EdgeKind::Creation).unwrap(),
            SoftOutcome::Cycle
        );
        // Self-wait is a cycle.
        assert_eq!(
            try_soft(&root, &conn, "c", "c", EdgeKind::Creation).unwrap(),
            SoftOutcome::Cycle
        );

        // Transitive: a -> b, b -> c, then c -> a is refused.
        assert_eq!(
            try_soft(&root, &conn, "b", "c", EdgeKind::Checker).unwrap(),
            SoftOutcome::Inserted
        );
        assert_eq!(
            try_soft(&root, &conn, "c", "a", EdgeKind::Checker).unwrap(),
            SoftOutcome::Cycle
        );

        // Settling b unblocks: after b's edges leave, c -> a inserts.
        clear_waiter(&conn, "b").unwrap();
        assert_eq!(
            try_soft(&root, &conn, "c", "a", EdgeKind::Checker).unwrap(),
            SoftOutcome::Inserted
        );

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    #[test]
    fn all_hard_cycle_is_a_real_error() {
        let root = test_root("hardcycle");
        let conn = db::open(&root.db_path()).unwrap();

        assert_eq!(
            try_demand(&root, &conn, "a", "b").unwrap(),
            DemandOutcome::Inserted
        );
        assert_eq!(
            try_demand(&root, &conn, "b", "a").unwrap(),
            DemandOutcome::RealCycle
        );

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    #[test]
    fn demand_evicts_checker_edge_and_then_inserts() {
        let root = test_root("evictchk");
        let conn = db::open(&root.db_path()).unwrap();

        // A speculative checker wait a -> b...
        assert_eq!(
            try_soft(&root, &conn, "a", "b", EdgeKind::Checker).unwrap(),
            SoftOutcome::Inserted
        );
        // ...must yield to the real demand b -> a: the checker edge is
        // evicted (not a real cycle), and the retry inserts.
        assert_eq!(
            try_demand(&root, &conn, "b", "a").unwrap(),
            DemandOutcome::Evicted {
                aborted_lineage: false
            }
        );
        let e = EdgeRef {
            owner: owner_id().to_string(),
            waiter: "a".into(),
            dep: "b".into(),
        };
        assert!(!edge_alive(&conn, &e).unwrap(), "checker edge must be evicted");
        assert_eq!(
            try_demand(&root, &conn, "b", "a").unwrap(),
            DemandOutcome::Inserted
        );

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    #[test]
    fn demand_evicts_creation_edge_and_cuts_the_instance_loose() {
        let root = test_root("evictcre");
        let conn = db::open(&root.db_path()).unwrap();

        // ctx speculatively spawned s (creation ctx -> s); s's own build
        // then HARD-demanded x (a real running do-file inside the
        // speculative lineage).
        assert_eq!(
            try_soft(&root, &conn, "ctx", "s", EdgeKind::Creation).unwrap(),
            SoftOutcome::Inserted
        );
        assert_eq!(
            try_demand(&root, &conn, "s", "x").unwrap(),
            DemandOutcome::Inserted
        );
        // A real demand x -> ctx cycles through ctx -> s -> x; the only
        // soft edge on the path is the creation edge, so the speculative
        // instance s is aborted and its own outgoing edges cut with it.
        assert_eq!(
            try_demand(&root, &conn, "x", "ctx").unwrap(),
            DemandOutcome::Evicted {
                aborted_lineage: true
            }
        );
        let sx = EdgeRef {
            owner: owner_id().to_string(),
            waiter: "s".into(),
            dep: "x".into(),
        };
        assert!(
            !edge_alive(&conn, &sx).unwrap(),
            "the aborted instance's outgoing edges must be cut"
        );
        assert_eq!(
            try_demand(&root, &conn, "x", "ctx").unwrap(),
            DemandOutcome::Inserted
        );

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    #[test]
    fn demand_upgrade_supersedes_creation_edge_and_stays_alive() {
        let root = test_root("upgrade");
        let conn = db::open(&root.db_path()).unwrap();

        assert_eq!(
            try_soft(&root, &conn, "ctx", "d", EdgeKind::Creation).unwrap(),
            SoftOutcome::Inserted
        );
        // The same process later demands d from the same waiter name: the
        // creation edge is superseded in place; the watch (any-kind
        // presence) stays alive, making the task un-abortable.
        assert_eq!(
            try_demand(&root, &conn, "ctx", "d").unwrap(),
            DemandOutcome::Inserted
        );
        let e = EdgeRef {
            owner: owner_id().to_string(),
            waiter: "ctx".into(),
            dep: "d".into(),
        };
        assert!(edge_alive(&conn, &e).unwrap(), "upgraded edge must stay alive");
        let kind: i64 = conn
            .query_row(
                "SELECT kind FROM waits WHERE owner=?1 AND waiter='ctx' AND dep='d'",
                params![owner_id()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, EdgeKind::Demand as i64);

        drop(conn);
        let _ = std::fs::remove_dir_all(&root.dir);
    }

    #[test]
    fn dead_owner_edges_are_not_cycles() {
        let root = test_root("dead");
        let conn = db::open(&root.db_path()).unwrap();

        // Plant an edge from a fabricated dead owner (no liveness lock).
        conn.execute(
            "INSERT INTO waits(waiter, dep, owner, kind) VALUES ('a', 'b', 'dead-1', 1)",
            [],
        )
        .unwrap();
        // b -> a would close a cycle over the dead edge; the GC-retry must
        // clear it and let the wait proceed.
        assert_eq!(
            try_soft(&root, &conn, "b", "a", EdgeKind::Checker).unwrap(),
            SoftOutcome::Inserted
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

    #[test]
    fn watch_env_roundtrip() {
        let watch = vec![
            EdgeRef {
                owner: "123-abc".into(),
                waiter: "x".into(),
                dep: "s".into(),
            },
            EdgeRef {
                owner: "9-f".into(),
                waiter: "y".into(),
                dep: "t".into(),
            },
        ];
        assert_eq!(watch_from_env(&watch_to_env(&watch)), watch);
        assert!(watch_from_env("").is_empty());
    }
}

//! SQLite state store at `.redo/redo-msh.db` (WAL mode).
//!
//! Two tables (see plan):
//!   * `files` — per-file stamp cache for *all* files (sources and targets).
//!     Equality is always by `csum` (blake3); `mtime`/`size` only gate whether
//!     we must recompute the hash.
//!   * `deps`  — dependency edges. The `csum` recorded here is the equality
//!     basis for change detection; `mtime`/`size` are fast-path accelerators.
//!
//! A `meta` table holds the schema version and the monotonic `runid` build
//! counter (clock-independent).

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;

pub const SCHEMA_VERSION: i64 = 2;

/// Dependency edge kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub enum DepKind {
    IfChange = 0,
    IfCreate = 1,
    Always = 2,
    DoFile = 3,
}

impl DepKind {
    pub fn from_i64(v: i64) -> Option<DepKind> {
        match v {
            0 => Some(DepKind::IfChange),
            1 => Some(DepKind::IfCreate),
            2 => Some(DepKind::Always),
            3 => Some(DepKind::DoFile),
            _ => None,
        }
    }
}

/// Open (creating if needed) the database and apply pragmas + schema.
pub fn open(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)
        .with_context(|| format!("opening database {}", db_path.display()))?;
    apply_pragmas(&conn)?;
    init_schema(&conn)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> Result<()> {
    // WAL: many readers + one writer; required for the parallel peer model.
    // busy_timeout: writers wait rather than erroring with SQLITE_BUSY.
    // synchronous=NORMAL: safe across app crashes in WAL; a lost last commit
    // only causes a (safe) rebuild, never a stale result.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.busy_timeout(std::time::Duration::from_secs(60))?;
    Ok(())
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );

        -- Path columns use COLLATE NOCASE so identity is case-insensitive,
        -- matching NTFS semantics and keeping one row per file when a project
        -- is shared between Windows and WSL. Original case is preserved in the
        -- stored text (used for filesystem I/O); only comparison is folded.
        CREATE TABLE IF NOT EXISTS files (
            path     TEXT PRIMARY KEY COLLATE NOCASE, -- root-relative, '/' separators
            dofile   TEXT,               -- .do file used (root-relative); NULL = source file
            built_at INTEGER,            -- unix nanos of last successful build (targets only)
            mtime    INTEGER,            -- last-observed mtime (unix nanos)
            size     INTEGER,            -- last-observed size in bytes
            csum     TEXT,               -- last-observed blake3 content hash (hex)
            runid    INTEGER             -- build counter when last verified/built
        );

        CREATE TABLE IF NOT EXISTS deps (
            target TEXT NOT NULL COLLATE NOCASE, -- dependent target (root-relative)
            kind   INTEGER NOT NULL,     -- DepKind: 0 ifchange, 1 ifcreate, 2 always, 3 dofile
            dep    TEXT COLLATE NOCASE,  -- dependency path (ifchange/ifcreate/dofile); NULL for always
            csum   TEXT,                 -- dep content hash expected at build time (equality basis)
            mtime  INTEGER,              -- recorded mtime (fast-path accelerator)
            size   INTEGER,              -- recorded size (fast-path accelerator)
            PRIMARY KEY (target, kind, dep)
        );

        CREATE INDEX IF NOT EXISTS deps_by_target ON deps(target);
        "#,
    )
    .context("initializing schema")?;

    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('schema_version', ?1)",
        [SCHEMA_VERSION],
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO meta(key, value) VALUES ('runid', 0)",
        [],
    )?;
    Ok(())
}

/// Atomically increment and return the global build counter.
pub fn next_runid(conn: &Connection) -> Result<i64> {
    conn.execute("UPDATE meta SET value = value + 1 WHERE key = 'runid'", [])?;
    let runid: i64 =
        conn.query_row("SELECT value FROM meta WHERE key = 'runid'", [], |r| r.get(0))?;
    Ok(runid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_case_insensitive() {
        let dir = std::env::temp_dir().join(format!("redo-msh-dbtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let conn = open(&dir.join("t.db")).unwrap();

        conn.execute("INSERT INTO files(path) VALUES ('App.txt')", [])
            .unwrap();
        // A different-cased lookup matches (NOCASE collation).
        let found: i64 = conn
            .query_row(
                "SELECT count(*) FROM files WHERE path = ?1",
                ["app.txt"],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(found, 1);

        // A different-cased insert collapses onto the same primary key.
        conn.execute("INSERT OR REPLACE INTO files(path) VALUES ('APP.TXT')", [])
            .unwrap();
        let total: i64 = conn
            .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(total, 1, "different casings must collapse to one row");

        // Original case is preserved in storage (for filesystem I/O).
        let stored: String = conn
            .query_row("SELECT path FROM files LIMIT 1", [], |r| r.get(0))
            .unwrap();
        assert!(stored.chars().any(|c| c.is_ascii_uppercase()));

        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

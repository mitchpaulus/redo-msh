//! Per-target advisory file locks for build exclusion.
//!
//! Exactly one process builds a given target; others that need it block on the
//! same lock and, on acquiring it, find the target already up to date. The lock
//! identity is the *target* (not the invocation), so two independent top-level
//! `redo` runs serialize naturally on shared targets.
//!
//! We use kernel advisory locks (`flock` on Unix, `LockFileEx` on Windows via
//! fs2). Their decisive robustness property: the kernel releases the lock when
//! the holding process dies, so a crashed build never leaves a stale lock.
//! The log follower (`logs.rs`) leans on the same property in reverse: a free
//! lock plus a log without a `done` terminator proves the builder died.

use crate::root::Root;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};
use std::path::PathBuf;

/// Held lock; releases on drop (the OS also releases it if the process dies).
pub struct TargetLock {
    _file: File,
}

/// A filesystem-safe, collision-resistant key derived from a root-relative
/// target path. Folds case (ASCII, matching the DB's NOCASE collation) so two
/// casings of the same target share one identity. This one key names both the
/// target's lock file and its log file — the follower probes the former for
/// the latter, so they must never be derived differently.
pub fn target_key(target_rel: &str) -> String {
    assert!(!target_rel.is_empty());
    let hash = blake3::hash(target_rel.to_ascii_lowercase().as_bytes()).to_hex();
    let key = hash.as_str()[..32].to_string();
    assert_eq!(key.len(), 32);
    key
}

pub fn lock_path(root: &Root, target_rel: &str) -> PathBuf {
    root.locks_dir().join(format!("{}.lock", target_key(target_rel)))
}

fn lock_path_for_key(root: &Root, key: &str) -> PathBuf {
    root.locks_dir().join(format!("{key}.lock"))
}

fn open_lock_file(root: &Root, target_rel: &str) -> Result<File> {
    let dir = root.locks_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating lock dir {}", dir.display()))?;
    let path = lock_path(root, target_rel);
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))
}

/// Acquire the exclusive build lock for `target_rel`, blocking until available.
pub fn lock_target(root: &Root, target_rel: &str) -> Result<TargetLock> {
    let file = open_lock_file(root, target_rel)?;
    file.lock_exclusive()
        .with_context(|| format!("locking target {target_rel}"))?;
    Ok(TargetLock { _file: file })
}

/// Try to acquire the build lock without blocking. `None` means another
/// process holds it — the caller can announce the wait, then block.
pub fn try_lock_target(root: &Root, target_rel: &str) -> Result<Option<TargetLock>> {
    let file = open_lock_file(root, target_rel)?;
    match file.try_lock_exclusive() {
        Ok(()) => Ok(Some(TargetLock { _file: file })),
        Err(_) => Ok(None),
    }
}

/// Whether no builder currently holds `target_rel`'s build lock. Read-only in
/// effect: the probe lock is released immediately. Used by the log follower to
/// classify a silent log (EOF, no `done`) as a crashed builder.
pub fn probe_unlocked(root: &Root, target_rel: &str) -> bool {
    probe_key_unlocked(root, &target_key(target_rel))
}

/// Lock probe by raw key (for the log GC, which only has the key from the log
/// file's name). A missing lock file means no builder ever ran: unlocked.
pub fn probe_key_unlocked(root: &Root, key: &str) -> bool {
    let file = match OpenOptions::new().read(true).write(true).open(lock_path_for_key(root, key))
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return true,
        Err(_) => return false, // Can't tell: claim locked, the safe answer.
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let _ = FileExt::unlock(&file);
            true
        }
        Err(_) => false,
    }
}

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

use crate::root::Root;
use anyhow::{Context, Result};
use fs2::FileExt;
use std::fs::{self, File, OpenOptions};

/// Held lock; releases on drop (the OS also releases it if the process dies).
pub struct TargetLock {
    _file: File,
}

/// Acquire the exclusive build lock for `target_rel`, blocking until available.
pub fn lock_target(root: &Root, target_rel: &str) -> Result<TargetLock> {
    let dir = root.locks_dir();
    fs::create_dir_all(&dir)
        .with_context(|| format!("creating lock dir {}", dir.display()))?;
    // A filesystem-safe, collision-resistant name derived from the target path.
    let hash = blake3::hash(target_rel.as_bytes()).to_hex();
    let path = dir.join(format!("{}.lock", &hash.as_str()[..32]));
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening lock file {}", path.display()))?;
    file.lock_exclusive()
        .with_context(|| format!("locking target {target_rel}"))?;
    Ok(TargetLock { _file: file })
}

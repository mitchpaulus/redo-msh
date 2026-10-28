//! File stamps for change detection.
//!
//! A stamp is `(mtime, size, csum, hashed_at)`. Equality is **always** decided
//! by `csum` (blake3 content hash); `mtime`/`size` are only fast-path
//! accelerators the out-of-date engine uses to decide whether a re-hash is
//! needed, and `hashed_at` says whether that fast path may be trusted at all
//! (see `build::stamp_is_racy`).

use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Sentinel csum for a path that is a directory (we do not hash directories).
pub const DIR_CSUM: &str = "<dir>";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp {
    /// Modification time in unix nanoseconds (best available resolution).
    pub mtime: i64,
    /// Size in bytes.
    pub size: i64,
    /// blake3 content hash, hex. The equality basis.
    pub csum: String,
    /// When the hash was taken (unix nanoseconds), sampled before the file
    /// was read. A later write in the same timestamp tick as `mtime` would
    /// leave mtime and size unchanged; comparing `hashed_at` with `mtime`
    /// tells whether such a write is possible.
    pub hashed_at: i64,
}

/// The current time as unix nanoseconds.
pub fn now_nanos() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Modification time of `meta` as unix nanoseconds.
pub fn mtime_nanos(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// blake3 hash of a file's contents, as hex.
pub fn hash_file(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    let mut f = fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    io::copy(&mut f, &mut hasher).with_context(|| format!("reading {}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// Full stamp of `path`. Returns `Ok(None)` if the path does not exist.
/// Follows symlinks (the resolved file's content is what matters).
pub fn stamp_file(path: &Path) -> Result<Option<Stamp>> {
    let hashed_at = now_nanos();
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("stat {}", path.display()))),
    };
    let mtime = mtime_nanos(&meta);
    if meta.is_dir() {
        return Ok(Some(Stamp {
            mtime,
            size: 0,
            csum: DIR_CSUM.to_string(),
            hashed_at,
        }));
    }
    Ok(Some(Stamp {
        mtime,
        size: meta.len() as i64,
        csum: hash_file(path)?,
        hashed_at,
    }))
}

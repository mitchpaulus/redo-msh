//! File stamps for change detection.
//!
//! A stamp is `(mtime, size, csum)`. Equality is **always** decided by `csum`
//! (blake3 content hash); `mtime`/`size` are only fast-path accelerators used
//! by the out-of-date engine (M3) to decide whether a re-hash is needed. In M2
//! we always hash; the guarded fast path is layered on in M3.

use anyhow::{Context, Result};
use std::fs;
use std::io;
use std::path::Path;
use std::time::UNIX_EPOCH;

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
        }));
    }
    Ok(Some(Stamp {
        mtime,
        size: meta.len() as i64,
        csum: hash_file(path)?,
    }))
}

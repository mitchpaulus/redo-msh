//! Local-filesystem guard for the `.redom/` state directory.
//!
//! Design decision (see DESIGN/plan): `.redom/` holds the SQLite database (WAL
//! mode) and the per-target advisory lock files. Neither SQLite WAL nor
//! `flock`/`LockFileEx` behaves correctly across machines, so `.redom/` MUST
//! live on a local disk. The working tree may live anywhere.
//!
//! We classify the filesystem under a path into one of four buckets:
//!   * `Local`      — proceed.
//!   * `Network`    — a true cross-machine FS (NFS/SMB/CIFS); hard error.
//!   * `MmapUnsafe` — a same-machine passthrough (WSL drvfs/9p, FUSE): file
//!     I/O and single-instance locking work, but shared writable mmap does
//!     not, so SQLite WAL mode fails in `xShmMap`. Proceed; the db layer
//!     selects a rollback-journal mode instead of WAL (see `db.rs`).
//!   * `Unknown`    — couldn't determine; proceed.
//!
//! `REDO_ALLOW_NETWORK_FS=1` downgrades the hard error to a warning.

use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Local,
    Network,
    /// Same-machine passthrough FS without shared-writable-mmap support
    /// (WSL drvfs/9p, FUSE). SQLite WAL cannot run here; rollback journal can.
    MmapUnsafe,
    Unknown,
}

/// Classify the filesystem backing `path` (which must exist).
#[cfg(target_os = "linux")]
pub fn classify(path: &Path) -> FsKind {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return FsKind::Unknown,
    };
    // SAFETY: zeroed statfs is valid to pass to statfs(2); we only read it on success.
    let mut buf: libc::statfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statfs(cpath.as_ptr(), &mut buf) };
    if rc != 0 {
        return FsKind::Unknown;
    }
    // f_type magic numbers (see statfs(2) and linux/magic.h).
    const NFS_SUPER_MAGIC: i64 = 0x6969;
    const SMB_SUPER_MAGIC: i64 = 0x517B;
    const CIFS_MAGIC: i64 = 0xFF53_4D42;
    const SMB2_MAGIC: i64 = 0xFE53_4D42;
    const AFS_SUPER_MAGIC: i64 = 0x5346_414F;
    const NCP_SUPER_MAGIC: i64 = 0x564C;
    const V9FS_MAGIC: i64 = 0x0102_1997; // 9p; WSL mounts /mnt/c (drvfs) as 9p
    const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;

    let t = buf.f_type as i64;
    match t {
        NFS_SUPER_MAGIC | SMB_SUPER_MAGIC | CIFS_MAGIC | SMB2_MAGIC | AFS_SUPER_MAGIC
        | NCP_SUPER_MAGIC => FsKind::Network,
        V9FS_MAGIC | FUSE_SUPER_MAGIC => FsKind::MmapUnsafe,
        _ => FsKind::Local,
    }
}

#[cfg(not(target_os = "linux"))]
pub fn classify(_path: &Path) -> FsKind {
    // TODO(windows): GetDriveType / PathIsNetworkPathW; macOS: statfs f_fstypename.
    FsKind::Unknown
}

/// Enforce the local-disk requirement for `.redom/`. Returns an error for a
/// detected network filesystem unless `REDO_ALLOW_NETWORK_FS=1` is set.
pub fn enforce_local(redo_dir: &Path) -> anyhow::Result<()> {
    let override_set = std::env::var_os("REDO_ALLOW_NETWORK_FS")
        .map(|v| v == *"1")
        .unwrap_or(false);
    match classify(redo_dir) {
        FsKind::Local => Ok(()),
        FsKind::Unknown => Ok(()),
        // Single-instance locking works on a passthrough FS; only WAL's shared
        // mmap does not, and db::open() selects a non-WAL journal mode there.
        FsKind::MmapUnsafe => Ok(()),
        FsKind::Network => {
            if override_set {
                eprintln!(
                    "redo-msh: warning: {} appears to be on a network filesystem; \
                     proceeding because REDO_ALLOW_NETWORK_FS=1 (locking/WAL may be unreliable)",
                    redo_dir.display()
                );
                Ok(())
            } else {
                anyhow::bail!(
                    "redo-msh: {} is on a network filesystem. The .redom/ state directory \
                     (SQLite WAL + advisory locks) must be on a local disk. Move the project, \
                     or set REDO_ALLOW_NETWORK_FS=1 to override at your own risk.",
                    redo_dir.display()
                )
            }
        }
    }
}

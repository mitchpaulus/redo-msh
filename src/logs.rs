//! Serialized per-target log output for parallel builds.
//!
//! Each do-file's stderr is captured to a per-target file; when the build
//! finishes we emit a single contiguous block (a `redo <target>` header plus
//! that captured stderr) to a shared sink, so concurrent builds never
//! interleave. Blocks stream live, in target-completion order, indented by
//! depth to show nesting.
//!
//! The sink is the real process output, reachable even from a child whose own
//! stderr was redirected into a capture file:
//!   * Unix: the top-level dups its real stderr to an inherited fd and passes
//!     the number in `REDO_LOG_FD`; every redo-msh process writes blocks there,
//!     serialized across processes by an exclusive lock on `.redo/output.lock`.
//!     This respects redirection (`redo 2>log` sends blocks to the file).
//!   * Other platforms: blocks go to the process's own stderr and bubble up
//!     through the capture chain to the top level (correct and non-interleaved,
//!     though aggregated rather than streamed). A named-pipe sink is a TODO.
//!
//! A process-global mutex serializes block writes across worker threads on all
//! platforms.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

pub const ENV_LOG_FD: &str = "REDO_LOG_FD";

static OUT_LOCK: Mutex<()> = Mutex::new(());

/// Set up the inherited real-output fd for the top-level process.
#[cfg(unix)]
pub fn init_top() {
    if std::env::var_os(ENV_LOG_FD).is_none() {
        // dup(2) yields a non-CLOEXEC fd, so it is inherited across exec.
        let fd = unsafe { libc::dup(2) };
        if fd >= 0 {
            std::env::set_var(ENV_LOG_FD, fd.to_string());
        }
    }
}

#[cfg(not(unix))]
pub fn init_top() {}

/// Emit one contiguous log block: `header`, a newline, then the bytes of
/// `body_path` (the captured do-file stderr). Serialized across threads and,
/// on Unix, across processes.
pub fn emit_block(redo_dir: &Path, header: &str, body_path: &Path) {
    let body = std::fs::read(body_path).unwrap_or_default();
    let _threads = OUT_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    #[cfg(unix)]
    {
        if let Some(fd) = log_fd() {
            let _proc = lock_output(redo_dir); // cross-process serialization
            write_fd(fd, header.as_bytes());
            write_fd(fd, b"\n");
            write_fd(fd, &body);
            return;
        }
    }
    let _ = redo_dir; // unused on the fallback path
    let mut err = std::io::stderr().lock();
    let _ = writeln!(err, "{header}");
    let _ = err.write_all(&body);
    let _ = err.flush();
}

#[cfg(unix)]
fn log_fd() -> Option<i32> {
    std::env::var(ENV_LOG_FD).ok()?.parse().ok()
}

#[cfg(unix)]
fn write_fd(fd: i32, buf: &[u8]) {
    let mut off = 0;
    while off < buf.len() {
        // SAFETY: writing a sub-slice of a valid buffer to a valid fd.
        let n = unsafe {
            libc::write(
                fd,
                buf[off..].as_ptr() as *const libc::c_void,
                buf.len() - off,
            )
        };
        if n <= 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        off += n as usize;
    }
}

#[cfg(unix)]
fn lock_output(redo_dir: &Path) -> Option<std::fs::File> {
    use fs2::FileExt;
    let path = redo_dir.join("output.lock");
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(path)
        .ok()?;
    f.lock_exclusive().ok()?;
    Some(f) // released (unlocked) on drop
}

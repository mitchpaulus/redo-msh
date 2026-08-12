//! Jobserver: a global limit on the number of do-files running concurrently
//! across the whole process tree.
//!
//! Model (GNU make-style, deadlock-free): a *token* is permission to run one
//! do-file. Every process is granted exactly one implicit "own" token by
//! whoever launched it, which it may use for one concurrent job — this
//! guarantees forward progress, so a deep dependency chain can never deadlock
//! waiting for tokens. For *additional* parallelism a process try-acquires
//! extra tokens from a shared pool and returns them when the job finishes.
//!
//! On Unix the pool is a pipe pre-loaded with `j-1` byte tokens; the read end
//! is non-blocking (we only ever try-acquire, never block on it — completion is
//! awaited via thread join, not the pipe). The pipe fds are inherited by child
//! processes and their numbers passed in `REDO_JOBSERVER`, so the limit is
//! shared across the entire tree.
//!
//! On non-Unix the pool is a per-process counter (cross-process bounding via a
//! named semaphore is a TODO); correctness never depends on the jobserver, only
//! the degree of parallelism.

use std::sync::atomic::{AtomicUsize, Ordering};

pub const ENV: &str = "REDO_JOBSERVER";

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum TokenSrc {
    /// The process's implicit own token (always available for one job).
    Own,
    /// A token taken from the shared pool; must be returned on completion.
    Pool,
}

enum Inner {
    #[cfg(unix)]
    Pipe { rfd: i32, wfd: i32 },
    #[cfg(windows)]
    Sem { handle: isize },
    Local { extra: AtomicUsize },
}

pub struct Jobserver(Inner);

impl Jobserver {
    /// Initialize the top-level jobserver for `j` total tokens and publish it to
    /// the environment for child processes.
    pub fn init_top(j: usize) -> Jobserver {
        let extra = j.saturating_sub(1);
        #[cfg(unix)]
        {
            if let Some(js) = init_pipe(extra) {
                return js;
            }
        }
        #[cfg(windows)]
        {
            if let Some(js) = init_sem(j) {
                return js;
            }
        }
        std::env::set_var(ENV, format!("local:{j}"));
        Jobserver(Inner::Local {
            extra: AtomicUsize::new(extra),
        })
    }

    /// Derive the jobserver from the environment in a child process.
    pub fn from_env() -> Jobserver {
        match std::env::var(ENV) {
            Ok(s) => {
                #[cfg(unix)]
                if let Some(js) = from_pipe_spec(&s) {
                    return js;
                }
                #[cfg(windows)]
                if let Some(rest) = s.strip_prefix("winsem:") {
                    if let Some(js) = open_sem(rest) {
                        return js;
                    }
                }
                if let Some(rest) = s.strip_prefix("local:") {
                    let j: usize = rest.parse().unwrap_or(1);
                    return Jobserver(Inner::Local {
                        extra: AtomicUsize::new(j.saturating_sub(1)),
                    });
                }
                // Unknown spec: serial.
                Jobserver(Inner::Local {
                    extra: AtomicUsize::new(0),
                })
            }
            // No jobserver in the environment: serial (own token only).
            Err(_) => Jobserver(Inner::Local {
                extra: AtomicUsize::new(0),
            }),
        }
    }

    /// Try to take one extra token from the shared pool without blocking.
    pub fn try_acquire(&self) -> bool {
        match &self.0 {
            #[cfg(unix)]
            Inner::Pipe { rfd, .. } => pipe_try_read(*rfd),
            #[cfg(windows)]
            Inner::Sem { handle } => sem_try_acquire(*handle),
            Inner::Local { extra } => {
                let mut cur = extra.load(Ordering::Acquire);
                loop {
                    if cur == 0 {
                        return false;
                    }
                    match extra.compare_exchange_weak(
                        cur,
                        cur - 1,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return true,
                        Err(observed) => cur = observed,
                    }
                }
            }
        }
    }

    /// Human-readable description of the token-pool mechanism, for the
    /// `--debug-jobs` banner. Notes explicitly when the pool is per-process
    /// (the local fallback), since that changes the effective global limit.
    pub fn describe(&self) -> String {
        match &self.0 {
            #[cfg(unix)]
            Inner::Pipe { rfd, wfd } => {
                format!("token pipe (fds {rfd},{wfd}), shared across the process tree")
            }
            #[cfg(windows)]
            Inner::Sem { .. } => {
                "named semaphore, shared across the process tree".to_string()
            }
            Inner::Local { extra } => format!(
                "in-process counter ({} extra tokens, NOT shared with child redo processes)",
                extra.load(Ordering::Acquire)
            ),
        }
    }

    /// Return one token to the shared pool.
    pub fn release(&self) {
        match &self.0 {
            #[cfg(unix)]
            Inner::Pipe { wfd, .. } => pipe_write(*wfd),
            #[cfg(windows)]
            Inner::Sem { handle } => sem_release(*handle),
            Inner::Local { extra } => {
                extra.fetch_add(1, Ordering::AcqRel);
            }
        }
    }
}

// Jobserver is shared across build threads via Arc; raw fds + atomics are safe.
unsafe impl Send for Jobserver {}
unsafe impl Sync for Jobserver {}

#[cfg(unix)]
fn init_pipe(extra: usize) -> Option<Jobserver> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe(2) fills two fds; we check the return code.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return None;
    }
    let (rfd, wfd) = (fds[0], fds[1]);
    set_nonblocking(rfd);
    for _ in 0..extra {
        pipe_write(wfd);
    }
    std::env::set_var(ENV, format!("{rfd},{wfd}"));
    Some(Jobserver(Inner::Pipe { rfd, wfd }))
}

#[cfg(unix)]
fn from_pipe_spec(s: &str) -> Option<Jobserver> {
    let (r, w) = s.split_once(',')?;
    let rfd: i32 = r.parse().ok()?;
    let wfd: i32 = w.parse().ok()?;
    set_nonblocking(rfd);
    Some(Jobserver(Inner::Pipe { rfd, wfd }))
}

#[cfg(unix)]
fn set_nonblocking(fd: i32) {
    // SAFETY: fcntl on a valid fd; failure is non-fatal (we fall back to a
    // blocking read which would still return a token, just less gracefully).
    unsafe {
        let fl = libc::fcntl(fd, libc::F_GETFL);
        if fl >= 0 {
            libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
        }
    }
}

#[cfg(unix)]
fn pipe_try_read(rfd: i32) -> bool {
    let mut buf = [0u8; 1];
    loop {
        // SAFETY: reading 1 byte into a stack buffer from a valid fd.
        let n = unsafe { libc::read(rfd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        if n == 1 {
            return true;
        }
        if n < 0 {
            let err = std::io::Error::last_os_error();
            match err.raw_os_error() {
                Some(libc::EINTR) => continue,
                _ => return false, // EAGAIN/EWOULDBLOCK: no token available
            }
        }
        return false; // n == 0: EOF
    }
}

#[cfg(unix)]
fn pipe_write(wfd: i32) {
    let buf = [b'+'; 1];
    loop {
        // SAFETY: writing 1 byte from a stack buffer to a valid fd.
        let n = unsafe { libc::write(wfd, buf.as_ptr() as *const libc::c_void, 1) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return;
    }
}

// ---- Windows: a named semaphore shared across the whole process tree --------

#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn init_sem(j: usize) -> Option<Jobserver> {
    use windows_sys::Win32::System::Threading::CreateSemaphoreW;
    let initial = j.saturating_sub(1) as i32; // extra pool tokens
    let maximum = j.max(1) as i32;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!("Local\\redo-msh-js-{}-{}", std::process::id(), nanos);
    let wname = wide(&name);
    // SAFETY: standard CreateSemaphoreW call with a valid name pointer.
    let h = unsafe { CreateSemaphoreW(std::ptr::null(), initial, maximum, wname.as_ptr()) };
    if h.is_null() {
        return None;
    }
    std::env::set_var(ENV, format!("winsem:{name}"));
    Some(Jobserver(Inner::Sem { handle: h as isize }))
}

#[cfg(windows)]
fn open_sem(name: &str) -> Option<Jobserver> {
    use windows_sys::Win32::System::Threading::OpenSemaphoreW;
    const SEMAPHORE_MODIFY_STATE: u32 = 0x0002;
    const SYNCHRONIZE: u32 = 0x0010_0000;
    let wname = wide(name);
    // SAFETY: opening an existing named semaphore created by the top-level.
    let h = unsafe { OpenSemaphoreW(SEMAPHORE_MODIFY_STATE | SYNCHRONIZE, 0, wname.as_ptr()) };
    if h.is_null() {
        return None;
    }
    Some(Jobserver(Inner::Sem { handle: h as isize }))
}

#[cfg(windows)]
fn sem_try_acquire(handle: isize) -> bool {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::WaitForSingleObject;
    let h = handle as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: non-blocking wait (timeout 0) on a valid semaphore handle.
    unsafe { WaitForSingleObject(h, 0) == WAIT_OBJECT_0 }
}

#[cfg(windows)]
fn sem_release(handle: isize) {
    use windows_sys::Win32::System::Threading::ReleaseSemaphore;
    let h = handle as windows_sys::Win32::Foundation::HANDLE;
    // SAFETY: releasing one count on a valid semaphore handle.
    unsafe {
        ReleaseSemaphore(h, 1, std::ptr::null_mut());
    }
}

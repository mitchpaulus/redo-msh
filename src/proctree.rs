//! Killing a do-file's whole process tree, not just its interpreter.
//!
//! SpeculationMP's R5 (drain abandonment), R3 (creation-edge eviction) and
//! R6 (`KillSubtree`) all assume that killing a speculative build kills
//! everything it started. `Child::kill` reaches only the interpreter: a
//! compiler, a `sleep`, a subshell started by the do-file would survive,
//! holding no token (breaking TokenPool's `Bound`) and still writing after
//! the target lock was released and the target re-claimed.
//!
//! Only SPECULATIVE do-files get a tree of their own ([`spawn`] with
//! `own_tree`). Speculation is the only thing redo ever kills, and keeping
//! demanded do-files where they always were — in the parent's process
//! group, sharing the terminal's foreground group with the top-level
//! `redo` — preserves Ctrl-C and interactive tools (`ssh`, `sudo`, `git`
//! credential prompts reading `/dev/tty`) exactly as before.
//!
//! **Unix — process groups.** A speculative do-file leads a new process
//! group, and so does everything it starts (unless a program moves itself
//! out with `setsid`/`setpgid`, which nothing short of cgroups can contain).
//! [`TreeChild::kill_tree`] sends `SIGTERM` to the group, waits for it to
//! empty (up to [`GRACE`]), then `SIGKILL`s whatever is left. A nested
//! redo process inside the group forwards the signal to the groups of its
//! own speculative do-files before dying ([`install_forwarding`]), so the
//! kill cascades down a speculative lineage; it also detects the abort on
//! its own through `REDO_SPEC_WATCH`. The same forwarding makes Ctrl-C
//! (which the terminal delivers only to the foreground group) reach
//! speculative groups, which are in the background.
//!
//! **Windows — job objects.** A speculative do-file is created suspended,
//! assigned to a job object of its own, then resumed, so every process it
//! starts is born inside the job. Jobs nest (Windows 8+): the job sits
//! inside the top-level redo's kill-on-close job (`winjob.rs`), and the
//! jobs that nested redo processes create sit inside it.
//! [`TreeChild::kill_tree`] calls `TerminateJobObject`, which kills every
//! process in the job and in all of its child jobs at once — the whole
//! subtree, exactly, with no cascade needed. If the job cannot be created
//! or assigned (pre-Windows 8 nesting rules), the do-file still runs and
//! the kill falls back to the interpreter alone.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::time::Duration;

/// How long [`TreeChild::kill_tree`] lets a process group exit on
/// `SIGTERM` before `SIGKILL` (Unix).
#[cfg_attr(not(unix), allow(dead_code))]
pub const GRACE: Duration = Duration::from_secs(2);

/// A spawned do-file, together with the handle that can kill its tree.
pub struct TreeChild {
    child: Child,
    #[cfg(unix)]
    pgid: Option<libc::pid_t>,
    #[cfg(windows)]
    job: Option<win::Job>,
}

/// Spawn `cmd`. With `own_tree`, the child and everything it starts can be
/// killed as one tree by [`TreeChild::kill_tree`].
pub fn spawn(cmd: &mut Command, own_tree: bool) -> io::Result<TreeChild> {
    imp::spawn(cmd, own_tree)
}

impl TreeChild {
    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        let st = self.child.wait();
        self.forget_tree();
        st
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let st = self.child.try_wait()?;
        if st.is_some() {
            self.forget_tree();
        }
        Ok(st)
    }

    /// Kill the child and, for an own-tree child, everything it started;
    /// then reap the child. Best-effort: failures are ignored (the child may
    /// already be gone).
    pub fn kill_tree(&mut self) {
        imp::kill_tree(self);
        let _ = self.child.wait();
        self.forget_tree();
    }

    fn forget_tree(&mut self) {
        #[cfg(unix)]
        if let Some(pg) = self.pgid.take() {
            unix::unregister(pg);
        }
        #[cfg(windows)]
        {
            self.job = None; // closes the handle; processes are unaffected
        }
    }
}

impl Drop for TreeChild {
    fn drop(&mut self) {
        self.forget_tree();
    }
}

// ---- Unix --------------------------------------------------------------------

#[cfg(unix)]
use unix as imp;

#[cfg(unix)]
mod unix {
    use super::{TreeChild, GRACE};
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
    use std::time::{Duration, Instant};

    /// Live own-tree process groups of THIS process, read by the signal
    /// handler (atomics only: async-signal-safe). 0 = free slot. If every
    /// slot is taken a group is simply not forwarded to; the explicit
    /// `kill_tree` path does not depend on this table.
    const SLOTS: usize = 512;
    #[allow(clippy::declare_interior_mutable_const)]
    const FREE: AtomicI32 = AtomicI32::new(0);
    static GROUPS: [AtomicI32; SLOTS] = [FREE; SLOTS];

    /// The terminal and the foreground group to restore if a signal kills
    /// this process while an overwrite prompt owns the terminal
    /// (`build::ConsoleForeground`). -1 = no prompt open.
    static PROMPT_FD: AtomicI32 = AtomicI32::new(-1);
    static PROMPT_PREV: AtomicI32 = AtomicI32::new(-1);

    pub(super) fn spawn(cmd: &mut Command, own_tree: bool) -> io::Result<TreeChild> {
        if !own_tree {
            return Ok(TreeChild { child: cmd.spawn()?, pgid: None });
        }
        install_forwarding();
        // The child leads a new group whose id is its pid.
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let pg = child.id() as libc::pid_t;
        register(pg);
        Ok(TreeChild { child, pgid: Some(pg) })
    }

    pub(super) fn kill_tree(tc: &mut TreeChild) {
        let Some(pg) = tc.pgid else {
            let _ = tc.child.kill();
            return;
        };
        // SAFETY: plain signal syscalls on a group id we created. The id
        // cannot name another group while any member of ours exists (POSIX
        // does not reuse a pid that is a live process-group id), and once
        // the group is empty the probe below reports ESRCH and we stop.
        unsafe {
            libc::kill(-pg, libc::SIGTERM);
            let deadline = Instant::now() + GRACE;
            loop {
                // Reap the leader once it has exited, so it stops counting
                // as a member; the rest of the group may still be running.
                let _ = tc.child.try_wait();
                if libc::kill(-pg, 0) != 0 {
                    return; // the whole group is gone
                }
                if Instant::now() >= deadline {
                    libc::kill(-pg, libc::SIGKILL);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    fn register(pg: libc::pid_t) {
        for slot in GROUPS.iter() {
            if slot
                .compare_exchange(0, pg, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    pub(super) fn unregister(pg: libc::pid_t) {
        for slot in GROUPS.iter() {
            if slot
                .compare_exchange(pg, 0, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
        }
    }

    /// Record that a prompt now owns the terminal (`fd`), and which group
    /// to hand it back to if a signal kills us mid-question.
    pub fn prompt_opened(fd: libc::c_int, prev: libc::pid_t) {
        install_forwarding();
        PROMPT_PREV.store(prev, Ordering::Release);
        PROMPT_FD.store(fd, Ordering::Release);
    }

    pub fn prompt_closed() {
        PROMPT_FD.store(-1, Ordering::Release);
    }

    /// Install (once) the handler for SIGINT, SIGTERM and SIGHUP: give the
    /// terminal back if a prompt holds it, forward the signal to every live
    /// own-tree group, then die of the signal as if unhandled. Signals that
    /// were IGNORED when we started (e.g. SIGINT for a background job of a
    /// non-interactive shell, or under `nohup`) are left ignored.
    pub fn install_forwarding() {
        static DONE: AtomicBool = AtomicBool::new(false);
        if DONE.swap(true, Ordering::AcqRel) {
            return;
        }
        for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            // SAFETY: sigaction with a handler that only performs
            // async-signal-safe operations (atomic loads, kill, tcsetpgrp,
            // raise).
            unsafe {
                let mut old: libc::sigaction = std::mem::zeroed();
                if libc::sigaction(sig, std::ptr::null(), &mut old) != 0 {
                    continue;
                }
                if old.sa_sigaction == libc::SIG_IGN {
                    continue;
                }
                let mut sa: libc::sigaction = std::mem::zeroed();
                sa.sa_sigaction = forward as extern "C" fn(libc::c_int) as libc::sighandler_t;
                libc::sigemptyset(&mut sa.sa_mask);
                // Reset to the default on entry, and do not block the signal
                // while handling it, so the closing raise() terminates us.
                sa.sa_flags = libc::SA_RESETHAND | libc::SA_NODEFER;
                libc::sigaction(sig, &sa, std::ptr::null_mut());
            }
        }
    }

    extern "C" fn forward(sig: libc::c_int) {
        // SAFETY: only async-signal-safe calls (see install_forwarding).
        unsafe {
            let fd = PROMPT_FD.load(Ordering::Acquire);
            if fd >= 0 {
                // SIGTTOU is already ignored while a prompt is open.
                libc::tcsetpgrp(fd, PROMPT_PREV.load(Ordering::Acquire));
            }
            for slot in GROUPS.iter() {
                let pg = slot.load(Ordering::Acquire);
                if pg > 0 {
                    libc::kill(-pg, sig);
                }
            }
            libc::raise(sig);
        }
    }
}

#[cfg(unix)]
pub use unix::{prompt_closed, prompt_opened};

// ---- Windows -----------------------------------------------------------------

#[cfg(windows)]
use win as imp;

#[cfg(windows)]
mod win {
    use super::TreeChild;
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;
    use std::process::Command;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        OpenThread, ResumeThread, CREATE_SUSPENDED, THREAD_SUSPEND_RESUME,
    };

    /// An owned job-object handle. Closing it does NOT kill the processes
    /// (no kill-on-close limit): a do-file that finished normally may leave
    /// background processes running, as it always could.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel object reference, usable from any
    // thread.
    unsafe impl Send for Job {}

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: we own the handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) fn spawn(cmd: &mut Command, own_tree: bool) -> io::Result<TreeChild> {
        if !own_tree {
            return Ok(TreeChild { child: cmd.spawn()?, job: None });
        }
        // Suspended, so nothing it starts can predate the job assignment.
        cmd.creation_flags(CREATE_SUSPENDED);
        let mut child = cmd.spawn()?;
        // SAFETY: Win32 calls on handles we own; the child's process handle
        // is valid for the life of `child`.
        let job = unsafe {
            let h = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if h.is_null() {
                None
            } else if AssignProcessToJobObject(h, child.as_raw_handle() as HANDLE) == 0 {
                CloseHandle(h);
                None
            } else {
                Some(Job(h))
            }
        };
        // The child MUST be resumed whatever happened above, or it would
        // sit suspended forever.
        if let Err(e) = resume_process(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        Ok(TreeChild { child, job })
    }

    pub(super) fn kill_tree(tc: &mut TreeChild) {
        match &tc.job {
            // SAFETY: valid job handle we own. Kills every process in the
            // job and in all of its nested jobs.
            Some(job) => unsafe {
                TerminateJobObject(job.0, 1);
            },
            None => {
                let _ = tc.child.kill();
            }
        }
    }

    /// Resume every thread of a process created with CREATE_SUSPENDED (a
    /// new process has exactly one; std does not expose its handle).
    fn resume_process(pid: u32) -> io::Result<()> {
        // SAFETY: Toolhelp snapshot iteration with a correctly sized entry;
        // every handle opened is closed.
        unsafe {
            let snap = CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0);
            if snap == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            let mut te: THREADENTRY32 = std::mem::zeroed();
            te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
            let mut resumed = 0;
            let mut ok = Thread32First(snap, &mut te) != 0;
            while ok {
                if te.th32OwnerProcessID == pid {
                    let th = OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID);
                    if !th.is_null() {
                        if ResumeThread(th) != u32::MAX {
                            resumed += 1;
                        }
                        CloseHandle(th);
                    }
                }
                ok = Thread32Next(snap, &mut te) != 0;
            }
            CloseHandle(snap);
            if resumed == 0 {
                return Err(io::Error::other(format!(
                    "could not resume suspended do-file process {pid}"
                )));
            }
        }
        Ok(())
    }
}

// ---- other platforms ---------------------------------------------------------

#[cfg(not(any(unix, windows)))]
use other as imp;

#[cfg(not(any(unix, windows)))]
mod other {
    use super::TreeChild;
    use std::io;
    use std::process::Command;

    pub(super) fn spawn(cmd: &mut Command, _own_tree: bool) -> io::Result<TreeChild> {
        Ok(TreeChild { child: cmd.spawn()? })
    }

    pub(super) fn kill_tree(tc: &mut TreeChild) {
        let _ = tc.child.kill();
    }
}

//! Windows Job Object setup for guaranteed no-orphan teardown.
//!
//! The top-level process creates a Job Object with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and assigns itself to it. Child
//! processes (msh, recursive redo-msh) are created within the same job by
//! default, so when the top-level exits or is killed — Ctrl-C, taskkill, a
//! crash — the OS closes the job handle and terminates every process still in
//! it. No orphaned builds survive.
//!
//! On Win8+ this nests cleanly under any existing job (e.g. a CI runner's), so
//! the assignment is best-effort and failures are ignored.

#[cfg(windows)]
pub fn setup() {
    use core::ffi::c_void;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    // SAFETY: a straightforward sequence of Win32 job-object calls; every
    // handle/return is checked, and we never dereference an invalid pointer.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = core::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            core::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if ok == 0 {
            CloseHandle(job);
            return;
        }
        // Best-effort: nests under an existing job on Win8+.
        AssignProcessToJobObject(job, GetCurrentProcess());
        // Intentionally do not close `job`: it must stay open for the lifetime
        // of this process so KILL_ON_JOB_CLOSE reaps any survivors when we
        // terminate. The OS closes it automatically on process exit.
    }
}

#[cfg(not(windows))]
pub fn setup() {}

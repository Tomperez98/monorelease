//! Platform-specific process-group and Job-Object lifecycle management.
//!
//! Exposes [`ManagedChild`] so the runner never imports `libc`,
//! `windows-sys`, `CommandExt`, or Job-Object APIs directly.

use std::io;
use std::process::{Child, Command, ExitStatus};

// ---------------------------------------------------------------------------
// Public interface
// ---------------------------------------------------------------------------

/// A child process whose process tree can be terminated reliably.
///
/// On Unix the child runs in its own process group; on Windows it is assigned
// to a Job Object without kill-on-close; successful-exit paths close the job
// handle without terminating live descendants
// (see `runner.rs`).  Callers must
// always call [`terminate_tree`] followed by [`wait`] instead of the plain
// `Child::kill`.
pub(crate) struct ManagedChild {
    child: Child,
    platform: PlatformState,
}

/// Result of a non-blocking wait.
pub(crate) enum WaitResult {
    Running,
    Exited(ExitStatus),
}

impl ManagedChild {
    /// Create a new `ManagedChild` by spawning `command`.
    ///
    /// The spawned process's whole tree will be terminable together.
    pub(crate) fn spawn(command: &mut Command) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Self::spawn_unix(command)
        }
        #[cfg(windows)]
        {
            Self::spawn_windows(command)
        }
    }

    /// Non-blocking check for exit status.
    pub(crate) fn try_wait(&mut self) -> io::Result<WaitResult> {
        match self.child.try_wait()? {
            Some(status) => Ok(WaitResult::Exited(status)),
            None => Ok(WaitResult::Running),
        }
    }

    /// Terminate the entire process tree managed by this child.
    ///
    /// After this call the caller must still call [`wait`] to reap the direct
    /// child.
    pub(crate) fn terminate_tree(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        {
            Self::terminate_tree_unix(&mut self.child, &self.platform)
        }
        #[cfg(windows)]
        {
            Self::terminate_tree_windows(&mut self.child, &self.platform)
        }
    }

    /// Reap the direct child.  Consumes self so the platform state is released
    /// after the child has been reaped.
    pub(crate) fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }

    /// Take the stdout pipe reader.
    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        self.child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>)
    }

    /// Take the stderr pipe reader.
    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        self.child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn std::io::Read + Send>)
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // If the child is still running, terminate the managed tree so
        // descendants don't become orphans, then do a blocking reap.
        // On Windows the job handle is closed *after* the wait.
        if self.child.try_wait().ok().flatten().is_none() {
            if self.terminate_tree().is_err() {
                // The public runner reports the tree-termination error, but
                // Drop must still prevent the direct child from surviving its
                // owner. Descendants may require separate cleanup, which is
                // why the original error remains visible to the caller.
                let _ = self.child.kill();
            }
            let _ = self.child.wait();
        }
        #[cfg(windows)]
        unsafe {
            Self::cleanup_job(&self.platform);
        }
    }
}

// ---------------------------------------------------------------------------
// Platform state types
// ---------------------------------------------------------------------------

#[cfg(unix)]
struct UnixState {
    pgid: i32,
}

#[cfg(windows)]
struct WindowsState {
    job_handle: windows_sys::Win32::Foundation::HANDLE,
}

#[cfg(unix)]
type PlatformState = UnixState;

#[cfg(windows)]
type PlatformState = WindowsState;

// ---------------------------------------------------------------------------
// Unix implementation
// ---------------------------------------------------------------------------

#[cfg(unix)]
impl ManagedChild {
    fn spawn_unix(command: &mut Command) -> io::Result<Self> {
        // Safety: we use CommandExt::process_group(0) to put the child in its
        // own process group whose ID equals the child PID.
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                // Set the process group ID of this (child) process to its own
                // PID, creating a new process group.
                let ret = libc::setpgid(0, 0);
                if ret == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        // In case pre_exec didn't run (vfork weirdness), set the pgid from the
        // parent side as well.  Ignore EACCES (already in the group).
        let pid = child.id() as i32;
        let ret = unsafe { libc::setpgid(pid, pid) };
        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EACCES) {
                // The child is already spawned but we cannot reliably
                // terminate its tree.  Kill and reap the child, then
                // fail closed rather than silently proceeding without
                // a process-group guarantee.
                let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
                let _ = child.wait();
                return Err(err);
            }
        }
        Ok(Self {
            child,
            platform: UnixState { pgid: pid },
        })
    }

    fn terminate_tree_unix(child: &mut Child, platform: &UnixState) -> io::Result<()> {
        let pgid = platform.pgid;
        if pgid <= 0 {
            return Err(io::Error::other("invalid process group id"));
        }

        // Send SIGTERM to the whole process group. Some platforms can reject
        // the graceful signal for a short-lived or already-changing group;
        // that is not a termination failure if the hard kill below succeeds.
        let _ = unsafe { libc::kill(-pgid, libc::SIGTERM) };

        // Give processes a moment to react to SIGTERM, then SIGKILL survivors.
        // Use a short sleep so that well-behaved processes exit gracefully.
        std::thread::sleep(std::time::Duration::from_millis(100));
        let ret = unsafe { libc::kill(-pgid, libc::SIGKILL) };
        if ret == -1 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::ESRCH) {
                // A group can disappear between SIGTERM and SIGKILL while
                // macOS still reports EPERM for the stale group handle. If
                // the direct child is already gone, cleanup succeeded from
                // the runner's perspective and wait() can reap it normally.
                if child.try_wait()?.is_none() {
                    return Err(err);
                }
            }
        }

        // NOTE: we do NOT reap here. The caller must call wait() after
        // terminate_tree() to reap the direct child exactly once.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod windows_impl {
    use std::io;
    use std::mem;
    use std::ptr;
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };

    pub(crate) fn create_job() -> io::Result<HANDLE> {
        unsafe {
            let job = CreateJobObjectW(ptr::null(), ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }

            // NOTE: deliberately NOT setting JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
            // All non-success paths (timeout, output-limit, wait-error, abandoned
            // child) call terminate_tree_windows which does TerminateJobObject.
            // On the normal success path the handle is closed without killing
            // live descendants.  See runner.rs for the success-path logic.
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = mem::zeroed();
            info.BasicLimitInformation.LimitFlags = 0;

            let result = SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const _,
                mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            );
            if result == 0 {
                CloseHandle(job);
                return Err(io::Error::last_os_error());
            }

            Ok(job)
        }
    }

    pub(crate) fn assign_process_to_job(job: HANDLE, process_handle: HANDLE) -> io::Result<()> {
        unsafe {
            // Use AssignProcessToJobObject via the Win32 API.
            let result = windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(
                job,
                process_handle,
            );
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
impl ManagedChild {
    fn spawn_windows(command: &mut Command) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::TerminateProcess;

        // Create a job object before spawning.
        let job_handle = windows_impl::create_job()?;

        // Spawn the process in a suspended state so it cannot create
        // descendants before we assign it to the job.
        const CREATE_SUSPENDED: u32 = 0x00000004;
        command.creation_flags(CREATE_SUSPENDED);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                // Spawn failed; close the job handle before propagating.
                unsafe {
                    windows_sys::Win32::Foundation::CloseHandle(job_handle);
                }
                return Err(e);
            }
        };

        let pid = child.id();
        let process_handle = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;

        // Assign the suspended process to the job object.
        if let Err(err) = windows_impl::assign_process_to_job(job_handle, process_handle) {
            unsafe {
                let _ = TerminateProcess(process_handle, 1);
            }
            let _ = child.wait();
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job_handle);
            }
            return Err(err);
        }

        // Find the primary thread of the suspended process and resume it.
        // Use a toolhelp snapshot to enumerate threads belonging to our PID.
        let resumed = unsafe { resume_primary_thread(pid) };

        if !resumed {
            // Could not resume — the process is permanently stuck.
            unsafe {
                let _ = TerminateProcess(process_handle, 1);
            }
            let _ = child.wait();
            unsafe {
                windows_sys::Win32::Foundation::CloseHandle(job_handle);
            }
            return Err(io::Error::other(
                "failed to resume the suspended child process",
            ));
        }

        Ok(Self {
            child,
            platform: WindowsState { job_handle },
        })
    }

    fn terminate_tree_windows(_child: &mut Child, platform: &WindowsState) -> io::Result<()> {
        use windows_sys::Win32::System::JobObjects::TerminateJobObject;
        // Safety: JobObject handle is valid and we own the job.
        unsafe {
            let result = TerminateJobObject(platform.job_handle, 1);
            if result == 0 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    unsafe fn cleanup_job(platform: &WindowsState) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // Safety: JobObject handle is valid and no longer used.
        unsafe {
            CloseHandle(platform.job_handle);
        }
    }
}

/// Enumerate threads in the system to find and resume the primary thread of
/// `pid`.  The process must have been created with `CREATE_SUSPENDED`, so it
/// has exactly one (suspended) thread.
#[cfg(windows)]
unsafe fn resume_primary_thread(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    // Safety: Toolhelp snapshot creation is safe with valid flags.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return false;
    }

    // Safety: zero-initializing THREADENTRY32 is safe; dwSize is set before use.
    let mut te: THREADENTRY32 = unsafe { std::mem::zeroed() };
    te.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;

    let mut found = false;
    // Safety: Thread32First/Thread32Next read into a properly initialized buffer.
    if unsafe { Thread32First(snapshot, &mut te) != 0 } {
        loop {
            if te.th32OwnerProcessID == pid {
                // Safety: OpenThread with valid thread ID; thread was created suspended.
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, te.th32ThreadID) };
                if !thread.is_null() {
                    // Safety: thread handle is valid and we own it.
                    let resumed = unsafe {
                        let prev = ResumeThread(thread);
                        CloseHandle(thread);
                        // ResumeThread returns u32::MAX on failure.
                        prev != u32::MAX
                    };
                    if resumed {
                        found = true;
                        break;
                    }
                }
            }
            // Safety: Thread32Next advances the iterator into a valid buffer.
            if unsafe { Thread32Next(snapshot, &mut te) == 0 } {
                break;
            }
        }
    }

    // Safety: CloseHandle on a valid toolhelp snapshot.
    unsafe {
        CloseHandle(snapshot);
    }
    found
}

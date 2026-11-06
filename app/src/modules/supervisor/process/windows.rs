//! Windows: there are no signals, so the equivalents are used.
//!
//! - **Graceful stop:** the plugin starts in its own console process group
//!   (`CREATE_NEW_PROCESS_GROUP`), so a Ctrl-Break event can be sent to it
//!   (and its children) alone, the way SIGTERM is on Unix. The default
//!   handler ends the process; plugins can catch it (e.g. tokio's
//!   `signal::windows::ctrl_break`). Console events only reach processes on
//!   the same console as the server: if the server has none (e.g. it runs as
//!   a service), this step is skipped and the plugin is killed after its
//!   `Shutdown` grace period.
//! - **Killing the tree:** the plugin is put in a Job Object that kills every
//!   process in it when terminated or when its handle is closed, so child
//!   processes can't outlive the plugin, nor the server.
//!
//! This module is the only place in the workspace where `unsafe` is
//! allowed. Every block calls a Windows API function with arguments that
//! satisfy its documented contract; each says why.

use std::io;
use std::os::windows::io::RawHandle;
use tokio::process::{Child, Command};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, GenerateConsoleCtrlEvent};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::CREATE_NEW_PROCESS_GROUP;

pub(super) fn configure(command: &mut Command) {
    command.creation_flags(CREATE_NEW_PROCESS_GROUP);
}

pub(super) struct Tree {
    /// The console process group id: the plugin's pid.
    group: Option<u32>,
    job: Option<Job>,
}

impl Tree {
    pub(super) fn attach(child: &Child, tag: &str) -> Self {
        let job = child.raw_handle().and_then(|process| {
            Job::for_process(process)
                .map_err(|err| {
                    log::warn!(
                        "{tag} couldn't put the plugin in a Job Object ({err}); \
                         its child processes may outlive it"
                    )
                })
                .ok()
        });
        Self {
            group: child.id(),
            job,
        }
    }

    /// Ctrl-Break to the plugin's console process group. Returns whether it
    /// was sent.
    pub(super) fn request_stop(&self) -> bool {
        let Some(group) = self.group else {
            return false;
        };
        // SAFETY: plain values only; `group` is the id of a process group we
        // created (CREATE_NEW_PROCESS_GROUP makes it the plugin's pid).
        unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, group) != 0 }
    }

    /// Terminates every process in the Job Object.
    pub(super) fn kill(&self) {
        if let Some(job) = &self.job {
            job.terminate();
        }
    }
}

/// An owned Job Object handle; closing it kills the processes in it.
struct Job(HANDLE);

// SAFETY: a Job Object handle is a kernel handle; Windows allows using it
// from any thread.
unsafe impl Send for Job {}
// SAFETY: as above; the functions used on it are thread-safe.
unsafe impl Sync for Job {}

impl Job {
    fn for_process(process: RawHandle) -> io::Result<Self> {
        // SAFETY: null security attributes and a null name are allowed:
        // default security, unnamed job.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle); // closed on drop from here on

        // SAFETY: an all-zero value is valid for this plain C struct (no
        // limits set); the field set below then enables kill-on-close.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let size = u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
            .expect("the struct is small");
        // SAFETY: `job.0` is a live job handle; the pointer and size describe
        // `info`, which is the struct this information class expects and
        // lives until the call returns.
        let set = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size,
            )
        };
        if set == 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both handles are live: `job.0` is ours, and `process` is the
        // child's handle owned by tokio's `Child`, which outlives this call.
        let assigned = unsafe { AssignProcessToJobObject(job.0, process as HANDLE) };
        if assigned == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn terminate(&self) {
        // SAFETY: `self.0` is a live job handle (closed only on drop).
        unsafe {
            TerminateJobObject(self.0, 1);
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: `self.0` is a live handle we own; it's closed exactly once.
        unsafe {
            CloseHandle(self.0);
        }
    }
}

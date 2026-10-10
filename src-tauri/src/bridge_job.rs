//! Kill-on-close job object for the bridge helper processes.
//!
//! `shutdown_bridges` only runs when Rust code gets a chance to run: a normal
//! exit through `RunEvent::Exit`, or Ctrl+C. A force-kill — Task Manager's "End
//! task", which is `TerminateProcess` — and a crash run no user code at all,
//! which used to leave `bridge64.exe` and `bridge32.exe` behind after the app
//! was gone.
//!
//! A job object carrying `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` moves that cleanup
//! into the kernel. When the last handle to the job closes, every process still
//! in it is terminated; the kernel closes a dying process's handles however it
//! died, without running any user code. So the bridges go away with the app even
//! when the app never gets to say goodbye.
//!
//! Only handles from processes this app spawned itself may be assigned — a job
//! close would terminate anything else put in here, including a game the user is
//! only speed-hacking.

use std::ffi::c_void;
use std::os::windows::io::AsRawHandle;
use std::process::Child;
use std::sync::OnceLock;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

use crate::applog;

/// Kernel handle to the job the bridges live in.
///
/// Deliberately never closed. It stays valid for the whole process lifetime, and
/// the kernel closing it at process death is precisely the signal that tears the
/// bridges down — so a `Drop` impl would turn every drop into a kill order.
/// `HANDLE` wraps a raw pointer and is therefore neither `Send` nor `Sync`; a
/// job handle is process-wide and every job API is safe from any thread, so the
/// manual impls are sound.
struct JobHandle(HANDLE);

unsafe impl Send for JobHandle {}
unsafe impl Sync for JobHandle {}

/// Created once on first use. `None` means setup failed and was already logged —
/// cached so the failure is reported once instead of per bridge.
static BRIDGE_JOB: OnceLock<Option<JobHandle>> = OnceLock::new();

/// The bridge job, created on first call. `None` if it could not be set up, in
/// which case the bridges run unprotected — the pre-existing behaviour — rather
/// than not at all.
fn bridge_job() -> Option<HANDLE> {
    BRIDGE_JOB.get_or_init(create_job).as_ref().map(|job| job.0)
}

fn create_job() -> Option<JobHandle> {
    unsafe {
        let job = match CreateJobObjectW(None, PCWSTR::null()) {
            Ok(handle) => handle,
            Err(e) => {
                applog::error("bridge", format!(
                    "CreateJobObject failed ({e}) — bridge processes will survive a force-kill of the app"
                ));
                return None;
            }
        };

        // Only KILL_ON_JOB_CLOSE, and nothing else: any further limit flag
        // (memory, active processes, UI) would change how a bridge behaves.
        //
        // Unnamed, with null security attributes, which also leaves the handle
        // non-inheritable — required. A bridge holding a handle to its own job
        // would keep the count above zero forever and kill-on-close could never
        // fire.
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

        // The extended struct and its full size: the smaller basic one is
        // rejected with ERROR_INVALID_PARAMETER.
        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) {
            applog::error("bridge", format!(
                "SetInformationJobObject(kill-on-close) failed ({e}) — no force-kill cleanup for the bridges"
            ));
            let _ = CloseHandle(job);
            return None;
        }

        applog::info(
            "bridge",
            "cleanup job created — bridge processes will die with the app",
        );
        Some(JobHandle(job))
    }
}

/// Put a just-spawned bridge into the cleanup job.
///
/// Never fatal: a bridge that could not be assigned still works, it is simply
/// not cleaned up if the app is force-killed. Returns whether membership was
/// confirmed.
pub fn assign(child: &Child, name: &str) -> bool {
    let Some(job) = bridge_job() else {
        return false;
    };
    // The process handle lives as long as `Child` does, so it stays valid here
    // even if the process has already exited.
    let process = HANDLE(child.as_raw_handle());

    if let Err(e) = unsafe { AssignProcessToJobObject(job, process) } {
        applog::warn(
            "bridge",
            format!(
            "could not put {name} (pid {}) in the cleanup job ({e}) — it will survive a force-kill",
            child.id()
        ),
        );
        return false;
    }

    // Read membership back rather than trusting the call. This is the line that
    // turns "we invoked the API" into a statement of fact in the log, which is
    // the only evidence available when diagnosing a machine we cannot reach.
    let mut member = BOOL::default();
    if unsafe { IsProcessInJob(process, job, &mut member) }.is_ok() && member.as_bool() {
        applog::info(
            "bridge",
            format!("{name} (pid {}) joined the cleanup job", child.id()),
        );
        true
    } else {
        applog::warn(
            "bridge",
            format!(
                "{name} (pid {}) is not confirmed in the cleanup job",
                child.id()
            ),
        );
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the setup that is easy to get silently wrong: the extended limit
    /// struct must be passed with its own (larger) size, and the flag has to be
    /// `KILL_ON_JOB_CLOSE`. Either mistake is rejected by the kernel, and the
    /// only symptom in production is a WARN in a log nobody reads until the day
    /// bridges start being left behind again.
    #[test]
    fn a_spawned_process_joins_the_cleanup_job() {
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "60", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn a stand-in child");

        let confirmed = assign(&child, "test-child");

        let _ = child.kill();
        let _ = child.wait();

        assert!(
            confirmed,
            "child was not confirmed as a member of the cleanup job"
        );
    }
}

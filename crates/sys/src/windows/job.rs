//! Job Objects: the Windows grouping for a unit's processes.
//!
//! A unit's leader starts inside its own job, and every process it starts joins
//! the job too unless it asks to break away. The job is only for grouping and
//! killing: no memory, CPU or process-count limits are set, and closing the last
//! handle does not kill the members, so units outlive a daemon crash.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOBOBJECT_BASIC_ACCOUNTING_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};

use super::security::SecurityAttributes;
use super::token::{owned, raw};

// `members` reads the process ID list header as one usize holding two u32 counts.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("devlanes supports 64-bit Windows (x64 and arm64) only");

/// A job object that only the current user can open.
#[derive(Debug)]
pub struct Job(OwnedHandle);

impl Job {
    /// Creates an unnamed job. With `breakaway`, processes inside may leave it by
    /// asking for `CREATE_BREAKAWAY_FROM_JOB` (some tools do, and fail to start
    /// otherwise); the runner then tracks them by parent and start time.
    ///
    /// `DIE_ON_UNHANDLED_EXCEPTION` keeps a crashed member from waiting on an
    /// invisible Windows Error Reporting dialog.
    pub fn create(breakaway: bool) -> io::Result<Self> {
        let mut attributes = SecurityAttributes::current_user_only()?;
        // SAFETY: `attributes` outlives the call; a null name makes an unnamed job.
        let handle = unsafe { CreateJobObjectW(attributes.as_mut_ptr(), ptr::null()) };
        let job = Self(owned(handle)?);
        // SAFETY: an all-zero value is valid for this plain C struct.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION
            | if breakaway {
                JOB_OBJECT_LIMIT_BREAKAWAY_OK
            } else {
                0
            };
        // SAFETY: `job` is open; `limits` matches the information class and size.
        let ok = unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// PIDs of the processes in the job right now, exited members excluded.
    pub fn members(&self) -> io::Result<Vec<u32>> {
        // The list is a header (two u32 counts) followed by usize PIDs; usize
        // elements keep it aligned. It is regrown while members are added.
        let mut capacity = 64_usize;
        loop {
            let mut buffer = vec![0_usize; 1 + capacity];
            let bytes = (buffer.len() * size_of::<usize>()) as u32;
            // SAFETY: `buffer` holds `bytes` writable, aligned bytes.
            let ok = unsafe {
                QueryInformationJobObject(
                    self.raw(),
                    JobObjectBasicProcessIdList,
                    buffer.as_mut_ptr().cast(),
                    bytes,
                    ptr::null_mut(),
                )
            };
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(ERROR_MORE_DATA as i32) {
                    capacity *= 4;
                    continue;
                }
                return Err(error);
            }
            let assigned = (buffer[0] & 0xffff_ffff) as usize;
            let listed = (buffer[0] >> 32) as usize;
            if assigned > listed && listed == capacity {
                capacity *= 4;
                continue;
            }
            return Ok(buffer[1..=listed.min(capacity)]
                .iter()
                .filter_map(|&pid| u32::try_from(pid).ok())
                .collect());
        }
    }

    /// How many processes in the job are still running.
    pub fn active_count(&self) -> io::Result<u32> {
        // SAFETY: an all-zero value is valid for this plain C struct.
        let mut info: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `info` matches the information class and size.
        let ok = unsafe {
            QueryInformationJobObject(
                self.raw(),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(info.ActiveProcesses)
    }

    /// Ends every process still in the job with `exit_code`.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self` is an open job handle with full access.
        if unsafe { TerminateJobObject(self.raw(), exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// The handle for Win32 calls; ownership stays with the job.
    pub(crate) fn raw(&self) -> HANDLE {
        raw(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::spawn::{Spawn, Stdio, spawn};

    /// A child started in the job, and the grandchild it starts, are members until
    /// the job is terminated; without breakaway, leaving the job is refused.
    #[test]
    fn members_follow_the_tree_and_terminate_together() -> io::Result<()> {
        let job = Job::create(false)?;
        let cmd = std::path::PathBuf::from(
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into()),
        );
        let child = spawn(&Spawn {
            program: &cmd,
            command_line: std::ffi::OsStr::new(r#"cmd.exe /d /c "ping -n 30 127.0.0.1 >NUL""#),
            environment: None,
            cwd: &std::env::temp_dir(),
            stdout: Stdio::Null,
            stderr: Stdio::Null,
            job: Some(&job),
            console: None,
        })?;
        // The ping grandchild joins shortly after cmd starts it.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while job.members()?.len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let members = job.members()?;
        assert!(members.contains(&child.pid()), "{members:?}");
        assert!(members.len() >= 2, "{members:?}");
        assert!(job.active_count()? >= 2);
        job.terminate(7)?;
        assert_eq!(child.wait()?, 7);
        assert_eq!(job.active_count()?, 0);
        Ok(())
    }
}

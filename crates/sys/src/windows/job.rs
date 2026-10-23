//! Job Objects: the Windows grouping for a unit's processes, and for a helper
//! whose whole tree has to end with it ([`Job::spawn_helper`]).
//!
//! A unit's leader starts inside its own job, and every process it starts joins
//! the job too: jobs refuse breakaway (see [`Job::create`]). The job is only for
//! grouping and killing: no memory, CPU or process-count limits are set, and
//! closing the last handle does not kill the members, so units outlive a daemon
//! crash. Helper jobs from [`Job::create_kill_on_close`] do the opposite.

use std::io;
use std::os::windows::io::OwnedHandle;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject,
};
use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

use super::security::SecurityAttributes;
use super::token::{owned, raw};

// `members` reads the process ID list header as one usize holding two u32 counts.
#[cfg(not(target_pointer_width = "64"))]
compile_error!("devlanes supports 64-bit Windows (x64 and arm64) only");

/// A job object that only the current user can open.
#[derive(Debug)]
pub struct Job(OwnedHandle);

impl Job {
    /// Creates an unnamed job that processes inside may not leave.
    ///
    /// Breakaway is refused because Git for Windows' MSYS2 runtime, which runs
    /// every `run:` string and ready probe, asks for `CREATE_BREAKAWAY_FROM_JOB`
    /// for every child it starts whenever the job allows it. A child whose
    /// short-lived parent (a fork before exec) no snapshot saw would then escape
    /// tracking by parent too. A tool that insists on breaking away gets access
    /// denied instead.
    ///
    /// `DIE_ON_UNHANDLED_EXCEPTION` keeps a crashed member from waiting on an
    /// invisible Windows Error Reporting dialog.
    pub fn create() -> io::Result<Self> {
        Self::with_limits(0)
    }

    /// Creates an unnamed job, without breakaway, that ends every member when
    /// its last handle closes: when the owner drops it, and when the owning
    /// process dies, crash included. For a helper whose whole tree must not
    /// outlive devlanes, such as `ssh.exe` and the `ProxyCommand` it starts.
    pub fn create_kill_on_close() -> io::Result<Self> {
        Self::with_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)
    }

    /// Creates the job with `DIE_ON_UNHANDLED_EXCEPTION` plus `flags`.
    fn with_limits(flags: u32) -> io::Result<Self> {
        let mut attributes = SecurityAttributes::current_user_only()?;
        // SAFETY: `attributes` outlives the call; a null name makes an unnamed job.
        let handle = unsafe { CreateJobObjectW(attributes.as_mut_ptr(), ptr::null()) };
        let job = Self(owned(handle)?);
        // SAFETY: an all-zero value is valid for this plain C struct.
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION | flags;
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

    /// Ends every process still in the job with `exit_code`.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: `self` is an open job handle with full access.
        if unsafe { TerminateJobObject(self.raw(), exit_code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Starts a helper program (see [`crate::NoWindow`]) inside this job from
    /// its first instruction, so everything it starts is a member too. For
    /// short-lived helpers whose whole tree has to end with them, such as a
    /// ready probe behind Git's `sh.exe` launcher.
    ///
    /// The child is created suspended, added to the job, then resumed; a new
    /// job without UI limits always takes it. Creation flags set on `command`
    /// before are replaced by `CREATE_NO_WINDOW` when this process has no
    /// console, as `no_window` sets. A child that cannot be resumed is killed.
    pub fn spawn_helper(
        &self,
        command: &mut tokio::process::Command,
    ) -> io::Result<tokio::process::Child> {
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;
        command.creation_flags(CREATE_SUSPENDED | crate::command::window_flags());
        let mut child = command.spawn()?;
        let adopted = match child.raw_handle() {
            // SAFETY: the job handle is open, and `handle` is the child's,
            // valid while `child` has not been waited for.
            Some(handle) if unsafe { AssignProcessToJobObject(self.raw(), handle) } != 0 => {
                child.id().map_or_else(
                    || Err(io::Error::other("the new process has no ID")),
                    resume,
                )
            }
            Some(_) => Err(io::Error::last_os_error()),
            None => Err(io::Error::other(
                "the new process has already been waited for",
            )),
        };
        if let Err(error) = adopted {
            // Still suspended (or never resumed): it has run no code yet.
            let _ = child.start_kill();
            return Err(error);
        }
        Ok(child)
    }

    /// The handle for Win32 calls; ownership stays with the job.
    pub(crate) fn raw(&self) -> HANDLE {
        raw(&self.0)
    }
}

/// Resumes the threads of process `pid`, which was created suspended and so
/// has only its initial thread. The caller holds the process's handle, so the
/// PID cannot name another process meanwhile.
fn resume(pid: u32) -> io::Result<()> {
    // SAFETY: plain call; the result is checked before use.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
    if snapshot as isize == -1 {
        return Err(io::Error::last_os_error());
    }
    let snapshot = owned(snapshot)?;
    let mut entry = THREADENTRY32 {
        dwSize: size_of::<THREADENTRY32>() as u32,
        // SAFETY: an all-zero value is valid for the rest of this plain C struct.
        ..unsafe { std::mem::zeroed() }
    };
    let mut resumed = 0;
    // SAFETY: `snapshot` is open and `entry.dwSize` is set as the API requires.
    let mut more = unsafe { Thread32First(raw(&snapshot), &raw mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == pid {
            // SAFETY: plain call; a null result is checked by `owned`.
            let thread =
                owned(unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) })?;
            // SAFETY: `thread` is open with THREAD_SUSPEND_RESUME.
            if unsafe { ResumeThread(raw(&thread)) } == u32::MAX {
                return Err(io::Error::last_os_error());
            }
            resumed += 1;
        }
        // SAFETY: as above, continuing the same enumeration.
        more = unsafe { Thread32Next(raw(&snapshot), &raw mut entry) } != 0;
    }
    if resumed == 0 {
        return Err(io::Error::other("the new process has no thread to resume"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::spawn::{Spawn, Stdio, spawn};

    /// A child started in the job, and the grandchild it starts, are members until
    /// the job is terminated; without breakaway, leaving the job is refused.
    #[test]
    fn members_follow_the_tree_and_terminate_together() -> io::Result<()> {
        let job = Job::create()?;
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
        job.terminate(7)?;
        assert_eq!(child.wait()?, 7);
        assert!(job.members()?.is_empty());
        Ok(())
    }

    /// A job whose members are ended when the test drops it, pass or fail.
    struct Ending(Job);

    impl Drop for Ending {
        fn drop(&mut self) {
            let _ = self.0.terminate(1);
        }
    }

    /// A helper started through the job runs (it was resumed), and the
    /// grandchild it starts joins the job, so terminating the job ends both.
    #[tokio::test]
    async fn helpers_start_inside_the_job() -> io::Result<()> {
        let job = Ending(Job::create()?);
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/d", "/c", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = job.0.spawn_helper(&mut command)?;
        let pid = child.id().ok_or_else(|| io::Error::other("no pid"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while job.0.members()?.len() < 2 && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        let members = job.0.members()?;
        assert!(members.contains(&pid), "{members:?}");
        assert!(
            members.len() >= 2,
            "the grandchild is not a member: {members:?}"
        );
        job.0.terminate(7)?;
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .map_err(|_| io::Error::other("the helper outlived its job"))??;
        assert_eq!(status.code(), Some(7));
        assert!(job.0.members()?.is_empty());
        Ok(())
    }

    /// A program that cannot start fails as it would outside a job, and the
    /// job is left empty.
    #[tokio::test]
    async fn missing_helpers_are_spawn_failures() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let job = Ending(Job::create_kill_on_close()?);
        let mut command = tokio::process::Command::new(dir.path().join("missing.exe"));
        command.kill_on_drop(true);
        match job.0.spawn_helper(&mut command) {
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound, "{error}"),
            Ok(_) => panic!("a missing program started"),
        }
        assert!(job.0.members()?.is_empty());
        Ok(())
    }

    /// Dropping a kill-on-close job ends the helper and the grandchild it
    /// started, which a kill of the helper alone would leave running.
    #[tokio::test]
    async fn kill_on_close_jobs_end_the_tree_when_dropped() -> io::Result<()> {
        let job = Job::create_kill_on_close()?;
        let mut command = tokio::process::Command::new("cmd.exe");
        command
            .args(["/d", "/c", "ping -n 30 127.0.0.1 >NUL"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        let mut child = job.spawn_helper(&mut command)?;
        let pid = child.id().ok_or_else(|| io::Error::other("no pid"))?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let grandchild = loop {
            if let Some(&other) = job.members()?.iter().find(|&&member| member != pid) {
                break other;
            }
            if std::time::Instant::now() >= deadline {
                return Err(io::Error::other("the grandchild never joined the job"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        };
        drop(job);
        let ended = tokio::task::spawn_blocking(move || {
            crate::windows::process::wait_exit(grandchild, std::time::Duration::from_secs(10))
        })
        .await
        .map_err(io::Error::other)??;
        assert!(ended, "the grandchild outlived the job");
        tokio::time::timeout(std::time::Duration::from_secs(10), child.wait())
            .await
            .map_err(|_| io::Error::other("the helper outlived its job"))??;
        Ok(())
    }
}

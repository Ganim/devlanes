//! Job objects: the Windows grouping for a unit's processes.
//!
//! Jobs created here never allow breakaway. Git for Windows' MSYS2 runtime
//! asks for `CREATE_BREAKAWAY_FROM_JOB` for every child it starts whenever
//! the job allows it, so a job that allowed it would lose every process a
//! `sh -c` starts. A tool that insists on breaking away gets access denied.

use std::io;
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_MORE_DATA, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, IsProcessInJob, JOB_OBJECT_LIMIT_BREAKAWAY_OK,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use crate::proc::{ProcessEntry, process_where};
use crate::util::{owned, raw};

/// Limits of a new job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobOptions {
    /// Closing the job's last handle ends every member, including when the
    /// owning process dies. For helpers whose whole tree must not outlive the
    /// owner; unit jobs leave it off so units survive a daemon crash.
    pub kill_on_close: bool,
    /// A member that crashes ends at once instead of waiting on an invisible
    /// Windows Error Reporting dialog.
    pub die_on_unhandled_exception: bool,
}

/// An unnamed job object. Dropping it closes the handle, which ends the
/// members only with [`JobOptions::kill_on_close`].
#[derive(Debug)]
pub struct Job(OwnedHandle);

impl Job {
    /// Creates an unnamed job with `options` that members may not leave. Add
    /// processes through [`crate::spawn`], which starts them inside the job.
    pub fn create(options: JobOptions) -> io::Result<Self> {
        // SAFETY: null attributes give the default security and a handle that
        // is not inherited; a null name makes an unnamed job.
        let handle = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        let job = Self(owned(handle)?);
        let mut flags = 0;
        if options.kill_on_close {
            flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if options.die_on_unhandled_exception {
            flags |= JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        }
        if flags != 0 {
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = flags;
            // SAFETY: `job` is open; `limits` matches the class and size.
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
        }
        Ok(job)
    }

    /// PIDs of the processes in the job now; exited members are not listed.
    pub fn members(&self) -> io::Result<Vec<u32>> {
        let header = std::mem::offset_of!(JOBOBJECT_BASIC_PROCESS_ID_LIST, ProcessIdList);
        let mut capacity = 64_usize;
        // Members can be added between two reads; a few rounds of growing
        // settle any real unit.
        for _ in 0..8 {
            // usize elements keep the list aligned for its header and PIDs.
            let mut buffer = vec![0_usize; header.div_ceil(size_of::<usize>()) + capacity];
            let bytes = u32::try_from(buffer.len() * size_of::<usize>())
                .map_err(|_| io::Error::other("job member list too large"))?;
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
            // SAFETY: the buffer starts with the list's header, written by the
            // call (also when it reports more data) or still zero.
            let list = unsafe { &*buffer.as_ptr().cast::<JOBOBJECT_BASIC_PROCESS_ID_LIST>() };
            let (assigned, listed) = (
                list.NumberOfAssignedProcesses as usize,
                list.NumberOfProcessIdsInList as usize,
            );
            if ok == 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(ERROR_MORE_DATA.cast_signed()) {
                    return Err(error);
                }
                capacity = capacity.max(assigned) * 2;
                continue;
            }
            let ids = &buffer[header.div_ceil(size_of::<usize>())..];
            return Ok(ids[..listed.min(ids.len())]
                .iter()
                .filter_map(|&pid| u32::try_from(pid).ok())
                .collect());
        }
        Err(io::Error::other("the job kept growing while being read"))
    }

    /// The processes in the job now. Each listed PID is opened and confirmed
    /// as a member on that handle, so a PID that another process took over
    /// after the list was read is never reported as a member. Members that
    /// exit or refuse queries meanwhile are left out.
    pub fn processes(&self) -> io::Result<Vec<ProcessEntry>> {
        let mut found = Vec::new();
        for pid in self.members()? {
            if let Ok(Some(entry)) = process_where(pid, |handle| self.contains(handle)) {
                found.push(entry);
            }
        }
        Ok(found)
    }

    /// Whether the process behind `handle` is in this job.
    fn contains(&self, handle: &OwnedHandle) -> bool {
        let mut member = 0;
        // SAFETY: both handles are open; the process handle has query rights.
        let ok = unsafe { IsProcessInJob(raw(handle), self.raw(), &raw mut member) };
        ok != 0 && member != 0
    }

    /// How many processes in the job are still running.
    pub fn active_count(&self) -> io::Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: the job is open; `info` matches the class and size.
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

    /// Ends every process in the job with `exit_code`.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the job handle is open with full access.
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

impl AsHandle for Job {
    /// The job handle, open with full access.
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.0.as_handle()
    }
}

impl AsRawHandle for Job {
    /// The job handle; ownership stays with the job.
    fn as_raw_handle(&self) -> std::os::windows::io::RawHandle {
        self.0.as_raw_handle()
    }
}

/// The job this process runs in, as far as it limits what this process starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobFacts {
    /// This process is in a job.
    pub in_job: bool,
    /// Closing the job's last handle ends every process in it, which is how
    /// some terminals (VS Code's, CI runners) clean up when they close.
    pub kill_on_close: bool,
    /// Children may leave the job with `CREATE_BREAKAWAY_FROM_JOB`.
    pub breakaway_ok: bool,
}

/// Facts about the job this process runs in; all `false` outside any job.
/// With nested jobs, the limits are those of the innermost one.
pub fn job_of_self() -> io::Result<JobFacts> {
    let mut in_job = 0;
    // SAFETY: the pseudo handle is always valid; a null job asks about any job.
    if unsafe { IsProcessInJob(GetCurrentProcess(), ptr::null_mut(), &raw mut in_job) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if in_job == 0 {
        return Ok(JobFacts {
            in_job: false,
            kill_on_close: false,
            breakaway_ok: false,
        });
    }
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    // SAFETY: a null job handle queries the caller's job; `info` matches the
    // class and size.
    let ok = unsafe {
        QueryInformationJobObject(
            ptr::null_mut(),
            JobObjectExtendedLimitInformation,
            (&raw mut info).cast(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = info.BasicLimitInformation.LimitFlags;
    Ok(JobFacts {
        in_job: true,
        kill_on_close: flags & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE != 0,
        breakaway_ok: flags
            & (JOB_OBJECT_LIMIT_BREAKAWAY_OK | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK)
            != 0,
    })
}

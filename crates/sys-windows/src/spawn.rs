//! Process creation: one `CreateProcessW` primitive that passes exactly the
//! handles a child should inherit, can start it inside a job or a pseudo
//! console from its first instruction, and keeps its handle so the PID cannot
//! be reused while the caller tracks it.

use std::cmp::Ordering;
use std::ffi::{OsStr, OsString, c_void};
use std::fs::File;
use std::io::{self, PipeReader, PipeWriter};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, OwnedHandle};
use std::path::Path;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::System::Console::HPCON;
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW,
    CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, InitializeProcThreadAttributeList,
    LPPROC_THREAD_ATTRIBUTE_LIST, PROC_THREAD_ATTRIBUTE_HANDLE_LIST,
    PROC_THREAD_ATTRIBUTE_JOB_LIST, PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
};

use crate::conpty::PseudoConsole;
use crate::job::Job;
use crate::proc::{creation_time, exit_code_of, has_exited, wait_handle};
use crate::util::{owned, raw, wide};

/// Where one standard stream of the child goes.
#[derive(Debug, Clone, Copy)]
pub enum Stream<'a> {
    /// The `NUL` device.
    Null,
    /// A handle of this process, such as the child's end of a [`pipe`]. It
    /// does not need to be inheritable. `INVALID_HANDLE_VALUE` (a missing
    /// standard handle) counts as [`Stream::Null`].
    Handle(BorrowedHandle<'a>),
}

impl<'a> Stream<'a> {
    /// `self`, with `INVALID_HANDLE_VALUE` read as [`Stream::Null`].
    fn or_null(self) -> Self {
        match self {
            Self::Handle(handle) if handle.as_raw_handle() == INVALID_HANDLE_VALUE => Self::Null,
            other => other,
        }
    }
}

/// The child's standard streams.
#[derive(Debug, Clone, Copy)]
pub enum Stdio<'a> {
    /// These three handles. `stdout` and `stderr` may be the same handle.
    Handles {
        /// Standard input.
        stdin: Stream<'a>,
        /// Standard output.
        stdout: Stream<'a>,
        /// Standard error.
        stderr: Stream<'a>,
    },
    /// The pseudo console's input and output; the child gets no standard
    /// handles of its own, so it uses the console's.
    PseudoConsole(&'a PseudoConsole),
}

/// Creation flags beyond the ones every spawn sets
/// (`CREATE_UNICODE_ENVIRONMENT`, `EXTENDED_STARTUPINFO_PRESENT`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CreationFlags {
    /// `CREATE_NO_WINDOW`: a console of its own without a window, so no
    /// window flashes and console control events can still reach it.
    pub no_window: bool,
    /// `DETACHED_PROCESS`: no console at all.
    pub detached_process: bool,
    /// `CREATE_NEW_PROCESS_GROUP`: the child leads a new process group and
    /// starts with Ctrl-C ignored, which its children inherit.
    pub new_process_group: bool,
    /// `CREATE_BREAKAWAY_FROM_JOB`: leave this process's job. Fails with
    /// access denied when that job does not allow breakaway.
    pub breakaway_from_job: bool,
}

impl CreationFlags {
    /// The flags for `CreateProcessW`.
    fn bits(self) -> u32 {
        let mut bits = 0;
        for (set, flag) in [
            (self.no_window, CREATE_NO_WINDOW),
            (self.detached_process, DETACHED_PROCESS),
            (self.new_process_group, CREATE_NEW_PROCESS_GROUP),
            (self.breakaway_from_job, CREATE_BREAKAWAY_FROM_JOB),
        ] {
            if set {
                bits |= flag;
            }
        }
        bits
    }
}

/// What to start.
#[derive(Debug, Clone, Copy)]
pub struct Spawn<'a> {
    /// The executable, already resolved; `PATH` is not searched. Batch files
    /// are started through `cmd.exe`, so this is `cmd.exe` for them.
    pub program: &'a Path,
    /// The whole command line, `argv[0]` included, quoted by the caller for
    /// the program's parser. It is copied into a writable buffer, as
    /// `CreateProcessW` requires.
    pub command_line: &'a OsStr,
    /// The child's environment, or `None` to inherit this process's.
    pub environment: Option<&'a Environment>,
    /// The working directory, or `None` for this process's.
    pub cwd: Option<&'a Path>,
    /// The standard streams.
    pub stdio: Stdio<'a>,
    /// Further handles the child inherits besides its standard streams.
    /// `INVALID_HANDLE_VALUE` is refused with `ErrorKind::InvalidInput`.
    pub inherit: &'a [BorrowedHandle<'a>],
    /// The job the child runs in from its first instruction.
    pub job: Option<&'a Job>,
    /// Extra creation flags.
    pub flags: CreationFlags,
}

/// Starts `spec`.
///
/// The child inherits only its standard handles and [`Spawn::inherit`],
/// through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. Those must be inheritable for
/// that, so inheritable duplicates exist from just before `CreateProcessW`
/// until right after it. A `std::process` or tokio spawn on another thread in
/// that moment starts with every inheritable handle of this process and may
/// get these duplicates too. That only delays end of file on a pipe until
/// that short-lived helper exits, so it is accepted instead of serialising
/// every spawn in the process. Handles are never inherited without a handle
/// list.
///
/// With a pseudo console the standard handle fields are set to null with
/// `STARTF_USESTDHANDLES`; otherwise the child would write to this process's
/// redirected output instead of the console.
///
/// If the child's creation time cannot be read, the child is terminated,
/// since it could not be tracked or stopped safely.
pub fn spawn(spec: &Spawn<'_>) -> io::Result<Child> {
    let program = wide(spec.program.as_os_str())?;
    // CreateProcessW may write into the command line, so it gets its own buffer.
    let mut command_line = wide(spec.command_line)?;
    let cwd = spec.cwd.map(|cwd| wide(cwd.as_os_str())).transpose()?;

    // -1 is also the pseudo-handle of the current process, so duplicating it
    // would give the child a full-access handle to this one.
    if spec
        .inherit
        .iter()
        .any(|handle| handle.as_raw_handle() == INVALID_HANDLE_VALUE)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "INVALID_HANDLE_VALUE cannot be inherited",
        ));
    }
    let stdio = match spec.stdio {
        Stdio::Handles {
            stdin,
            stdout,
            stderr,
        } => Stdio::Handles {
            stdin: stdin.or_null(),
            stdout: stdout.or_null(),
            stderr: stderr.or_null(),
        },
        console @ Stdio::PseudoConsole(_) => console,
    };

    // Sources of inherited handles, in the order std slots then extras.
    let null = match stdio {
        Stdio::Handles {
            stdin,
            stdout,
            stderr,
        } if [stdin, stdout, stderr]
            .iter()
            .any(|stream| matches!(stream, Stream::Null)) =>
        {
            Some(File::options().read(true).write(true).open("NUL")?)
        }
        _ => None,
    };
    let source = |stream: Stream<'_>| -> HANDLE {
        match (stream, &null) {
            (Stream::Handle(handle), _) => handle.as_raw_handle(),
            (Stream::Null, Some(null)) => null.as_raw_handle(),
            // `null` is opened whenever a stream is `Null`.
            (Stream::Null, None) => ptr::null_mut(),
        }
    };
    let std_sources = match stdio {
        Stdio::Handles {
            stdin,
            stdout,
            stderr,
        } => Some([source(stdin), source(stdout), source(stderr)]),
        Stdio::PseudoConsole(_) => None,
    };
    let mut sources: Vec<HANDLE> = std_sources.into_iter().flatten().collect();
    sources.extend(spec.inherit.iter().map(|handle| handle.as_raw_handle()));

    // One duplicate per distinct source, so a pipe passed as both stdout and
    // stderr is inherited once; a handle list must not repeat a handle.
    let mut unique: Vec<HANDLE> = Vec::new();
    for handle in &sources {
        if !unique.contains(handle) {
            unique.push(*handle);
        }
    }
    let duplicates = unique
        .iter()
        .map(|&handle| inheritable(handle))
        .collect::<io::Result<Vec<OwnedHandle>>>()?;
    let list: Vec<HANDLE> = duplicates.iter().map(raw).collect();
    let duplicate_of = |handle: HANDLE| {
        unique
            .iter()
            .position(|&known| known == handle)
            .map_or(ptr::null_mut(), |index| list[index])
    };

    let pseudo_console = match stdio {
        Stdio::PseudoConsole(console) => Some(console.raw()),
        Stdio::Handles { .. } => None,
    };
    let job = spec.job.map(Job::raw);
    let count = u32::from(!list.is_empty())
        + u32::from(pseudo_console.is_some())
        + u32::from(job.is_some());
    let mut attributes = AttributeList::new(count)?;
    // `list`, `pseudo_console` and `job` outlive the list's use below.
    if !list.is_empty() {
        attributes.update(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            list.as_ptr().cast(),
            size_of_val(list.as_slice()),
        )?;
    }
    if let Some(console) = pseudo_console {
        // The attribute's value is the HPCON itself, not a pointer to it.
        attributes.update(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            console as *const c_void,
            size_of::<HPCON>(),
        )?;
    }
    if let Some(job) = &job {
        attributes.update(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            ptr::from_ref(job).cast(),
            size_of::<HANDLE>(),
        )?;
    }

    let mut info = STARTUPINFOEXW::default();
    info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    if let Some([stdin, stdout, stderr]) = std_sources {
        info.StartupInfo.hStdInput = duplicate_of(stdin);
        info.StartupInfo.hStdOutput = duplicate_of(stdout);
        info.StartupInfo.hStdError = duplicate_of(stderr);
    }
    info.lpAttributeList = attributes.as_mut_ptr();
    let flags = CREATE_UNICODE_ENVIRONMENT | EXTENDED_STARTUPINFO_PRESENT | spec.flags.bits();
    let mut process = PROCESS_INFORMATION::default();
    // SAFETY: every pointer refers to a live, NUL-terminated buffer or a
    // structure owned by this function; the environment block is
    // double-NUL-terminated UTF-16 as CREATE_UNICODE_ENVIRONMENT says; handle
    // inheritance is on only with a handle list, which limits it to `list`.
    let ok = unsafe {
        CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            i32::from(!list.is_empty()),
            flags,
            spec.environment
                .map_or(ptr::null(), |environment| environment.0.as_ptr().cast()),
            cwd.as_ref().map_or(ptr::null(), |cwd| cwd.as_ptr()),
            &raw const info.StartupInfo,
            &raw mut process,
        )
    };
    let error = (ok == 0).then(io::Error::last_os_error);
    drop(attributes);
    drop(duplicates);
    if let Some(error) = error {
        return Err(error);
    }
    // The primary thread's handle is not needed; the process handle is kept.
    drop(owned(process.hThread));
    let handle = owned(process.hProcess)?;
    let start_time = match creation_time(&handle) {
        Ok(start_time) => start_time,
        Err(error) => {
            // SAFETY: `handle` is the new process's handle, with all access.
            unsafe { TerminateProcess(raw(&handle), 1) };
            return Err(error);
        }
    };
    Ok(Child {
        handle,
        pid: process.dwProcessId,
        start_time,
    })
}

/// A started process. Holding it keeps the PID from being reused, the role an
/// unreaped child plays on Unix; dropping it does not stop the process.
#[derive(Debug)]
pub struct Child {
    handle: OwnedHandle,
    pid: u32,
    start_time: u64,
}

impl Child {
    /// The process ID.
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// The creation time, the start time of the process's identity.
    pub fn start_time(&self) -> u64 {
        self.start_time
    }

    /// Blocks until the process exits and returns its exit code.
    pub fn wait(&self) -> io::Result<u32> {
        wait_handle(&self.handle, None)?;
        exit_code_of(&self.handle)
    }

    /// Waits up to `timeout`; the exit code once the process has exited,
    /// `None` while it runs.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<u32>> {
        if !wait_handle(&self.handle, Some(timeout))? {
            return Ok(None);
        }
        exit_code_of(&self.handle).map(Some)
    }

    /// The exit code once the process has exited, `None` while it runs.
    pub fn exit_code(&self) -> io::Result<Option<u32>> {
        if !has_exited(&self.handle) {
            return Ok(None);
        }
        exit_code_of(&self.handle).map(Some)
    }

    /// Ends the process with `exit_code`; nothing happens once it has exited.
    pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
        // SAFETY: the handle CreateProcessW returned has all access.
        if unsafe { TerminateProcess(raw(&self.handle), exit_code) } == 0 {
            let error = io::Error::last_os_error();
            if has_exited(&self.handle) {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }
}

impl AsHandle for Child {
    /// The process handle, with all access.
    fn as_handle(&self) -> BorrowedHandle<'_> {
        self.handle.as_handle()
    }
}

/// An anonymous pipe whose ends are both not inheritable. Pass the child's
/// end to [`spawn`] as a [`Stream::Handle`]; it gets an inheritable duplicate
/// only for the moment of the spawn. Close the child's end in this process
/// after the spawn, or the reader never sees end of file.
pub fn pipe() -> io::Result<(PipeReader, PipeWriter)> {
    let (mut read, mut write): (HANDLE, HANDLE) = (ptr::null_mut(), ptr::null_mut());
    // SAFETY: both out pointers are writable; null attributes give handles
    // that are not inherited, and size 0 asks for the default buffer.
    if unsafe { CreatePipe(&raw mut read, &raw mut write, ptr::null(), 0) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let (read, write) = (owned(read)?, owned(write)?);
    Ok((PipeReader::from(read), PipeWriter::from(write)))
}

/// An environment block for [`Spawn::environment`]: `NAME=VALUE` entries
/// sorted by name as Windows orders them, each NUL-terminated, then a NUL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Environment(Vec<u16>);

/// Why an environment cannot be passed to a child.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EnvError {
    /// A variable has an empty name.
    #[error("an environment variable has an empty name")]
    EmptyName,
    /// A name or value contains a NUL, which would end the entry early (a
    /// value `x\0EVIL=1` would add a variable).
    #[error("environment variable {0:?} contains a NUL character")]
    Nul(OsString),
    /// A name contains `=`, which separates name and value.
    #[error("environment variable name {0:?} contains '='")]
    Equals(OsString),
    /// Two names differ only in case, so they are one variable on Windows.
    #[error("environment variables {0:?} and {1:?} differ only in case")]
    Duplicate(OsString, OsString),
}

impl Environment {
    /// Builds a block from `variables`. Refuses empty names, a NUL anywhere,
    /// `=` in a name (also the `=C:` drive variables, which callers drop),
    /// and names that are equal ignoring case.
    pub fn new<I, K, V>(variables: I) -> Result<Self, EnvError>
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let mut entries: Vec<(Vec<u16>, Vec<u16>)> = Vec::new();
        for (name, value) in variables {
            let (name, value) = (name.as_ref(), value.as_ref());
            let units: Vec<u16> = name.encode_wide().collect();
            let value: Vec<u16> = value.encode_wide().collect();
            if units.is_empty() {
                return Err(EnvError::EmptyName);
            }
            if units.contains(&0) || value.contains(&0) {
                return Err(EnvError::Nul(name.to_owned()));
            }
            if units.contains(&u16::from(b'=')) {
                return Err(EnvError::Equals(name.to_owned()));
            }
            entries.push((units, value));
        }
        entries.sort_by(|(a, _), (b, _)| compare_names(a, b));
        for pair in entries.windows(2) {
            if compare_names(&pair[0].0, &pair[1].0) == Ordering::Equal {
                return Err(EnvError::Duplicate(
                    OsString::from_wide(&pair[0].0),
                    OsString::from_wide(&pair[1].0),
                ));
            }
        }
        let mut block = Vec::new();
        for (name, value) in entries {
            block.extend(name);
            block.push(u16::from(b'='));
            block.extend(value);
            block.push(0);
        }
        // An empty block is two NULs, as an empty entry list plus the end.
        if block.is_empty() {
            block.push(0);
        }
        block.push(0);
        Ok(Self(block))
    }

    /// The block as UTF-16 units, terminators included.
    pub fn as_units(&self) -> &[u16] {
        &self.0
    }
}

/// Orders names the way Windows does: by UTF-16 unit, each uppercased alone.
fn compare_names(a: &[u16], b: &[u16]) -> Ordering {
    a.iter()
        .map(|&unit| upcase(unit))
        .cmp(b.iter().map(|&unit| upcase(unit)))
}

/// One UTF-16 unit uppercased the way Windows compares names: units with a
/// single-unit uppercase form map to it, the rest (surrogates, `ß`) stay.
fn upcase(unit: u16) -> u16 {
    let Some(c) = char::from_u32(u32::from(unit)) else {
        return unit;
    };
    let mut upper = c.to_uppercase();
    match (upper.next(), upper.next()) {
        (Some(single), None) => u16::try_from(u32::from(single)).unwrap_or(unit),
        _ => unit,
    }
}

/// An inheritable duplicate of `handle`, closed when dropped.
fn inheritable(handle: HANDLE) -> io::Result<OwnedHandle> {
    let mut duplicate: HANDLE = ptr::null_mut();
    // SAFETY: `handle` is open in this process for the call; the duplicate is
    // a new handle owned by the returned value.
    let ok = unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle,
            GetCurrentProcess(),
            &raw mut duplicate,
            0,
            1,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    owned(duplicate)
}

/// A `PROC_THREAD_ATTRIBUTE_LIST` in an owned, aligned buffer.
struct AttributeList(Vec<usize>);

impl AttributeList {
    /// An initialised list with room for `count` attributes.
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: a size query with a null list; it fails and reports the size.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &raw mut size) };
        let mut buffer = vec![0_usize; size.div_ceil(size_of::<usize>()).max(1)];
        // SAFETY: `buffer` holds at least `size` writable, aligned bytes.
        let ok = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &raw mut size)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }

    /// The list for Win32 calls.
    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// Sets one attribute. `value` must stay valid until the process is created.
    fn update(&mut self, attribute: usize, value: *const c_void, size: usize) -> io::Result<()> {
        // SAFETY: the list was initialised in `new` with room for every
        // attribute set; the caller keeps `value` alive.
        let ok = unsafe {
            windows_sys::Win32::System::Threading::UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                attribute,
                value,
                size,
                ptr::null_mut(),
                ptr::null(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        // SAFETY: the list was initialised in `new` and is not used after this.
        unsafe { DeleteProcThreadAttributeList(self.as_mut_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `INVALID_HANDLE_VALUE`, which is also this process's pseudo-handle,
    /// is `NUL` as a standard stream and refused as an extra handle, so a
    /// child never gets a handle to this process.
    #[test]
    fn invalid_handles_are_never_duplicated() -> io::Result<()> {
        // SAFETY: nothing closes or uses the value; `spawn` only compares it.
        let invalid = unsafe { BorrowedHandle::borrow_raw(INVALID_HANDLE_VALUE) };
        let program = crate::file::system_directory()?.join("cmd.exe");
        let extra = [invalid];
        let mut spec = Spawn {
            program: &program,
            command_line: OsStr::new(r#"cmd.exe /d /c "echo hidden""#),
            environment: None,
            cwd: None,
            stdio: Stdio::Handles {
                stdin: Stream::Handle(invalid),
                stdout: Stream::Handle(invalid),
                stderr: Stream::Null,
            },
            inherit: &extra,
            job: None,
            flags: CreationFlags {
                no_window: true,
                ..CreationFlags::default()
            },
        };
        let refused = spawn(&spec).expect_err("refused");
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{refused}");
        spec.inherit = &[];
        let child = spawn(&spec)?;
        assert_eq!(child.wait_timeout(Duration::from_secs(20))?, Some(0));
        Ok(())
    }
}

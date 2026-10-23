//! Starting a unit's leader: `CreateProcessW` with exactly the handles it should
//! inherit, inside its job from the first instruction, and without a console
//! window.

use std::collections::BTreeMap;
use std::ffi::{OsStr, c_void};
use std::io;
use std::os::windows::io::{AsRawHandle, BorrowedHandle, OwnedHandle};
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{
    DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, WAIT_OBJECT_0,
};
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_UNICODE_ENVIRONMENT, CreateProcessW, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, GetCurrentProcess, GetExitCodeProcess, INFINITE,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess, UpdateProcThreadAttribute,
    WaitForSingleObject,
};

use windows_sys::Win32::System::Console::HPCON;
use windows_sys::Win32::System::Threading::PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE;

use super::conpty::PseudoConsole;
use super::job::Job;
use super::proc::{Console, creation_time, exited};
use super::security::wide;
use super::token::{owned, raw};

/// Where one of the child's standard streams goes.
#[derive(Debug, Clone, Copy)]
pub enum Stdio<'a> {
    /// The `NUL` device.
    Null,
    /// An open handle of this process, such as a pipe's write end.
    Handle(BorrowedHandle<'a>),
}

/// What to start. The child's stdin is always `NUL`.
#[derive(Debug)]
pub struct Spawn<'a> {
    /// The executable, already resolved; `PATH` is not searched.
    pub program: &'a Path,
    /// The whole command line, `argv[0]` included, quoted for `CommandLineToArgvW`
    /// or for `cmd.exe`, as the caller decided.
    pub command_line: &'a OsStr,
    /// The environment, or `None` to inherit this process's. Names must be unique
    /// ignoring case, as Windows treats them.
    pub environment: Option<&'a BTreeMap<String, String>>,
    /// The working directory the child starts in.
    pub cwd: &'a Path,
    /// Where the child's standard output goes.
    pub stdout: Stdio<'a>,
    /// Where the child's standard error goes; may be the same handle as `stdout`.
    pub stderr: Stdio<'a>,
    /// The job the child runs in from its first instruction.
    pub job: Option<&'a Job>,
    /// A pseudo console to attach the child to instead of `stdout`/`stderr`.
    pub console: Option<&'a PseudoConsole>,
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

    /// Blocks until the process exits and returns its exit code. Call it from a
    /// blocking thread.
    pub fn wait(&self) -> io::Result<u32> {
        // SAFETY: the handle is open; INFINITE waits until the process ends.
        if unsafe { WaitForSingleObject(raw(&self.handle), INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error());
        }
        self.exit_code()?
            .ok_or_else(|| io::Error::other("process still running after its wait"))
    }

    /// The console the process is attached to now, held so its host's PID
    /// stays its own; `None` while it has none, as before its console is set
    /// up.
    pub fn console(&self) -> io::Result<Option<Console>> {
        Console::of(&self.handle)
    }

    /// The exit code once the process has exited, `None` while it runs.
    pub fn exit_code(&self) -> io::Result<Option<u32>> {
        if !exited(&self.handle) {
            return Ok(None);
        }
        let mut code = 0;
        // SAFETY: the handle is open with query rights.
        if unsafe { GetExitCodeProcess(raw(&self.handle), &raw mut code) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Some(code))
    }
}

/// Starts `spec`.
///
/// Without a pseudo console the child runs without a console window
/// (`CREATE_NO_WINDOW`: it still gets a console, so console control events can
/// reach it, but no window flashes), and only its three standard handles are
/// inherited, through `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`. They have to be
/// inheritable for that, so inheritable duplicates exist from just before
/// `CreateProcessW` until right after; a `std::process` spawn on another thread in
/// that moment could inherit them too, which only delays end of file on a unit's
/// pipe until that helper exits.
///
/// With a pseudo console, `stdout` and `stderr` are ignored and nothing is
/// inherited: the child's standard handles are the console's. The standard handle
/// fields are still set (to null), or the child would write to this process's
/// redirected output instead.
pub fn spawn(spec: &Spawn<'_>) -> io::Result<Child> {
    // Checked first, so a bad variable fails before anything is created.
    let environment = spec.environment.map(environment_block).transpose()?;
    let inherited = match spec.console {
        Some(_) => None,
        None => {
            let null = std::fs::File::options()
                .read(true)
                .write(true)
                .open("NUL")?;
            let handle_of = |stdio: Stdio<'_>| match stdio {
                Stdio::Null => null.as_raw_handle(),
                Stdio::Handle(handle) => handle.as_raw_handle(),
            };
            Some([
                inheritable(null.as_raw_handle())?,
                inheritable(handle_of(spec.stdout))?,
                inheritable(handle_of(spec.stderr))?,
            ])
        }
    };
    let raw_inherited = inherited
        .as_ref()
        .map(|handles| handles.each_ref().map(raw));

    let mut attributes = AttributeList::new(1 + u32::from(spec.job.is_some()))?;
    // `raw_inherited` and `console` outlive the list's use below.
    if let Some(console) = spec.console {
        attributes.update(
            PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE as usize,
            console.attribute_value(),
            size_of::<HPCON>(),
        )?;
    } else if let Some(handles) = &raw_inherited {
        attributes.update(
            PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
            handles.as_ptr().cast(),
            size_of_val(handles),
        )?;
    }
    let job = spec.job.map(Job::raw);
    if let Some(job) = &job {
        attributes.update(
            PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            ptr::from_ref(job).cast(),
            size_of::<HANDLE>(),
        )?;
    }

    // SAFETY: an all-zero value is valid for this plain C struct.
    let mut info: STARTUPINFOEXW = unsafe { std::mem::zeroed() };
    info.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    info.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    if let Some([stdin, stdout, stderr]) = raw_inherited {
        info.StartupInfo.hStdInput = stdin;
        info.StartupInfo.hStdOutput = stdout;
        info.StartupInfo.hStdError = stderr;
    }
    info.lpAttributeList = attributes.as_mut_ptr();
    let (inherit, window) = match spec.console {
        Some(_) => (0, 0),
        None => (1, CREATE_NO_WINDOW),
    };

    let program = wide(spec.program.as_os_str());
    // CreateProcessW may write into the command line, so it gets its own buffer.
    let mut command_line = wide(spec.command_line);
    let cwd = wide(spec.cwd.as_os_str());
    // SAFETY: an all-zero value is valid for this plain C struct.
    let mut process: PROCESS_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: every pointer refers to a live, NUL-terminated buffer or structure
    // owned by this function; handle inheritance is limited by the attribute list.
    let ok = unsafe {
        CreateProcessW(
            program.as_ptr(),
            command_line.as_mut_ptr(),
            ptr::null(),
            ptr::null(),
            inherit,
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | window,
            environment
                .as_ref()
                .map_or(ptr::null(), |block| block.as_ptr().cast::<c_void>()),
            cwd.as_ptr(),
            &raw const info.StartupInfo,
            &raw mut process,
        )
    };
    let error = (ok == 0).then(io::Error::last_os_error);
    drop(attributes);
    drop(inherited);
    if let Some(error) = error {
        return Err(error);
    }
    // The primary thread's handle is not needed; the process handle is kept.
    drop(owned(process.hThread));
    let handle = owned(process.hProcess)?;
    let start_time = match creation_time(&handle) {
        Ok(start_time) => start_time,
        Err(error) => {
            // Without its identity the child could never be tracked or stopped
            // safely, so it must not keep running.
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

/// An inheritable duplicate of `handle`, closed when dropped.
fn inheritable(handle: HANDLE) -> io::Result<OwnedHandle> {
    let mut duplicate: HANDLE = ptr::null_mut();
    // SAFETY: `handle` is open in this process; the duplicate is a new handle
    // owned by the returned value.
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

/// A `PROC_THREAD_ATTRIBUTE_LIST` in an owned, aligned buffer. Every access that
/// can write to it goes through `&mut self`.
struct AttributeList(Vec<usize>);

impl AttributeList {
    /// An initialised list with room for `count` attributes.
    fn new(count: u32) -> io::Result<Self> {
        let mut size = 0;
        // SAFETY: a size query with a null list; it fails and reports the size.
        unsafe { InitializeProcThreadAttributeList(ptr::null_mut(), count, 0, &raw mut size) };
        let mut buffer = vec![0_usize; size.div_ceil(size_of::<usize>())];
        // SAFETY: `buffer` holds at least `size` writable, aligned bytes.
        let ok = unsafe {
            InitializeProcThreadAttributeList(buffer.as_mut_ptr().cast(), count, 0, &raw mut size)
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(buffer))
    }

    /// The list for Win32 calls, which may write to it.
    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.0.as_mut_ptr().cast()
    }

    /// Sets one attribute. `value` must stay valid until the process is created.
    fn update(&mut self, attribute: usize, value: *const c_void, size: usize) -> io::Result<()> {
        // SAFETY: the list was initialised in `new`; the caller keeps `value` alive.
        let ok = unsafe {
            UpdateProcThreadAttribute(
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

/// A Unicode environment block: `KEY=VALUE\0` entries sorted by key without
/// regard to case, as Windows expects, then an extra `\0`.
///
/// Refuses what would corrupt the block or make a variable ambiguous: a NUL
/// anywhere (a value `x\0EVIL=1` would add a variable), an empty name or one with
/// `=` after its first character, and two names that differ only in case.
fn environment_block(environment: &BTreeMap<String, String>) -> io::Result<Vec<u16>> {
    let invalid = |message: String| io::Error::new(io::ErrorKind::InvalidInput, message);
    let mut entries: Vec<(&String, &String)> = environment.iter().collect();
    entries.sort_by_key(|(key, _)| key.to_uppercase());
    for pair in entries.windows(2) {
        // Windows folds non-ASCII letters too, as the sort above does.
        if pair[0].0.to_uppercase() == pair[1].0.to_uppercase() {
            return Err(invalid(format!(
                "environment variables {} and {} are one variable on Windows",
                pair[0].0, pair[1].0
            )));
        }
    }
    let mut block = Vec::new();
    for (key, value) in entries {
        if key.is_empty()
            || key.chars().skip(1).any(|c| c == '=')
            || key.contains('\0')
            || value.contains('\0')
        {
            return Err(invalid(format!(
                "environment variable {key:?} cannot be passed"
            )));
        }
        block.extend(key.encode_utf16());
        block.push(u16::from(b'='));
        block.extend(value.encode_utf16());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;

    /// Output reaches the given pipe, the environment is exactly the one given,
    /// and the exit code comes back.
    #[test]
    fn output_environment_and_exit_code() -> io::Result<()> {
        let (mut reader, writer) = std::io::pipe()?;
        let cmd = Path::new(r"C:\Windows\System32\cmd.exe");
        let environment = BTreeMap::from([
            ("SystemRoot".to_owned(), r"C:\Windows".to_owned()),
            ("DEVLANES_PROBE".to_owned(), "here".to_owned()),
        ]);
        let child = spawn(&Spawn {
            program: cmd,
            command_line: OsStr::new(r#"cmd.exe /d /c "echo %DEVLANES_PROBE% %HOME%& exit 3""#),
            environment: Some(&environment),
            cwd: &std::env::temp_dir(),
            stdout: Stdio::Handle(std::os::windows::io::AsHandle::as_handle(&writer)),
            stderr: Stdio::Null,
            job: None,
            console: None,
        })?;
        drop(writer);
        let mut output = String::new();
        reader.read_to_string(&mut output)?;
        assert_eq!(output.trim(), "here %HOME%");
        assert_eq!(child.wait()?, 3);
        assert!(child.start_time() > 0);
        Ok(())
    }

    /// Keys are ordered without case, which Windows requires of the block.
    #[test]
    fn environment_blocks_sort_without_case() -> io::Result<()> {
        let block = environment_block(&BTreeMap::from([
            ("b".to_owned(), "2".to_owned()),
            ("A".to_owned(), "1".to_owned()),
        ]))?;
        assert_eq!(String::from_utf16_lossy(&block), "A=1\0b=2\0\0");
        assert_eq!(environment_block(&BTreeMap::new())?, [0, 0]);
        // Drive-current-directory variables legitimately start with `=`.
        environment_block(&BTreeMap::from([("=C:".to_owned(), r"C:\x".to_owned())]))?;
        Ok(())
    }

    /// Anything that would corrupt the block or duplicate a variable is refused.
    #[test]
    fn environment_blocks_refuse_ambiguity() {
        for (key, value) in [
            ("", "x"),
            ("A=B", "x"),
            ("A\0B", "x"),
            ("A", "x\0EVIL=1"),
            ("ÿ=", "x"),
        ] {
            let refused = environment_block(&BTreeMap::from([(key.to_owned(), value.to_owned())]));
            assert!(refused.is_err(), "{key:?}={value:?}");
        }
        let duplicated = environment_block(&BTreeMap::from([
            ("Path".to_owned(), "a".to_owned()),
            ("PATH".to_owned(), "b".to_owned()),
        ]));
        assert!(duplicated.is_err());
    }
}

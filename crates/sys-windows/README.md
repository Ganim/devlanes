# devlanes-sys-windows

Safe wrappers over the Win32 calls devlanes makes on Windows. The `sys` facade, the Windows runner, the client and the daemon call it and add their own policy; it is the only Windows crate allowed `unsafe` code, and it is empty on other platforms.

## Usage

```rust
use std::ffi::OsStr;
use std::os::windows::io::AsHandle;
use std::path::Path;

use devlanes_sys_windows::job::{Job, JobOptions};
use devlanes_sys_windows::spawn::{self, CreationFlags, Environment, Spawn, Stdio, Stream};
use devlanes_sys_windows::{proc, security};

fn run(cmd: &Path, home: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Err(problem) = security::ensure_private_dir(home)? {
        return Err(format!("{} is not private: {problem:?}", home.display()).into());
    }

    let job = Job::create(JobOptions { die_on_unhandled_exception: true, ..JobOptions::default() })?;
    let (output, writer) = spawn::pipe()?;
    let environment = Environment::new([("SystemRoot", r"C:\Windows"), ("PORT", "3000")])?;
    let child = spawn::spawn(&Spawn {
        program: cmd,
        command_line: OsStr::new(r#"cmd.exe /d /c "echo %PORT%""#),
        environment: Some(&environment),
        cwd: None,
        stdio: Stdio::Handles { stdin: Stream::Null, stdout: Stream::Handle(writer.as_handle()), stderr: Stream::Null },
        inherit: &[],
        job: Some(&job),
        flags: CreationFlags { no_window: true, ..CreationFlags::default() },
    })?;
    drop(writer);

    // Later, possibly after a daemon restart: act only if the PID was not reused.
    proc::terminate_verified(child.pid(), child.start_time(), 1)?;
    drop(output);
    Ok(())
}
```

Every call blocks. Async callers use `spawn_blocking`, except for the `pipe` module, which is built on tokio's named pipes.

## API

- `token`: `Sid` (binary, strict text parsing), `current_user`, `current_facts` (elevation, integrity level, restricted, AppContainer).
- `security`: `SecurityDescriptor` from SDDL, `ensure_private_dir`, `check_private_dir`, `check_private_file` with a `Problem` when something is not private.
- `pipe`: `PipeFactory` for server instances with an explicit descriptor, `server_owner` for the client's check.
- `proc`: `processes`, `process`, `matches`, `Held` (a verified, pinned process), `terminate_verified`, `listening_tcp`, `established_tcp` (every established connection, accepted or not), `boot_id`.
- `power`: `PowerHandle`, a verified, pinned process opened for one `PowerAccess`: suspend and resume once each, or read and set its `PriorityClass` and `PowerThrottling` (EcoQoS) for efficiency mode. `resume_verified` for processes a crashed daemon left suspended.
- `job`: `Job` (no breakaway, members, processes verified as members, active count, terminate), `job_of_self`.
- `spawn`: `spawn` (one `CreateProcessW` with handle list, job list or pseudo console), `Environment`, `pipe`.
- `console`: `Attached` for the console-control helper, `alloc_console` (`AllocConsoleWithOptions`), `enable_vt_output`, `keyboard` (console, mintty or none), `raw_vt_input`, `hidden_input`, `read_console_utf16`, `release_stdout`.
- `window`: `close_windows` for verified processes, `watch_session_end` for logoff and shutdown, `visible_windows` for tests that check no window appears.
- `conpty`, `usage`, `registry` (HKCU values; typed reads and subkey lists of HKLM too), `shell_execute::open_url`, `shell_execute::run_elevated` (UAC prompt, waits for the exit code), `file` (identity, retrying rename and removal, `system_directory`).
- `cert`: `CertStore` over the user's certificate stores (logical view read-only, changes through the user's registry store), `interactive_desktop`; the `test-support` feature adds `ScratchStore`, a throwaway store deleted on drop.
- `net`: `set_exclusive_address_use` and `exclusive_address_use` (`SO_EXCLUSIVEADDRUSE`).

## Behaviour

A process's identity is its PID plus its creation `FILETIME` as a `u64`. Functions that act on a recorded process open it and compare that time on the same handle first, so a reused PID is never reached.

`spawn` makes inheritable duplicates of the handles it passes only around the `CreateProcessW` call. A `std::process` spawn on another thread at that moment may inherit them too, which only delays end of file on a pipe until that helper exits.

Adding a certificate to or deleting one from the user's `Root` store shows a Windows security warning and blocks until the user answers; no flag suppresses it. No other store prompts, so the tests only change scratch stores.

Requires Windows 11 24H2 (build 26100) or later.

## License

Apache-2.0

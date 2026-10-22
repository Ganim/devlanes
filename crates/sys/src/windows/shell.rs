//! Opening web URLs with the user's default browser, and running a program as
//! administrator through the UAC prompt.

use std::ffi::OsStr;
use std::io;
use std::ptr;

use windows_sys::Win32::Foundation::ERROR_NO_ASSOCIATION;
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::security::wide;

/// Opens an `http` or `https` URL with the default browser, the way a click in
/// Explorer does. The URL goes to the shell as one string, so no command line is
/// parsed (unlike `cmd /c start`, where `&` and quotes in URLs split commands).
///
/// Refused with [`io::ErrorKind::InvalidInput`]: other schemes and plain paths,
/// which the shell's `open` verb would run or open as files, and URLs with
/// quotes, whitespace, control characters or non-ASCII text, which the browser
/// would receive on a command line that the registered handler builds by
/// pasting the URL into its template. URLs devlanes prints are plain ASCII.
pub fn open_url(url: &str) -> io::Result<()> {
    check_url(url)?;
    let _com = Com::init();
    let verb = wide(OsStr::new("open"));
    let file = wide(OsStr::new(url));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC: the call returns only once the browser was started, since the
        // calling thread may end right after. NO_UI: errors come back to the
        // caller instead of as a dialog box.
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..SHELLEXECUTEINFOW::default()
    };
    // SAFETY: `info` is a correctly sized SHELLEXECUTEINFOW whose string fields
    // point to NUL-terminated UTF-16 that outlives the call; the null owner
    // window, parameters and directory are documented as allowed, and no process
    // handle is requested.
    if unsafe { ShellExecuteExW(&raw mut info) } != 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    Err(match error.raw_os_error().map(|code| code as u32) {
        Some(ERROR_NO_ASSOCIATION) => io::Error::new(
            io::ErrorKind::NotFound,
            "no program is registered to open web URLs",
        ),
        _ => io::Error::other(format!("the shell could not open the URL: {error}")),
    })
}

/// `ERROR_CANCELLED`: the user answered No at the UAC prompt.
pub const ERROR_CANCELLED: i32 = 1223;

/// Runs `program` with the command line `parameters` as administrator, the way
/// "Run as administrator" does: Windows shows the UAC prompt (on the secure
/// desktop), then starts the program elevated and hidden. Waits for it to exit
/// and returns its exit code. Blocks the calling thread, prompt included, so
/// call it off async executors.
///
/// A No at the prompt fails with raw OS error [`ERROR_CANCELLED`]. The prompt
/// needs a visible desktop; check `cert::interactive_desktop` first. `parameters`
/// is passed as is, so quote arguments the way the program parses them.
pub fn run_elevated(program: &std::path::Path, parameters: &str) -> io::Result<u32> {
    run_elevated_child(program, parameters).map(|exit| exit.code)
}

/// How an elevated program ended, and which process it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ElevatedExit {
    /// The elevated process's ID, for matching it with a pipe client
    /// (`pipe::client_process_id`). It was alive, and its handle held, from the
    /// start until the exit, so no other process had this ID meanwhile.
    pub pid: u32,
    /// Its exit code.
    pub code: u32,
}

/// [`run_elevated`], also returning the elevated process's ID.
pub fn run_elevated_child(program: &std::path::Path, parameters: &str) -> io::Result<ElevatedExit> {
    use std::os::windows::io::{FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, GetProcessId, INFINITE, WaitForSingleObject,
    };
    use windows_sys::Win32::UI::Shell::SEE_MASK_NOCLOSEPROCESS;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;

    let _com = Com::init();
    let verb = wide(OsStr::new("runas"));
    let file = wide(program.as_os_str());
    let parameters = wide(OsStr::new(parameters));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOCLOSEPROCESS: hand back the process to wait on. NOASYNC: the call
        // returns once the process exists. NO_UI: errors come back as codes.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..SHELLEXECUTEINFOW::default()
    };
    // SAFETY: `info` is a correctly sized SHELLEXECUTEINFOW whose string fields
    // point to NUL-terminated UTF-16 that outlives the call; on success with
    // NOCLOSEPROCESS, `hProcess` is a process handle this function now owns.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if info.hProcess.is_null() {
        return Err(io::Error::other(
            "the elevated program did not start a process",
        ));
    }
    // SAFETY: the handle was just returned to us and is closed once, on drop.
    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess) };
    // SAFETY: `process` is a valid process handle with query access.
    let pid = unsafe { GetProcessId(info.hProcess) };
    if pid == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `process` is a valid process handle with SYNCHRONIZE access.
    if unsafe { WaitForSingleObject(info.hProcess, INFINITE) } != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error());
    }
    let mut code = 0u32;
    // SAFETY: `process` has PROCESS_QUERY_LIMITED_INFORMATION; `code` is writable.
    if unsafe { GetExitCodeProcess(info.hProcess, &raw mut code) } == 0 {
        return Err(io::Error::last_os_error());
    }
    drop(process);
    Ok(ElevatedExit { pid, code })
}

/// Accepts only `http` and `https` URLs made of printable ASCII other than space
/// and `"`.
fn check_url(url: &str) -> io::Result<()> {
    let web = ["http://", "https://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
    });
    if !web {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http and https URLs can be opened",
        ));
    }
    if !url
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'"')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URLs with quotes, spaces, control characters or non-ASCII text are not opened",
        ));
    }
    Ok(())
}

/// COM on the calling thread for the length of a shell call, which may hand the
/// URL to a COM-based handler. Uninitialises only what it initialised: when the
/// thread already uses another apartment (`RPC_E_CHANGED_MODE`) or the call
/// fails, the shell call runs without it and nothing is undone.
struct Com(bool);

impl Com {
    /// Joins a single-threaded apartment without OLE1 DDE, as `ShellExecuteEx`
    /// documents.
    fn init() -> Self {
        // SAFETY: plain call with a null reserved pointer.
        let result = unsafe {
            CoInitializeEx(
                ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        // S_OK and S_FALSE (already initialised the same way) both need a
        // matching CoUninitialize; failures, RPC_E_CHANGED_MODE included, do not.
        Self(result >= 0)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx in `init`; the guard
            // lives in one `open_url` call, so it drops on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_web_urls_reach_the_shell() {
        for refused in [
            "C:\\Windows\\System32\\calc.exe",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:",
            "",
            "http:",
            "http://example.invalid/\0",
            "https://example.invalid/\" --new-window \"calc",
            "https://example.invalid/a b",
            "https://example.invalid/\ta",
            "https://example.invalid/\r\n",
            "https://ex\u{e4}mple.invalid/",
            "https://example.invalid/\u{7f}",
        ] {
            let error = check_url(refused).expect_err(refused);
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{refused:?}");
            let error = open_url(refused).expect_err(refused);
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{refused:?}");
        }
        for accepted in [
            "http://example.invalid",
            "HTTPS://api.acme.localhost:8443/a?b=c&d=%20#e",
        ] {
            assert!(check_url(accepted).is_ok(), "{accepted}");
        }
    }
}

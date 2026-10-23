//! Opening web URLs with the user's default browser, and running a program
//! as administrator after the UAC prompt.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::ptr;

use windows_sys::Win32::Foundation::{ERROR_CANCELLED, WAIT_OBJECT_0};
use windows_sys::Win32::System::Com::{
    COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
};
use windows_sys::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows_sys::Win32::UI::Shell::{
    SEE_MASK_FLAG_NO_UI, SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW,
    ShellExecuteExW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

use crate::util::{owned, raw, wide};

/// Failure of [`run_elevated`].
#[derive(Debug, thiserror::Error)]
pub enum ElevateError {
    /// The user answered No in the UAC prompt.
    #[error("administrator approval was declined")]
    Cancelled,
    /// The session has no visible window station (SSH, a service), so the
    /// UAC prompt could not be shown.
    #[error("Windows needs to ask for administrator approval, but this session has no desktop")]
    NoDesktop,
    /// Any other failure.
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Runs `program` with `args` as administrator and returns its exit code once
/// it ends. Windows shows the UAC prompt first (none when the caller is
/// already elevated); the program's window is hidden.
///
/// `program` must be an absolute path, so the elevated process never comes
/// from a search. The arguments are quoted the way the C runtime splits a
/// command line. Blocks until the program exits.
pub fn run_elevated(program: &Path, args: &[&OsStr]) -> Result<u32, ElevateError> {
    if !program.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "the program to run as administrator needs an absolute path",
        )
        .into());
    }
    let parameters = command_line(args)?;
    if !crate::cert::interactive_desktop()? {
        return Err(ElevateError::NoDesktop);
    }
    let _com = Com::init();
    let verb = wide(OsStr::new("runas"))?;
    let file = wide(program.as_os_str())?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOCLOSEPROCESS: hand back the process handle to wait on. NOASYNC:
        // the call returns once the process exists. NO_UI: errors come back
        // to the caller instead of as a dialog box; the UAC prompt still shows.
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        lpParameters: parameters.as_ptr(),
        nShow: SW_HIDE,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized SHELLEXECUTEINFOW whose strings are
    // NUL-terminated UTF-16 that outlives the call; the null owner window and
    // directory are allowed.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(ERROR_CANCELLED.cast_signed()) {
            return Err(ElevateError::Cancelled);
        }
        return Err(error.into());
    }
    // An executable always starts a process, so the handle is set.
    let process = owned(info.hProcess)?;
    // SAFETY: `process` is a process handle with SYNCHRONIZE access.
    if unsafe { WaitForSingleObject(raw(&process), INFINITE) } != WAIT_OBJECT_0 {
        return Err(io::Error::last_os_error().into());
    }
    let mut code = 0_u32;
    // SAFETY: `process` is open with query access; `code` is writable.
    if unsafe { GetExitCodeProcess(raw(&process), &raw mut code) } == 0 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(code)
}

/// `args` as one NUL-terminated command line for the program to split back
/// the way the C runtime (`CommandLineToArgvW`) does.
fn command_line(args: &[&OsStr]) -> io::Result<Vec<u16>> {
    let mut line: Vec<u16> = Vec::new();
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            line.push(u16::from(b' '));
        }
        push_quoted(&mut line, arg)?;
    }
    line.push(0);
    Ok(line)
}

/// Appends `arg`, quoted when it is empty or holds whitespace or quotes:
/// backslashes are doubled only before a quote, and quotes are escaped.
fn push_quoted(line: &mut Vec<u16>, arg: &OsStr) -> io::Result<()> {
    let units: Vec<u16> = arg.encode_wide().collect();
    if units.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "argument contains a NUL character",
        ));
    }
    let special = |unit: &u16| b" \t\n\x0b\"".map(u16::from).contains(unit);
    if !units.is_empty() && !units.iter().any(special) {
        line.extend(&units);
        return Ok(());
    }
    let (quote, backslash) = (u16::from(b'"'), u16::from(b'\\'));
    line.push(quote);
    let mut backslashes = 0;
    for unit in units {
        if unit == backslash {
            backslashes += 1;
            continue;
        }
        let repeat = if unit == quote {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        line.extend(std::iter::repeat_n(backslash, repeat));
        line.push(unit);
        backslashes = 0;
    }
    line.extend(std::iter::repeat_n(backslash, backslashes * 2));
    line.push(quote);
    Ok(())
}

/// Opens an `http` or `https` URL with the default browser, the way a click
/// in Explorer does. The URL goes to the shell as one string, so no command
/// line is parsed (unlike `cmd /c start`, where `&` splits commands).
///
/// Refused with [`io::ErrorKind::InvalidInput`]: other schemes and plain
/// paths, which the `open` verb would run or open as files, and URLs with
/// quotes, whitespace, control characters or non-ASCII text, which a
/// registered handler would receive pasted into its command-line template.
pub fn open_url(url: &str) -> io::Result<()> {
    check_url(url)?;
    let _com = Com::init();
    let verb = wide(OsStr::new("open"))?;
    let file = wide(OsStr::new(url))?;
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        // NOASYNC: return only once the browser was started, since the calling
        // thread may end right after. NO_UI: errors come back to the caller
        // instead of as a dialog box.
        fMask: SEE_MASK_NOASYNC | SEE_MASK_FLAG_NO_UI,
        lpVerb: verb.as_ptr(),
        lpFile: file.as_ptr(),
        nShow: SW_SHOWNORMAL,
        ..Default::default()
    };
    // SAFETY: `info` is a correctly sized SHELLEXECUTEINFOW whose strings are
    // NUL-terminated UTF-16 that outlives the call; the null owner window,
    // parameters and directory are allowed, and no process handle is asked for.
    if unsafe { ShellExecuteExW(&raw mut info) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Accepts only `http` and `https` URLs made of printable ASCII other than `"`.
fn check_url(url: &str) -> io::Result<()> {
    let web = ["http://", "https://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
    });
    if !web {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http and https URLs are opened",
        ));
    }
    if !url
        .bytes()
        .all(|byte| byte.is_ascii_graphic() && byte != b'"')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "URLs with quotes, whitespace, control characters or non-ASCII text are not opened",
        ));
    }
    Ok(())
}

/// COM on the calling thread for the length of a shell call, which may hand
/// the URL to a COM-based handler. Uninitialises only what it initialised:
/// when the thread already uses another apartment (`RPC_E_CHANGED_MODE`) the
/// shell call runs without it and nothing is undone.
struct Com(bool);

impl Com {
    /// Joins a single-threaded apartment without OLE1 DDE, as
    /// `ShellExecuteEx` documents.
    fn init() -> Self {
        // SAFETY: plain call with a null reserved pointer.
        let result = unsafe {
            CoInitializeEx(
                ptr::null(),
                (COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) as u32,
            )
        };
        // S_OK and S_FALSE both need a matching CoUninitialize; failures do not.
        Self(result >= 0)
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: balances the successful CoInitializeEx in `init`, on the
            // same thread, since the guard never leaves one shell call.
            unsafe { CoUninitialize() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_web_urls_are_accepted() {
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

    /// The command line splits back into the same arguments.
    #[test]
    fn arguments_are_quoted_for_the_c_runtime() {
        let line = |args: &[&str]| {
            let args: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
            let mut units = command_line(&args).expect("line");
            units.pop();
            String::from_utf16(&units).expect("utf-16")
        };
        assert_eq!(
            line(&["import", r"C:\Users\me\.devlanes\nrpt.reg"]),
            r"import C:\Users\me\.devlanes\nrpt.reg"
        );
        assert_eq!(
            line(&["import", r"C:\Users\Jo Doe\.devlanes\nrpt.reg"]),
            r#"import "C:\Users\Jo Doe\.devlanes\nrpt.reg""#
        );
        assert_eq!(line(&[""]), r#""""#);
        assert_eq!(line(&[r"a b\"]), r#""a b\\""#);
        assert_eq!(line(&[r#"say "hi""#]), r#""say \"hi\"""#);
        assert_eq!(line(&[r#"x\"y z"#]), r#""x\\\"y z""#);
        assert!(command_line(&[OsStr::new("a\0b")]).is_err());
    }

    /// A relative program is refused before any prompt could appear.
    #[test]
    fn elevated_programs_need_absolute_paths() {
        let error = run_elevated(Path::new("reg.exe"), &[]).expect_err("relative");
        assert!(
            matches!(&error, ElevateError::Io(io) if io.kind() == io::ErrorKind::InvalidInput),
            "{error}"
        );
    }
}

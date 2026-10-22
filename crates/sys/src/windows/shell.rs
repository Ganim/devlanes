//! Opening web URLs with the user's default browser.

use std::ffi::OsStr;
use std::io;
use std::ptr;

use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

use super::security::wide;

/// `ShellExecuteW` results at or below this value are error codes.
const SHELL_EXECUTE_ERROR_MAX: isize = 32;
/// `SE_ERR_NOASSOC`: no program is registered for the URL scheme.
const SE_ERR_NOASSOC: isize = 31;

/// Opens an `http` or `https` URL with the default browser, the way a click in
/// Explorer does. The URL goes to the shell as one string, so no command line is
/// parsed (unlike `cmd /c start`, where `&` and quotes in URLs split commands).
///
/// Other schemes and plain paths are refused with [`io::ErrorKind::InvalidInput`]:
/// the shell's `open` verb would also run a program or open a file named by them.
pub fn open_url(url: &str) -> io::Result<()> {
    let web = ["http://", "https://"].iter().any(|scheme| {
        url.get(..scheme.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(scheme))
    });
    if !web || url.contains('\0') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "only http and https URLs can be opened",
        ));
    }
    let verb = wide(OsStr::new("open"));
    let file = wide(OsStr::new(url));
    // SAFETY: `verb` and `file` are NUL-terminated UTF-16 strings that outlive the
    // call; a null owner window, parameters and directory are documented as allowed.
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    match result as isize {
        code if code > SHELL_EXECUTE_ERROR_MAX => Ok(()),
        SE_ERR_NOASSOC => Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no program is registered to open web URLs",
        )),
        code => Err(io::Error::other(format!(
            "the shell could not open the URL (ShellExecuteW returned {code})"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_web_urls_reach_the_shell() {
        for refused in [
            "C:\\Windows\\System32\\calc.exe",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:",
            "",
            "http://example.invalid/\0",
        ] {
            let error = open_url(refused).expect_err(refused);
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{refused:?}");
        }
    }
}

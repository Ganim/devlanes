//! Writes rendered results; the renderers themselves live in the cli crate.

use std::{io::Write, process::ExitCode};

use devlanes_cli::Rendered;

/// Data on stdout, diagnostics on stderr, never interleaved within one rendering.
pub fn emit(rendered: &Rendered) {
    if !rendered.stdout.is_empty() {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(rendered.stdout.as_bytes());
        let _ = stdout.flush();
    }
    if !rendered.stderr.is_empty() {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(rendered.stderr.as_bytes());
    }
}

/// Whether stdout and stderr render escape sequences (colors, cursor moves).
/// Unix terminals always do.
#[cfg(unix)]
pub fn enable_escapes() -> bool {
    true
}

/// Turns on VT processing for stdout and stderr where they are a console, so
/// colors render in the classic console host too; false when a console refuses
/// it (hosts older than Windows 10). The mode stays on at exit, as shells leave it.
#[cfg(windows)]
pub fn enable_escapes() -> bool {
    use devlanes_sys::windows::console;
    fn enable(handle: impl std::os::windows::io::AsHandle) -> bool {
        match console::mode(&handle) {
            Ok(mode) => {
                console::set_mode(&handle, mode | console::VIRTUAL_TERMINAL_PROCESSING).is_ok()
            }
            // Files, pipes and mintty are not consoles; there is no mode to set.
            Err(_) => true,
        }
    }
    enable(std::io::stdout()) & enable(std::io::stderr())
}

/// Failures before a runtime exists use the plain error shape of cli-design 3.6.
pub fn local_error(message: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "error: {message}");
    ExitCode::from(code)
}

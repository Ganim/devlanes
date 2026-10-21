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

/// Failures before a runtime exists use the plain error shape of cli-design 3.6.
pub fn local_error(message: &str, code: u8) -> ExitCode {
    let _ = writeln!(std::io::stderr(), "error: {message}");
    ExitCode::from(code)
}

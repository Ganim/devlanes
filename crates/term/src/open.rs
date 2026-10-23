//! Opening a URL in the Windows default browser, for clients on Windows and
//! inside WSL (`devlanes url open` and the TUI). The browser is always started
//! by the client, never the daemon, which may run on a remote or headless
//! machine. Elsewhere the clients run the desktop's opener (`open`,
//! `xdg-open`) themselves.

/// Opening failures, shown by `devlanes url open` and the TUI status line.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error("{program} {}", ended(*.code))]
    Failed {
        program: String,
        /// `None` when a signal ended the opener.
        code: Option<i32>,
    },
}

/// How an opener ended, for [`OpenError::Failed`].
fn ended(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exited with status {code}"),
        None => "was killed by a signal".to_owned(),
    }
}

/// Opens `url` with the default browser through the shell (`ShellExecuteExW`),
/// which takes the URL as one string, so `&` is never a command separator.
/// Always `Some` on Windows. Blocking.
#[cfg(windows)]
pub fn open_in_windows_browser(url: &str) -> Option<Result<(), OpenError>> {
    Some(
        devlanes_sys::windows::shell::open_url(url).map_err(|source| OpenError::Spawn {
            program: "the default browser".into(),
            source,
        }),
    )
}

/// Inside WSL with interop, opens `url` with the Windows default browser
/// through `rundll32.exe url.dll,FileProtocolHandler` from Windows' System32,
/// and waits for it to hand the URL over. `None` outside WSL, without interop
/// or without a System32 mount: the desktop's own opener applies then.
/// Blocking.
///
/// WSL quotes each argument for the Windows command line only when it has
/// spaces or quotes, and `rundll32` hands everything after the entry point to
/// `FileProtocolHandler` without a shell, so `&` and `?` in a URL arrive
/// unchanged. It is a GUI program that writes nothing, so it gets no streams.
#[cfg(target_os = "linux")]
pub fn open_in_windows_browser(url: &str) -> Option<Result<(), OpenError>> {
    use std::process::{Command, Stdio};
    let wsl = devlanes_sys::wsl().filter(|wsl| wsl.interop)?;
    let program = wsl.system32.as_ref()?.join("rundll32.exe");
    let status = Command::new(&program)
        .args(["url.dll,FileProtocolHandler", url])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    Some(match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(OpenError::Failed {
            program: "rundll32.exe".into(),
            code: status.code(),
        }),
        Err(source) => Err(OpenError::Spawn {
            program: program.display().to_string(),
            source,
        }),
    })
}

/// Never: the browser is a Windows one only on Windows and inside WSL.
#[cfg(all(unix, not(target_os = "linux")))]
pub fn open_in_windows_browser(_url: &str) -> Option<Result<(), OpenError>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A failure names the exit status as a number, or says that a signal
    /// ended the opener.
    #[test]
    fn failures_name_the_status_or_the_signal() {
        let failed = |code| OpenError::Failed {
            program: "rundll32.exe".into(),
            code,
        };
        assert_eq!(
            failed(Some(3)).to_string(),
            "rundll32.exe exited with status 3"
        );
        assert_eq!(
            failed(None).to_string(),
            "rundll32.exe was killed by a signal"
        );
    }
}

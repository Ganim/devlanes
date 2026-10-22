//! Opening a URL in the user's browser, shared by the CLI (`devlanes url open`)
//! and the TUI. The browser is always started by the client, never the daemon,
//! which may run on a remote or headless machine.

#[cfg(any(unix, test))]
use std::process::{Command, Stdio};

#[cfg(any(unix, test))]
use devlanes_sys::NoWindow;

/// Opening failures, shown by `devlanes url open` and the TUI status line.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("nothing to run")]
    EmptyCommand,
    #[error("could not start {program}: {source}")]
    Spawn {
        program: String,
        source: std::io::Error,
    },
    #[error(
        "{program} exited with status {code:?}{}",
        .detail.as_deref().map(|detail| format!(": {detail}")).unwrap_or_default()
    )]
    Failed {
        program: String,
        code: Option<i32>,
        /// The last line the opener wrote to stderr, when it was captured.
        detail: Option<String>,
    },
}

/// Where an opener's output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The opener shares the caller's terminal, as for `devlanes url open`: a
    /// text browser can draw there and the opener's own messages show.
    Terminal,
    /// Nothing reaches the terminal, as for the TUI, which a child writing to
    /// it would corrupt. An opener that is waited for has its stderr captured
    /// for [`LaunchError::Failed`].
    Hidden,
}

/// A planned opener process, kept as data so the WSL fallback order stays
/// testable. Windows opens URLs through the shell instead, so there it only
/// exists for the tests.
#[cfg(any(unix, test))]
#[derive(Debug, Clone, PartialEq, Eq)]
struct Invocation {
    program: String,
    args: Vec<String>,
    /// True for openers that exit once they handed the URL over; false for a
    /// program that may live as long as its window.
    wait: bool,
}

#[cfg(any(unix, test))]
impl Invocation {
    /// Builds an invocation from string-like parts.
    fn new(program: impl Into<String>, args: Vec<String>, wait: bool) -> Self {
        Self {
            program: program.into(),
            args,
            wait,
        }
    }

    /// Runs the opener with its output going where `output` says.
    fn run_with(&self, output: Output) -> Result<(), LaunchError> {
        let mut command = Command::new(&self.program);
        command.no_window().args(&self.args);
        match output {
            // Set explicitly: `Command::output` would capture unset streams.
            Output::Terminal => command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit()),
            Output::Hidden => {
                command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(if self.wait {
                        Stdio::piped()
                    } else {
                        Stdio::null()
                    })
            }
        };
        let spawn_error = |source| LaunchError::Spawn {
            program: self.program.clone(),
            source,
        };
        if self.wait {
            // `output` reads the piped stderr while waiting; inherited and null
            // streams stay as set.
            let result = command.output().map_err(spawn_error)?;
            if !result.status.success() {
                return Err(LaunchError::Failed {
                    program: self.program.clone(),
                    code: result.status.code(),
                    detail: last_line(&result.stderr),
                });
            }
            return Ok(());
        }
        let mut child = command.spawn().map_err(spawn_error)?;
        // The program may outlive the caller's interest; a waiter thread reaps
        // it so long TUI sessions do not collect zombies.
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

/// The last non-empty line of an opener's stderr, trimmed.
#[cfg(any(unix, test))]
fn last_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_owned)
}

/// True when `program` is an executable file in a `PATH` directory. Only the
/// WSL fallback order needs it.
#[cfg(target_os = "linux")]
fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(program))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// Opens `url` in the default browser with `open` and waits for it to hand the
/// URL over; a non-zero exit is [`LaunchError::Failed`]. Blocking, so async
/// callers use `spawn_blocking`.
#[cfg(target_os = "macos")]
pub fn open_url(url: &str, output: Output) -> Result<(), LaunchError> {
    Invocation::new("open", vec![url.to_owned()], true).run_with(output)
}

/// Opens `url` in the default browser; blocking. Outside WSL this runs
/// `xdg-open`. Inside WSL with interop the Windows browser is the one to use,
/// so it tries `wslview`, then Windows' `rundll32.exe url.dll,FileProtocolHandler`,
/// then `xdg-open`, until one succeeds. No opener runs through a shell. Only
/// the last opener gets `output`; the ones before it stay hidden, so a
/// fallback that works is not preceded by the failed one's messages.
#[cfg(target_os = "linux")]
pub fn open_url(url: &str, output: Output) -> Result<(), LaunchError> {
    let openers = match devlanes_sys::wsl() {
        Some(info) if info.interop => wsl_openers(url, info.system32.as_deref(), on_path(WSLVIEW)),
        _ => vec![xdg_open(url)],
    };
    let mut failure = LaunchError::EmptyCommand;
    let last = openers.len().saturating_sub(1);
    for (index, opener) in openers.into_iter().enumerate() {
        let output = if index == last {
            output
        } else {
            Output::Hidden
        };
        match opener.run_with(output) {
            Ok(()) => return Ok(()),
            Err(error) => failure = error,
        }
    }
    Err(failure)
}

/// Opens `url` in the default browser through the shell (`ShellExecuteExW`),
/// which takes the URL as one string, so `&` is never a command separator.
/// No helper process writes to the terminal, so `output` changes nothing.
/// Blocking.
#[cfg(windows)]
pub fn open_url(url: &str, _output: Output) -> Result<(), LaunchError> {
    devlanes_sys::windows::shell::open_url(url).map_err(|source| LaunchError::Spawn {
        program: "the default browser".into(),
        source,
    })
}

/// Opens `url` with the freedesktop opener, as Linux does outside WSL. Blocking.
#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub fn open_url(url: &str, output: Output) -> Result<(), LaunchError> {
    Invocation::new("xdg-open", vec![url.to_owned()], true).run_with(output)
}

/// wslu's opener, which hands the URL to the Windows default browser.
#[cfg(any(target_os = "linux", test))]
const WSLVIEW: &str = "wslview";

/// The freedesktop opener.
#[cfg(any(target_os = "linux", test))]
fn xdg_open(url: &str) -> Invocation {
    Invocation::new("xdg-open", vec![url.to_owned()], true)
}

/// The openers to try inside WSL, in order: `wslview` when on `PATH`, then
/// `rundll32.exe url.dll,FileProtocolHandler <url>` from `system32`, then
/// `xdg-open`, which a distro may route to Windows itself.
///
/// wslu is archived and its older releases fail where WSL names its binfmt
/// entry `WSLInterop-late`, so `rundll32` stays as the fallback. WSL quotes each
/// argument for the Windows command line only when it has spaces or quotes
/// (checked against `GetCommandLineW`), and `rundll32` hands everything after
/// the entry point to `FileProtocolHandler` without a shell, so `&` and `?` in
/// a URL arrive unchanged.
#[cfg(any(target_os = "linux", test))]
fn wsl_openers(url: &str, system32: Option<&std::path::Path>, wslview: bool) -> Vec<Invocation> {
    let mut openers = Vec::new();
    if wslview {
        openers.push(Invocation::new(WSLVIEW, vec![url.to_owned()], true));
    }
    if let Some(system32) = system32 {
        openers.push(Invocation::new(
            system32.join("rundll32.exe").to_string_lossy(),
            vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
            true,
        ));
    }
    openers.push(xdg_open(url));
    openers
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    const URL: &str = "https://web.acme-2.localhost:8443/search?q=a&page=2";

    /// The argv of each planned opener.
    fn argvs(openers: &[Invocation]) -> Vec<Vec<String>> {
        openers
            .iter()
            .map(|opener| {
                std::iter::once(opener.program.clone())
                    .chain(opener.args.iter().cloned())
                    .collect()
            })
            .collect()
    }

    /// A hidden opener that fails reports its last stderr line; one sharing the
    /// terminal reports only its status.
    #[test]
    fn failed_openers_report_the_last_stderr_line_when_hidden() {
        #[cfg(unix)]
        let failing = Invocation::new(
            "sh",
            vec![
                "-c".into(),
                "echo first >&2; echo 'no browser found' >&2; echo >&2; exit 3".into(),
            ],
            true,
        );
        #[cfg(windows)]
        let failing = Invocation::new(
            "cmd.exe",
            vec![
                "/d".into(),
                "/c".into(),
                "echo first 1>&2 & echo no browser found 1>&2 & exit /b 3".into(),
            ],
            true,
        );
        match failing.run_with(Output::Hidden) {
            Err(LaunchError::Failed { code, detail, .. }) => {
                assert_eq!(code, Some(3));
                assert_eq!(detail.as_deref(), Some("no browser found"));
            }
            other => panic!("expected a failure, got {other:?}"),
        }
        #[cfg(unix)]
        let quiet = Invocation::new("sh", vec!["-c".into(), "exit 4".into()], true);
        #[cfg(windows)]
        let quiet = Invocation::new(
            "cmd.exe",
            vec!["/d".into(), "/c".into(), "exit /b 4".into()],
            true,
        );
        let error = quiet.run_with(Output::Terminal).expect_err("fails");
        assert!(
            matches!(
                &error,
                LaunchError::Failed {
                    code: Some(4),
                    detail: None,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(last_line(b"one\n  two  \n\n"), Some("two".into()));
        assert_eq!(last_line(b""), None);
    }

    /// wslview first when installed, then rundll32 from System32, then xdg-open;
    /// the URL is one argument, `&` included, and every opener is waited for.
    #[test]
    fn wsl_openers_prefer_wslview_then_rundll32() {
        let system32 = Path::new("/mnt/c/Windows/System32");
        let rundll32 = system32.join("rundll32.exe").to_string_lossy().into_owned();
        let openers = wsl_openers(URL, Some(system32), true);
        assert_eq!(
            argvs(&openers),
            [
                vec!["wslview", URL],
                vec![rundll32.as_str(), "url.dll,FileProtocolHandler", URL],
                vec!["xdg-open", URL],
            ]
        );
        assert!(openers.iter().all(|opener| opener.wait));
    }

    /// Without wslview or a System32 mount, only what exists is planned.
    #[test]
    fn wsl_openers_skip_what_is_missing() {
        let rundll32 = Path::new("/mnt/c/Windows/System32").join("rundll32.exe");
        assert_eq!(
            argvs(&wsl_openers(
                URL,
                Some(Path::new("/mnt/c/Windows/System32")),
                false
            )),
            [
                vec![
                    rundll32.to_string_lossy().into_owned(),
                    "url.dll,FileProtocolHandler".into(),
                    URL.into()
                ],
                vec!["xdg-open".into(), URL.into()],
            ]
        );
        assert_eq!(
            argvs(&wsl_openers(URL, None, false)),
            [vec!["xdg-open", URL]]
        );
    }
}

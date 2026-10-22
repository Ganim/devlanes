//! Terminal integrations: opening a unit in the user's terminal application,
//! the terminal's own progress bar ([`progress`]), and opening URLs in the
//! browser ([`open_url`]).
//!
//! Each terminal integration lives in its own `term-*` crate and implements
//! [`TerminalLauncher`]; only the composition root decides which ones exist.

mod open;
pub mod progress;

pub use open::open_url;

pub use progress::{
    PROGRESS_ENV, Progress, ProgressBar, ProgressMode, StartTracker, Wrap, phase_detail,
    progress_support, progress_support_from_env,
};

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

use devlanes_sys::NoWindow;

/// What the new terminal window or tab should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// Working directory of the new shell or command.
    pub cwd: PathBuf,
    /// Program and arguments; never interpreted by a shell before quoting.
    pub command: Vec<String>,
    /// Window or tab title where the terminal supports one.
    pub title: Option<String>,
    /// Variables `command` needs on top of the caller's environment. Only
    /// launchers whose terminal hands its caller's environment to the command
    /// (Windows Terminal, WezTerm outside macOS) pass them on; terminals
    /// started through `open` or `osascript` get launchd's environment, so
    /// callers there put variables in `command` instead.
    pub env: Vec<(String, String)>,
}

/// A planned helper process, kept as data so launchers stay testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    /// True for helpers that exit quickly (`osascript`, `open`); false for
    /// terminal binaries that may live as long as the window.
    pub wait: bool,
    /// Variables set on the helper process on top of the caller's environment.
    pub env: Vec<(String, String)>,
}

/// Launch failures are shown in the TUI status line.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("{terminal} is not available on this platform")]
    Unsupported { terminal: &'static str },
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
        /// The last line the helper wrote to stderr, when it was captured.
        detail: Option<String>,
    },
}

/// Where a helper's output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Output {
    /// The helper shares the caller's terminal, as for `devlanes url open`: a
    /// text browser can draw there and the opener's own messages show.
    Terminal,
    /// Nothing reaches the terminal, as for the TUI, which a child writing to
    /// it would corrupt. A helper that is waited for has its stderr captured
    /// for [`LaunchError::Failed`].
    Hidden,
}

/// Opens a new terminal window or tab at a directory running a command.
///
/// `launch` blocks for at most as long as a short helper such as `osascript`
/// runs, so async callers should use `spawn_blocking`.
pub trait TerminalLauncher: Send + Sync {
    /// The user config value that selects this launcher (`terminal: ghostty`).
    fn name(&self) -> &'static str;

    /// Plans the helper process without running it.
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError>;

    /// Runs the planned helper process.
    fn launch(&self, request: &LaunchRequest) -> Result<(), LaunchError> {
        self.invocation(request)?.run()
    }

    /// Whether the terminal is installed here; used to pick a default when
    /// the user configured none. Checks the filesystem, so call it off async
    /// executor threads.
    fn available(&self) -> bool {
        true
    }
}

/// True when `program` is an executable file in a `PATH` directory.
#[cfg(unix)]
pub fn on_path(program: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| {
        std::fs::metadata(dir.join(program))
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    })
}

/// True when `Command::new(program)` would find `program` in a `PATH` directory:
/// the name as given when it has an extension, otherwise with `.exe` appended.
/// `PATHEXT` is ignored because std only ever appends `.exe`, so a `.cmd` or
/// `.bat` launcher counts only when `program` names that extension.
#[cfg(windows)]
pub fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| windows_path::in_dir(&dir, program))
}

#[cfg(windows)]
mod windows_path {
    use std::path::Path;

    /// Whether `dir` holds the file `Command::new(program)` looks for there.
    /// Like std, any dot counts as an extension.
    pub(crate) fn in_dir(dir: &Path, program: &str) -> bool {
        if program.contains('.') {
            present(&dir.join(program))
        } else {
            present(&dir.join(format!("{program}.exe")))
        }
    }

    /// Whether `candidate` is a file. App Execution Aliases such as `wt.exe` in
    /// `%LOCALAPPDATA%\Microsoft\WindowsApps` are reparse points `metadata`
    /// may fail to follow, so the link itself existing is enough.
    fn present(candidate: &Path) -> bool {
        match std::fs::metadata(candidate) {
            Ok(meta) => meta.is_file(),
            Err(_) => std::fs::symlink_metadata(candidate).is_ok_and(|meta| !meta.is_dir()),
        }
    }
}

/// True when a macOS app bundle such as `kitty.app` is in `/Applications` or
/// `~/Applications`. macOS only: elsewhere launchers look on `PATH`.
#[cfg(target_os = "macos")]
pub fn app_installed(bundle: &str) -> bool {
    let mut dirs = vec![PathBuf::from("/Applications")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(home).join("Applications"));
    }
    dirs.iter().any(|dir| dir.join(bundle).is_dir())
}

impl Invocation {
    /// Builds an invocation from string-like parts.
    pub fn new(program: impl Into<String>, args: Vec<String>, wait: bool) -> Self {
        Self {
            program: program.into(),
            args,
            wait,
            env: Vec::new(),
        }
    }

    /// Sets `env` on the helper process, for terminals that pass their
    /// caller's environment on to the command they run.
    pub fn with_env(mut self, env: &[(String, String)]) -> Self {
        self.env.extend_from_slice(env);
        self
    }

    /// Runs the helper with no inherited stdio, so it cannot draw over the TUI.
    pub fn run(&self) -> Result<(), LaunchError> {
        self.run_with(Output::Hidden)
    }

    /// Runs the helper with its output going where `output` says.
    pub fn run_with(&self, output: Output) -> Result<(), LaunchError> {
        let mut command = Command::new(&self.program);
        // Windows Terminal (`wt.exe`) and `wezterm-gui` are GUI programs, which
        // the flag does not affect; it only keeps a console launcher such as
        // `wezterm.exe` from flashing a console next to the terminal it opens.
        command
            .no_window()
            .args(&self.args)
            .envs(self.env.iter().map(|(key, value)| (key, value)));
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
        // The terminal may outlive the caller's interest; a waiter thread reaps it
        // so long TUI sessions do not collect zombies.
        std::thread::spawn(move || child.wait());
        Ok(())
    }
}

/// The last non-empty line of a helper's stderr, trimmed.
fn last_line(stderr: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_owned)
}

impl LaunchRequest {
    /// Fails early when a launcher would otherwise open an empty shell.
    pub fn require_command(&self) -> Result<&[String], LaunchError> {
        if self.command.is_empty() {
            return Err(LaunchError::EmptyCommand);
        }
        Ok(&self.command)
    }

    /// The line typed into terminals that only accept shell text:
    /// `sh -c 'cd <cwd> && exec <command>'`. The user's login shell may be
    /// fish or nushell, so it only has to run `sh` with one quoted word.
    pub fn shell_line(&self) -> Result<String, LaunchError> {
        let command = self.require_command()?;
        let script = format!(
            "cd {} && exec {}",
            shell_quote(&self.cwd.to_string_lossy()),
            shell_join(command)
        );
        Ok(format!("sh -c {}", shell_quote(&script)))
    }

    /// The working directory as a string argument.
    pub fn cwd_arg(&self) -> String {
        self.cwd.to_string_lossy().into_owned()
    }
}

/// Quotes one word for POSIX shells.
pub fn shell_quote(word: &str) -> String {
    let safe = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./-_".contains(&b));
    if safe {
        return word.to_owned();
    }
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// Quotes and joins argv for POSIX shells.
pub fn shell_join(argv: &[String]) -> String {
    argv.iter()
        .map(|word| shell_quote(word))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Encodes text as an AppleScript string literal.
pub fn applescript_string(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', r"\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LaunchRequest {
        LaunchRequest {
            cwd: PathBuf::from("/tmp/my repo"),
            command: vec!["devlanes".into(), "attach".into(), "acme@2:api".into()],
            title: None,
            env: Vec::new(),
        }
    }

    #[test]
    fn shell_line_quotes_paths_and_keeps_safe_words() -> Result<(), LaunchError> {
        assert_eq!(
            request().shell_line()?,
            r#"sh -c 'cd '\''/tmp/my repo'\'' && exec devlanes attach acme@2:api'"#
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn shell_line_round_trips_through_sh() -> Result<(), Box<dyn std::error::Error>> {
        let request = LaunchRequest {
            cwd: std::env::temp_dir(),
            command: vec!["printf".into(), "%s|%s".into(), "it's".into(), "a b".into()],
            title: None,
            env: Vec::new(),
        };
        // The typed line is itself a shell command: run it the way a login shell would.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(request.shell_line()?)
            .stdout(Stdio::piped())
            .spawn()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while child.try_wait()?.is_none() {
            if std::time::Instant::now() > deadline {
                child.kill()?;
                child.wait()?;
                return Err("sh did not finish within 5 seconds".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let output = child.wait_with_output()?;
        assert_eq!(String::from_utf8(output.stdout)?, "it's|a b");
        Ok(())
    }

    /// A hidden helper that fails reports its last stderr line; one sharing the
    /// terminal reports only its status.
    #[test]
    fn failed_helpers_report_the_last_stderr_line_when_hidden() {
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

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote(""), "''");
    }

    #[test]
    fn applescript_string_escapes_quotes_and_backslashes() {
        assert_eq!(applescript_string(r#"a "b" \c"#), r#""a \"b\" \\c""#);
    }

    /// `cmd` resolves to `cmd.exe` from the real `PATH`; unknown names do not.
    #[cfg(windows)]
    #[test]
    fn on_path_finds_cmd_exe() {
        assert!(on_path("cmd"));
        assert!(on_path("CMD.EXE"));
        assert!(!on_path("devlanes-no-such-program"));
    }

    /// `wt` is an App Execution Alias in `WindowsApps`, which `metadata` may
    /// not follow.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs Windows Terminal"]
    fn on_path_finds_app_execution_aliases() {
        assert!(on_path("wt"));
    }

    /// Bare names match only `<name>.exe`, as `Command::new` resolves them;
    /// names with an extension match as given. Directories do not count.
    #[cfg(windows)]
    #[test]
    fn windows_lookup_matches_command_new() -> std::io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join("tool.EXE"), "")?;
        std::fs::write(dir.path().join("shim.cmd"), "")?;
        std::fs::write(dir.path().join("plain"), "")?;
        std::fs::create_dir(dir.path().join("folder.exe"))?;
        assert!(windows_path::in_dir(dir.path(), "tool"));
        assert!(windows_path::in_dir(dir.path(), "TOOL.exe"));
        assert!(windows_path::in_dir(dir.path(), "SHIM.cmd"));
        assert!(!windows_path::in_dir(dir.path(), "shim"));
        assert!(!windows_path::in_dir(dir.path(), "plain"));
        assert!(!windows_path::in_dir(dir.path(), "folder"));
        Ok(())
    }

    #[test]
    fn empty_command_is_rejected() {
        let request = LaunchRequest {
            command: Vec::new(),
            ..request()
        };
        assert!(matches!(
            request.shell_line(),
            Err(LaunchError::EmptyCommand)
        ));
    }
}

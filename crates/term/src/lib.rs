//! Opens a unit in the user's terminal application.
//!
//! Each terminal integration lives in its own `term-*` crate and implements
//! [`TerminalLauncher`]; only the composition root decides which ones exist.

use std::{
    path::PathBuf,
    process::{Command, Stdio},
};

/// What the new terminal window or tab should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchRequest {
    /// Working directory of the new shell or command.
    pub cwd: PathBuf,
    /// Program and arguments; never interpreted by a shell before quoting.
    pub command: Vec<String>,
    /// Window or tab title where the terminal supports one.
    pub title: Option<String>,
}

/// A planned helper process, kept as data so launchers stay testable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    pub program: String,
    pub args: Vec<String>,
    /// True for helpers that exit quickly (`osascript`, `open`); false for
    /// terminal binaries that may live as long as the window.
    pub wait: bool,
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
    #[error("{program} exited with status {code:?}")]
    Failed { program: String, code: Option<i32> },
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
}

impl Invocation {
    /// Builds an invocation from string-like parts.
    pub fn new(program: impl Into<String>, args: Vec<String>, wait: bool) -> Self {
        Self {
            program: program.into(),
            args,
            wait,
        }
    }

    /// Runs the helper with no inherited stdio, so it cannot draw over the TUI.
    pub fn run(&self) -> Result<(), LaunchError> {
        let mut command = Command::new(&self.program);
        command
            .args(&self.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let spawn_error = |source| LaunchError::Spawn {
            program: self.program.clone(),
            source,
        };
        if self.wait {
            let status = command.status().map_err(spawn_error)?;
            if !status.success() {
                return Err(LaunchError::Failed {
                    program: self.program.clone(),
                    code: status.code(),
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

impl LaunchRequest {
    /// Fails early when a launcher would otherwise open an empty shell.
    pub fn require_command(&self) -> Result<&[String], LaunchError> {
        if self.command.is_empty() {
            return Err(LaunchError::EmptyCommand);
        }
        Ok(&self.command)
    }

    /// The `cd <cwd> && <command>` line typed into terminals that only accept shell text.
    pub fn shell_line(&self) -> Result<String, LaunchError> {
        let command = self.require_command()?;
        Ok(format!(
            "cd {} && {}",
            shell_quote(&self.cwd.to_string_lossy()),
            shell_join(command)
        ))
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
        }
    }

    #[test]
    fn shell_line_quotes_paths_and_keeps_safe_words() -> Result<(), LaunchError> {
        assert_eq!(
            request().shell_line()?,
            "cd '/tmp/my repo' && devlanes attach acme@2:api"
        );
        Ok(())
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

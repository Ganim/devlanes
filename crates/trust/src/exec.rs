use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use tempfile::TempPath;

use crate::{CaCertificate, TrustError};

/// A program and its arguments, built by a store and run by a [`CommandExecutor`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandLine {
    /// Program name or absolute path.
    pub program: String,
    /// Arguments, passed without a shell.
    pub args: Vec<String>,
}

impl CommandLine {
    /// Starts a command line for `program`.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
        }
    }

    /// Appends one argument.
    #[must_use]
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Appends several arguments.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Program followed by arguments, for assertions.
    pub fn argv(&self) -> Vec<&str> {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect()
    }
}

impl fmt::Display for CommandLine {
    /// Space-joined argv, quoting arguments that contain spaces, for error messages.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, part) in self.argv().into_iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            if part.is_empty() || part.contains(char::is_whitespace) {
                write!(f, "'{part}'")?;
            } else {
                f.write_str(part)?;
            }
        }
        Ok(())
    }
}

/// Captured result of a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit code; `None` when killed by a signal.
    pub code: Option<i32>,
    /// Standard output, lossily decoded.
    pub stdout: String,
    /// Standard error, lossily decoded.
    pub stderr: String,
}

impl CommandOutput {
    /// A successful output with `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// A failed output with `code` and `stderr`.
    pub fn failed(code: i32, stderr: impl Into<String>) -> Self {
        Self {
            code: Some(code),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Whether the command exited with 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    /// Turns a non-zero exit into [`TrustError::CommandFailed`].
    pub fn into_checked(self, command: &CommandLine) -> Result<Self, TrustError> {
        if self.success() {
            Ok(self)
        } else {
            Err(TrustError::CommandFailed {
                command: command.to_string(),
                code: self.code,
                stderr: self.stderr,
            })
        }
    }
}

/// Runs external commands for trust stores.
///
/// Tests inject a fake that records argv; production uses [`SystemExecutor`].
#[async_trait]
pub trait CommandExecutor: Send + Sync {
    /// Runs `command` to completion with null stdin and captured output.
    ///
    /// `sudo` and the macOS keychain prompt through `/dev/tty` and the GUI, so
    /// capturing output does not hide their prompts.
    async fn run(&self, command: &CommandLine) -> io::Result<CommandOutput>;

    /// Finds an executable named `name` in `extra_dirs`, then on `PATH`.
    ///
    /// Only for tools that run as the user (`certutil`); root commands use
    /// [`find_system_program`], which never reads `PATH`.
    fn find_program(&self, name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf>;

    /// Whether `path` is an executable regular file.
    fn is_executable(&self, path: &Path) -> bool;
}

/// Root-owned directories searched for privileged programs, in order.
#[cfg(unix)]
pub const SYSTEM_PROGRAM_DIRS: [&str; 4] = ["/usr/sbin", "/usr/bin", "/sbin", "/bin"];

/// Absolute path of `name` in [`SYSTEM_PROGRAM_DIRS`].
///
/// Commands run through sudo or as root must not come from the user's `PATH`: a
/// directory the user (or anything running as the user) can write would turn a
/// trust change into arbitrary code running as root.
#[cfg(unix)]
pub fn find_system_program(executor: &dyn CommandExecutor, name: &str) -> Option<PathBuf> {
    SYSTEM_PROGRAM_DIRS
        .iter()
        .map(|dir| Path::new(dir).join(name))
        .find(|candidate| executor.is_executable(candidate))
}

/// Absolute path of `name` (`netsh`, or `netsh.exe`) in System32, as a `.exe`
/// or `.com` file.
///
/// Like the Unix version this never reads `PATH`: elevated commands must not
/// come from a directory the user can write. Batch files are left out, since
/// they would run through `cmd.exe` with its own quoting rules.
#[cfg(windows)]
pub fn find_system_program(executor: &dyn CommandExecutor, name: &str) -> Option<PathBuf> {
    let directory = devlanes_sys_windows::file::system_directory().ok()?;
    system_program_in(executor, &directory, name)
}

/// [`find_system_program`] in `directory`.
#[cfg(windows)]
fn system_program_in(
    executor: &dyn CommandExecutor,
    directory: &Path,
    name: &str,
) -> Option<PathBuf> {
    const EXTENSIONS: [&str; 2] = ["exe", "com"];
    // A bare file name only: `..\x` or `C:x` would leave System32.
    if Path::new(name).file_name() != Some(std::ffi::OsStr::new(name)) {
        return None;
    }
    let candidates: Vec<PathBuf> = match Path::new(name).extension() {
        Some(extension)
            if EXTENSIONS
                .iter()
                .any(|known| extension.eq_ignore_ascii_case(known)) =>
        {
            vec![directory.join(name)]
        }
        Some(_) => Vec::new(),
        None => EXTENSIONS
            .iter()
            .map(|extension| directory.join(format!("{name}.{extension}")))
            .collect(),
    };
    candidates
        .into_iter()
        .find(|candidate| executor.is_executable(candidate))
}

/// Runs commands with `tokio::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemExecutor;

#[async_trait]
impl CommandExecutor for SystemExecutor {
    /// Spawns the program directly, without a shell.
    async fn run(&self, command: &CommandLine) -> io::Result<CommandOutput> {
        let output = tokio::process::Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .kill_on_drop(true)
            .output()
            .await?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    /// Looks for an executable regular file; a few `stat` calls, cheap enough inline.
    fn find_program(&self, name: &str, extra_dirs: &[PathBuf]) -> Option<PathBuf> {
        let path_dirs = std::env::var_os("PATH")
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default();
        extra_dirs
            .iter()
            .chain(&path_dirs)
            // Relative PATH entries would depend on the current directory.
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(name))
            .find(|candidate| is_executable(candidate))
    }

    /// Checks the real filesystem.
    fn is_executable(&self, path: &Path) -> bool {
        is_executable(path)
    }
}

/// Whether `path` is a regular file with an extension Windows runs directly.
#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    let runnable = path.extension().is_some_and(|extension| {
        ["exe", "com", "bat", "cmd"]
            .iter()
            .any(|known| extension.eq_ignore_ascii_case(known))
    });
    runnable && std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Whether `path` is a regular file with an execute bit.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// Runs `command`, mapping a missing program to [`TrustError::ToolMissing`].
///
/// Any exit code is returned as output; callers decide which failures are benign.
pub async fn run(
    executor: &dyn CommandExecutor,
    command: &CommandLine,
    hint: &str,
) -> Result<CommandOutput, TrustError> {
    match executor.run(command).await {
        Ok(output) => Ok(output),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(TrustError::ToolMissing {
            tool: command.program.clone(),
            hint: hint.into(),
        }),
        Err(error) => Err(TrustError::io(format!("running `{command}`"), error)),
    }
}

/// Like [`run`], but a non-zero exit is [`TrustError::CommandFailed`].
pub async fn run_checked(
    executor: &dyn CommandExecutor,
    command: &CommandLine,
    hint: &str,
) -> Result<CommandOutput, TrustError> {
    run(executor, command, hint).await?.into_checked(command)
}

/// Converts a path to a command argument; argv is UTF-8 so it can be shown and asserted.
pub fn path_arg(path: &Path) -> Result<String, TrustError> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        TrustError::io(
            format!("using {}", path.display()),
            io::Error::new(io::ErrorKind::InvalidInput, "path is not valid UTF-8"),
        )
    })
}

/// The CA written to a private temporary file, for tools that only read files.
///
/// Removed on [`TempPem::close`]; dropping it also removes it as a fallback on
/// early error returns.
#[derive(Debug)]
pub struct TempPem {
    path: TempPath,
    arg: String,
}

impl TempPem {
    /// Writes the CA's PEM on a blocking thread.
    pub async fn create(ca: &CaCertificate) -> Result<Self, TrustError> {
        let pem = ca.pem().to_owned();
        let path = tokio::task::spawn_blocking(move || -> io::Result<TempPath> {
            use std::io::Write as _;
            let mut file = tempfile::Builder::new()
                .prefix("devlanes-trust-")
                .suffix(".pem")
                .tempfile()?;
            file.write_all(pem.as_bytes())?;
            file.flush()?;
            Ok(file.into_temp_path())
        })
        .await
        .map_err(|error| TrustError::io("writing the CA to a temp file", io::Error::other(error)))?
        .map_err(|error| TrustError::io("writing the CA to a temp file", error))?;
        let arg = path_arg(&path)?;
        Ok(Self { path, arg })
    }

    /// Path as a command argument.
    pub fn arg(&self) -> &str {
        &self.arg
    }

    /// Removes the file on a blocking thread.
    pub async fn close(self) {
        let path = self.path;
        // A leftover public certificate in the temp dir is harmless, so failures are ignored.
        let _ = tokio::task::spawn_blocking(move || path.close()).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_quoted_argv() {
        let command = CommandLine::new("certutil").args(["-n", "devlanes CA", ""]);
        assert_eq!(command.to_string(), "certutil -n 'devlanes CA' ''");
        assert_eq!(command.argv(), ["certutil", "-n", "devlanes CA", ""]);
    }

    #[cfg(unix)]
    #[test]
    fn system_programs_never_come_from_path() {
        let fake = crate::fake::FakeExecutor::new(|_| Ok(CommandOutput::ok("")))
            .with_programs(&["sudo", "/bin/rm"]);
        assert_eq!(find_system_program(&fake, "sudo"), None, "bare PATH entry");
        assert_eq!(
            find_system_program(&fake, "rm"),
            Some(PathBuf::from("/bin/rm"))
        );
        let sh = find_system_program(&SystemExecutor, "sh").expect("sh");
        assert!(
            SYSTEM_PROGRAM_DIRS
                .iter()
                .any(|dir| sh.parent() == Some(Path::new(dir))),
            "{}",
            sh.display()
        );
    }

    #[tokio::test]
    async fn missing_program_is_tool_missing() {
        let command = CommandLine::new("devlanes-definitely-missing-tool");
        let error = run(&SystemExecutor, &command, "install it")
            .await
            .expect_err("missing program");
        assert!(
            matches!(error, TrustError::ToolMissing { ref tool, .. } if tool == "devlanes-definitely-missing-tool")
        );
    }

    /// System32 programs resolve to `.exe` or `.com` files there, never to a
    /// batch file, another extension or a path that leaves the directory.
    #[cfg(windows)]
    #[test]
    fn system_programs_come_from_system32() {
        let dir = Path::new(r"C:\Windows\System32");
        let fake = crate::fake::FakeExecutor::new(|_| Ok(CommandOutput::ok(""))).with_programs(&[
            r"C:\Windows\System32\netsh.exe",
            r"C:\Windows\System32\more.com",
            r"C:\Windows\System32\tool.bat",
            r"C:\Windows\tool.exe",
            "certutil",
        ]);
        let found = |name| system_program_in(&fake, dir, name);
        assert_eq!(found("netsh"), Some(dir.join("netsh.exe")));
        assert_eq!(found("netsh.exe"), Some(dir.join("netsh.exe")));
        assert_eq!(found("more"), Some(dir.join("more.com")));
        assert_eq!(found("tool"), None, "batch files are not programs here");
        assert_eq!(found("tool.bat"), None);
        assert_eq!(found(r"..\tool"), None);
        assert_eq!(found("certutil"), None, "bare PATH entry");
        let real = find_system_program(&SystemExecutor, "netsh").expect("netsh");
        assert_eq!(
            real.parent(),
            Some(
                devlanes_sys_windows::file::system_directory()
                    .expect("System32")
                    .as_path()
            )
        );
    }

    /// Exit code, stdout and stderr come back separately.
    #[cfg(windows)]
    #[tokio::test]
    async fn system_executor_captures_output() {
        let cmd = find_system_program(&SystemExecutor, "cmd").expect("cmd");
        let command = CommandLine::new(cmd.display().to_string()).args([
            "/d",
            "/c",
            "echo out&& echo err 1>&2&& exit 3",
        ]);
        let output = SystemExecutor.run(&command).await.expect("run cmd");
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout, "out\r\n");
        assert_eq!(output.stderr, "err \r\n");
        assert!(SystemExecutor.find_program("cmd.exe", &[]).is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn system_executor_captures_output() {
        let command = CommandLine::new("sh").args(["-c", "echo out; echo err >&2; exit 3"]);
        let output = SystemExecutor.run(&command).await.expect("run sh");
        assert_eq!(output.code, Some(3));
        assert_eq!(output.stdout, "out\n");
        assert_eq!(output.stderr, "err\n");
        assert!(SystemExecutor.find_program("sh", &[]).is_some());
    }
}

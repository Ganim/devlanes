//! Command execution behind a trait, so tests check argv and stdin instead of running sudo.

use std::fmt;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt as _;

/// A program, its arguments and optional stdin, run without a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Program name or absolute path.
    pub program: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Bytes written to stdin; `None` connects stdin to `/dev/null`.
    pub stdin: Option<Vec<u8>>,
}

impl CommandSpec {
    /// Starts a command for `program`.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            stdin: None,
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

    /// Sets stdin.
    #[must_use]
    pub fn stdin(mut self, bytes: Vec<u8>) -> Self {
        self.stdin = Some(bytes);
        self
    }

    /// Program followed by arguments, for assertions.
    pub fn argv(&self) -> Vec<&str> {
        std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect()
    }
}

impl fmt::Display for CommandSpec {
    /// Space-joined argv, quoting arguments with spaces, for messages.
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
    /// Exit 0 with `stdout`.
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    /// Exit `code` with `stderr`.
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
}

/// Runs commands for the hosts client and the installer.
#[async_trait]
pub trait Executor: Send + Sync {
    /// Runs `command` to completion and captures stdout and stderr.
    ///
    /// sudo prompts through `/dev/tty`, so capturing output does not hide its
    /// password prompt in the CLI.
    async fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput>;
}

/// Runs commands with `tokio::process`.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemExecutor;

#[async_trait]
impl Executor for SystemExecutor {
    /// Spawns the program directly; the child is killed if the future is dropped.
    async fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput> {
        let mut child = tokio::process::Command::new(&command.program)
            .args(&command.args)
            .stdin(if command.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let pipe = child.stdin.take();
        // Written concurrently with reading output so neither side can fill a pipe
        // and block the other.
        let write = async {
            if let (Some(bytes), Some(mut pipe)) = (&command.stdin, pipe) {
                // A child that exits without reading (sudo refusing) closes the pipe
                // early; its exit status explains the failure better than EPIPE does.
                match pipe.write_all(bytes).await {
                    Err(error) if error.kind() != io::ErrorKind::BrokenPipe => return Err(error),
                    _ => {}
                }
            }
            Ok(())
        };
        let (written, output) = tokio::join!(write, child.wait_with_output());
        let output = output?;
        written?;
        Ok(CommandOutput {
            code: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Kind of a filesystem entry, from `lstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// A directory.
    Dir,
    /// A regular file.
    File,
    /// A symlink (not followed).
    Symlink,
    /// Anything else.
    Other,
}

/// The parts of `lstat` the installer and client check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    /// Entry kind.
    pub kind: FileKind,
    /// Owner uid.
    pub uid: u32,
    /// Permission bits (`mode & 0o7777`).
    pub mode: u32,
}

/// Read-only filesystem checks, injectable so install checks run against fakes.
pub trait FsProbe: Send + Sync {
    /// `lstat(path)`; `Ok(None)` when it does not exist.
    fn lstat(&self, path: &Path) -> io::Result<Option<FileInfo>>;
}

/// Real `lstat`. Single stat calls are cheap enough to run inline, as the trust
/// stores do for `PATH` lookups.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemFs;

impl FsProbe for SystemFs {
    /// `std::fs::symlink_metadata`, mapping `NotFound` to `None`.
    fn lstat(&self, path: &Path) -> io::Result<Option<FileInfo>> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) => {
                let file_type = meta.file_type();
                let kind = if file_type.is_symlink() {
                    FileKind::Symlink
                } else if file_type.is_dir() {
                    FileKind::Dir
                } else if file_type.is_file() {
                    FileKind::File
                } else {
                    FileKind::Other
                };
                Ok(Some(FileInfo {
                    kind,
                    uid: meta.uid(),
                    mode: meta.mode() & 0o7777,
                }))
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn displays_quoted_argv() {
        let command = CommandSpec::new("sudo").args(["-n", "a b", ""]);
        assert_eq!(command.to_string(), "sudo -n 'a b' ''");
    }

    #[tokio::test]
    async fn system_executor_pipes_stdin() {
        let command = CommandSpec::new("sh")
            .args(["-c", "cat; echo err >&2; exit 3"])
            .stdin(b"names\n".to_vec());
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SystemExecutor.run(&command),
        )
        .await
        .expect("finishes")
        .expect("runs");
        assert_eq!(
            output,
            CommandOutput {
                code: Some(3),
                stdout: "names\n".into(),
                stderr: "err\n".into()
            }
        );
    }

    #[tokio::test]
    async fn early_exit_is_not_an_epipe_error() {
        let command = CommandSpec::new("sh")
            .args(["-c", "exit 1"])
            .stdin(vec![b'x'; 1 << 20]);
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            SystemExecutor.run(&command),
        )
        .await
        .expect("finishes")
        .expect("runs");
        assert_eq!(output.code, Some(1));
    }

    #[test]
    fn system_fs_reports_kinds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("f");
        std::fs::write(&file, b"x").expect("write");
        let link = dir.path().join("l");
        std::os::unix::fs::symlink(&file, &link).expect("symlink");
        assert_eq!(
            SystemFs.lstat(dir.path()).expect("dir").map(|i| i.kind),
            Some(FileKind::Dir)
        );
        assert_eq!(
            SystemFs.lstat(&file).expect("file").map(|i| i.kind),
            Some(FileKind::File)
        );
        assert_eq!(
            SystemFs.lstat(&link).expect("link").map(|i| i.kind),
            Some(FileKind::Symlink)
        );
        assert_eq!(
            SystemFs.lstat(&dir.path().join("none")).expect("missing"),
            None
        );
    }
}

//! Blocking start of a unit's leader inside a new job.

use std::{
    collections::BTreeMap,
    io::{self, PipeReader},
    os::windows::{ffi::OsStrExt, io::AsHandle},
    path::{Path, PathBuf},
};

use devlanes_core::{LaunchSpec, OutputStream, TerminalSize};
use devlanes_sys_windows::{
    conpty::PseudoConsole,
    job::{Job, JobOptions},
    spawn::{self as win, Child, CreationFlags, EnvError, Environment, Spawn, Stdio, Stream},
};

use crate::{
    StderrMode,
    command::{self, CommandError, Quoting},
    terminal::Terminal,
};

/// The program `run:` strings and `ready.cmd` resolve to on every OS.
pub(crate) const POSIX_SHELL: &str = "/bin/sh";

/// Working directories this long or longer are refused: `cmd.exe` cannot use
/// them and runs batch files from the Windows directory instead.
const MAX_CWD: usize = 260;

/// Why a unit could not start. Converted to [`devlanes_core::Error`] at the
/// runner boundary; [`SpawnError::code`] names the case.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The unit runs through `/bin/sh` and no Git for Windows `sh.exe` was found.
    #[error(
        "`run:` strings and `/bin/sh` need Git for Windows' sh.exe, and no `git` on PATH has one; install Git for Windows and restart the daemon"
    )]
    ShellMissing,
    /// The working directory is too long for `cmd.exe`.
    #[error(
        "the working directory {} is {length} characters long; Windows units need one shorter than {MAX_CWD}",
        cwd.display()
    )]
    CwdTooLong { cwd: PathBuf, length: usize },
    /// The program was not found on the unit's `PATH` or at its path.
    #[error("{} not found", program.display())]
    NotFound {
        program: PathBuf,
        searched: Vec<PathBuf>,
    },
    /// An argument for a `.bat` or `.cmd` program that `cmd.exe` would cut
    /// short or misread.
    #[error("argument {0:?} cannot be passed to a batch file safely")]
    UnsafeBatchArgument(String),
    /// The program cannot be resolved or started as given.
    #[error("{0}")]
    InvalidCommand(String),
    /// The environment cannot be passed to a Windows process.
    #[error(transparent)]
    Environment(#[from] EnvError),
    /// Creating the job, the pipes or the process failed.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl SpawnError {
    /// A stable name for the case: `shell_missing`, `cwd_too_long`,
    /// `not_found`, `unsafe_batch_argument`, `invalid_command`,
    /// `invalid_environment` or `io`.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ShellMissing => "shell_missing",
            Self::CwdTooLong { .. } => "cwd_too_long",
            Self::NotFound { .. } => "not_found",
            Self::UnsafeBatchArgument(_) => "unsafe_batch_argument",
            Self::InvalidCommand(_) => "invalid_command",
            Self::Environment(_) => "invalid_environment",
            Self::Io(_) => "io",
        }
    }
}

impl From<CommandError> for SpawnError {
    /// Keeps the cases callers act on apart.
    fn from(error: CommandError) -> Self {
        match error {
            CommandError::NotFound { program, searched } => Self::NotFound { program, searched },
            CommandError::ArgumentNotSafeForBatch(arg) => Self::UnsafeBatchArgument(arg),
            other @ (CommandError::ContainsNul(_) | CommandError::InvalidProgram(_)) => {
                Self::InvalidCommand(other.to_string())
            }
        }
    }
}

impl From<SpawnError> for devlanes_core::Error {
    /// A failed creation stays the OS error, so the daemon classifies it as it
    /// does on Unix. Every other case is an I/O error (`NotFound` for a
    /// missing program, `InvalidInput` for refusals) whose source is a
    /// [`devlanes_core::SpawnRefusal`] with the case's code, which the daemon
    /// reports as the `spawn_failed` reason.
    fn from(error: SpawnError) -> Self {
        let kind = match error {
            SpawnError::Io(error) => return Self::Io(error),
            SpawnError::NotFound { .. } => io::ErrorKind::NotFound,
            _ => io::ErrorKind::InvalidInput,
        };
        let refusal = devlanes_core::SpawnRefusal {
            code: error.code(),
            message: error.to_string(),
            searched: match error {
                SpawnError::NotFound { searched, .. } => searched,
                _ => Vec::new(),
            },
        };
        Self::Io(io::Error::new(kind, refusal))
    }
}

/// A started leader and the parent's ends of its output pipes.
pub(crate) struct Launched {
    pub child: Child,
    pub job: Job,
    pub pipes: Vec<(OutputStream, PipeReader)>,
    /// The program is Git's `sh.exe` standing in for `/bin/sh`.
    pub through_shell: bool,
    /// For `pty: true`: the pseudo console and the size it was created at.
    pub terminal: Option<(Terminal, TerminalSize)>,
}

/// Starts `spec`'s leader in a new job. Blocking: call from `spawn_blocking`.
///
/// The job lets no process leave, ends a crashing member at once, and does
/// not end its members when closed, so pipe units outlive the daemon. A
/// `pty: true` leader runs in a pseudo console of `spec.size`, or
/// [`TerminalSize::RENDERED_HEADLESS`] when that is the default (no viewer
/// yet); its one output pipe carries the console host's rendering. Nothing
/// that fails before `CreateProcessW` succeeded leaves a job or a process
/// behind.
pub(crate) fn launch(
    spec: LaunchSpec,
    stderr: StderrMode,
    shell: Option<&Path>,
) -> Result<Launched, SpawnError> {
    let through_shell = spec.program == POSIX_SHELL;
    let mut env = spec.env;
    let program = if through_shell {
        let shell = shell.ok_or(SpawnError::ShellMissing)?;
        devlanes_shell_windows::shell_env(&mut env, shell);
        shell.to_string_lossy().into_owned()
    } else {
        spec.program
    };
    let length = spec.cwd.as_os_str().encode_wide().count();
    if length >= MAX_CWD {
        return Err(SpawnError::CwdTooLong {
            cwd: spec.cwd,
            length,
        });
    }
    let env = child_env(env);
    let system = devlanes_sys_windows::file::system_directory()?;
    let quoting = if through_shell {
        Quoting::Msys
    } else {
        Quoting::Native
    };
    let command = command::build(&program, &spec.args, &env, &spec.cwd, &system, quoting)?;
    let environment = Environment::new(&env)?;
    let job = || {
        Job::create(JobOptions {
            kill_on_close: false,
            die_on_unhandled_exception: true,
        })
    };
    if spec.pty {
        let size = if spec.size == TerminalSize::default() {
            TerminalSize::RENDERED_HEADLESS
        } else {
            spec.size
        };
        let (input_reader, input_writer) = win::pipe()?;
        let (output_reader, output_writer) = win::pipe()?;
        let console = PseudoConsole::create(
            size.columns,
            size.rows,
            input_reader.as_handle(),
            output_writer.as_handle(),
        )?;
        // Before the leader starts, so a failure leaves no process behind.
        let terminal = Terminal::new(console, size, input_writer)?;
        let job = job()?;
        // No `CREATE_NO_WINDOW`: the leader is attached to the pseudo console,
        // which has no window.
        let child = win::spawn(&Spawn {
            program: &command.application,
            command_line: &command.command_line,
            environment: Some(&environment),
            cwd: Some(&spec.cwd),
            stdio: Stdio::PseudoConsole(terminal.console()),
            inherit: &[],
            job: Some(&job),
            flags: CreationFlags::default(),
        })?;
        // The console holds its own duplicates; ours would keep its output
        // from ever ending.
        drop((input_reader, output_writer));
        return Ok(Launched {
            child,
            job,
            pipes: vec![(OutputStream::Terminal, output_reader)],
            through_shell,
            terminal: Some((terminal, size)),
        });
    }

    let merged = stderr == StderrMode::Merged;
    let (reader, writer) = win::pipe()?;
    let errors = if merged { None } else { Some(win::pipe()?) };
    let job = job()?;
    let child = win::spawn(&Spawn {
        program: &command.application,
        command_line: &command.command_line,
        environment: Some(&environment),
        cwd: Some(&spec.cwd),
        stdio: Stdio::Handles {
            stdin: Stream::Null,
            stdout: Stream::Handle(writer.as_handle()),
            stderr: Stream::Handle(
                errors
                    .as_ref()
                    .map_or(writer.as_handle(), |(_, writer)| writer.as_handle()),
            ),
        },
        inherit: &[],
        job: Some(&job),
        flags: CreationFlags {
            no_window: true,
            ..CreationFlags::default()
        },
    })?;
    // Only the child may hold the write ends, or the readers never see EOF.
    drop(writer);
    let mut pipes = vec![(OutputStream::Stdout, reader)];
    if let Some((reader, writer)) = errors {
        drop(writer);
        pipes.push((OutputStream::Stderr, reader));
    }
    Ok(Launched {
        child,
        job,
        pipes,
        through_shell,
        terminal: None,
    })
}

/// A child's environment from `env`: the `=C:`-style per-drive directory
/// entries are dropped (a block cannot carry names starting with `=`), and
/// names equal ignoring case keep the value of the one sorting last (Windows
/// has one variable for both). Shell units get the shell's variables from
/// [`devlanes_shell_windows::shell_env`] before this.
pub(crate) fn child_env(env: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut folded: BTreeMap<String, (String, String)> = BTreeMap::new();
    for (name, value) in env {
        if name.starts_with('=') {
            continue;
        }
        folded.insert(devlanes_core::env::fold_name(name.clone()), (name, value));
    }
    folded.into_values().collect()
}

/// Whether `name` is the console host Windows starts for a console; the unit's
/// own one is a member of its job.
pub(crate) fn is_console_host(name: &str) -> bool {
    name.eq_ignore_ascii_case("conhost.exe")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drive entries go, case duplicates collapse to the last spelling, and
    /// shell units get the MSYS variables over any of their own plus Git's
    /// tools at the end of `PATH`.
    #[test]
    fn environment_rules() {
        let env = BTreeMap::from([
            ("=C:".to_owned(), r"C:\work".to_owned()),
            ("PATH".to_owned(), r"C:\a".to_owned()),
            ("Path".to_owned(), r"C:\b".to_owned()),
            ("msys_no_pathconv".to_owned(), "0".to_owned()),
            ("HOME".to_owned(), r"C:\Users\acme".to_owned()),
        ]);
        let native = child_env(env.clone());
        assert_eq!(
            native,
            BTreeMap::from([
                ("HOME".to_owned(), r"C:\Users\acme".to_owned()),
                ("Path".to_owned(), r"C:\b".to_owned()),
                ("msys_no_pathconv".to_owned(), "0".to_owned()),
            ])
        );
        assert!(Environment::new(&native).is_ok());
        let mut shell = child_env(env);
        devlanes_shell_windows::shell_env(&mut shell, Path::new(r"C:\Git\usr\bin\sh.exe"));
        let shell = child_env(shell);
        assert_eq!(shell.get("MSYS_NO_PATHCONV").map(String::as_str), Some("1"));
        assert_eq!(
            shell.get("MSYS2_ARG_CONV_EXCL").map(String::as_str),
            Some("*")
        );
        assert!(!shell.contains_key("msys_no_pathconv"));
        assert_eq!(
            shell.get("Path").map(String::as_str),
            Some(r"C:\b;C:\Git\usr\bin")
        );
        assert!(Environment::new(&shell).is_ok());
    }

    /// Every case but a failed creation reaches the daemon as an I/O error
    /// whose source names the case; a missing program keeps the `NotFound`
    /// kind and the directories searched.
    #[test]
    fn errors_convert() {
        let refusal = |error: SpawnError| {
            let devlanes_core::Error::Io(io) = devlanes_core::Error::from(error) else {
                panic!("an I/O error");
            };
            let kind = io.kind();
            let refusal = io
                .into_inner()
                .and_then(|inner| inner.downcast::<devlanes_core::SpawnRefusal>().ok())
                .expect("a refusal");
            (kind, *refusal)
        };
        let (kind, shell) = refusal(SpawnError::ShellMissing);
        assert_eq!(kind, io::ErrorKind::InvalidInput);
        assert_eq!(shell.code, "shell_missing");
        assert!(
            shell.message.contains("Git for Windows"),
            "{}",
            shell.message
        );
        let missing = SpawnError::from(CommandError::NotFound {
            program: "tool".into(),
            searched: vec![r"C:\bin".into()],
        });
        assert_eq!(missing.code(), "not_found");
        let (kind, missing) = refusal(missing);
        assert_eq!(kind, io::ErrorKind::NotFound);
        assert_eq!(missing.searched, [PathBuf::from(r"C:\bin")]);
        let batch = SpawnError::from(CommandError::ArgumentNotSafeForBatch("a\nb".into()));
        assert_eq!(batch.code(), "unsafe_batch_argument");
        assert_eq!(refusal(batch).1.code, "unsafe_batch_argument");
        let os = devlanes_core::Error::from(SpawnError::Io(io::Error::from_raw_os_error(5)));
        assert!(matches!(os, devlanes_core::Error::Io(io) if io.raw_os_error() == Some(5)));
    }
}

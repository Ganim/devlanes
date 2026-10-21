//! Argument parsing and the subcommands of `devlanes-hosts`.
//!
//! Output is meant for the devlanes daemon and for people: one `key=value` line on
//! stdout for `apply` and `clear`, the raw block for `status`, and
//! `devlanes-hosts: <message>` on stderr for errors.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use crate::block::{self, BlockError};
use crate::names::{self, InputError, MAX_INPUT_BYTES, NameError, Suffixes};
use crate::write::{self, Outcome, UpdateError, WriteError};
use crate::{HOSTS_PATH, PROTOCOL, exit};

/// A subcommand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Sets the block to the names on stdin, allowing `.localhost` plus `tld`.
    Apply {
        /// Extra allowed suffix, validated by [`names::validate_tld`].
        tld: Option<String>,
    },
    /// Removes the block.
    Clear,
    /// Prints the block.
    Status,
    /// Prints version, protocol and build profile.
    Version,
}

/// A parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// What to do.
    pub command: Command,
    /// File to edit; always `/etc/hosts` in release builds.
    pub hosts_file: PathBuf,
}

impl Invocation {
    /// Caches are only flushed for the real file; a test file has no readers.
    pub fn flushes(&self) -> bool {
        self.hosts_file == std::path::Path::new(HOSTS_PATH)
    }
}

/// Usage text for `--help` and usage errors.
pub const USAGE: &str = "usage: devlanes-hosts <command>

commands:
  apply [--tld <tld>]  replace the devlanes block in /etc/hosts with the names on
                       stdin (one per line); names must end in .localhost or .<tld>
  clear                remove the devlanes block
  status               print the devlanes block (needs no root)
  version              print version, protocol and build profile
";

/// Parses arguments after the program name.
///
/// `--hosts-file <path>` exists only in debug builds so tests can edit a temp file.
/// It is refused when `under_sudo` is set: a debug build reachable through the
/// NOPASSWD rule would otherwise let the user write any file as root.
pub fn parse(args: &[String], under_sudo: bool) -> Result<Invocation, String> {
    let mut command = None;
    let mut tld = None;
    // Only the debug-only `--hosts-file` arm reassigns it.
    #[cfg_attr(not(debug_assertions), allow(unused_mut))]
    let mut hosts_file = PathBuf::from(HOSTS_PATH);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--tld" => {
                let value = iter.next().ok_or("--tld needs a value")?;
                if tld.replace(value.clone()).is_some() {
                    return Err("--tld given twice".into());
                }
            }
            #[cfg(debug_assertions)]
            "--hosts-file" => {
                if under_sudo {
                    return Err("--hosts-file is refused under sudo".into());
                }
                let value = iter.next().ok_or("--hosts-file needs a value")?;
                hosts_file = PathBuf::from(value);
            }
            "apply" | "clear" | "status" | "version" if command.is_none() => {
                command = Some(arg.clone());
            }
            other => return Err(format!("unexpected argument `{other}`")),
        }
    }
    // Release builds never read `under_sudo`; keep the parameter for one signature.
    let _ = under_sudo;
    let command = match command.as_deref() {
        Some("apply") => Command::Apply { tld },
        Some(_) if tld.is_some() => return Err("--tld only applies to `apply`".into()),
        Some("clear") => Command::Clear,
        Some("status") => Command::Status,
        Some("version") => Command::Version,
        _ => return Err("missing command".into()),
    };
    Ok(Invocation {
        command,
        hosts_file,
    })
}

/// A failure mapped to its exit code.
#[derive(Debug)]
struct Failure {
    code: u8,
    message: String,
}

impl Failure {
    /// A failure with `code`.
    fn new(code: u8, message: impl ToString) -> Self {
        Self {
            code,
            message: message.to_string(),
        }
    }
}

impl From<NameError> for Failure {
    /// Bad `--tld`.
    fn from(error: NameError) -> Self {
        Self::new(exit::INVALID_NAMES, error)
    }
}

impl From<InputError> for Failure {
    /// Bad names on stdin.
    fn from(error: InputError) -> Self {
        Self::new(exit::INVALID_NAMES, error)
    }
}

impl From<WriteError> for Failure {
    /// Refused or failed file access.
    fn from(error: WriteError) -> Self {
        let code = match error {
            WriteError::Io { .. } => exit::IO,
            WriteError::Symlink(_) | WriteError::NotRegular(_) | WriteError::Changed(_) => {
                exit::REFUSED_FILE
            }
        };
        Self::new(code, error)
    }
}

impl From<BlockError> for Failure {
    /// Malformed markers.
    fn from(error: BlockError) -> Self {
        Self::new(exit::REFUSED_FILE, error)
    }
}

impl From<UpdateError<BlockError>> for Failure {
    /// Either half of an update.
    fn from(error: UpdateError<BlockError>) -> Self {
        match error {
            UpdateError::Content(error) => error.into(),
            UpdateError::Write(error) => error.into(),
        }
    }
}

/// Runs `invocation`, writing results to `stdout` and warnings to `stderr`.
///
/// Returns the exit code; errors are printed to `stderr`.
pub fn execute(
    invocation: &Invocation,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> u8 {
    match run(invocation, stdin, stdout, stderr) {
        Ok(()) => exit::OK,
        Err(failure) => {
            let _ = writeln!(stderr, "devlanes-hosts: {}", failure.message);
            failure.code
        }
    }
}

/// [`execute`] with typed failures.
fn run(
    invocation: &Invocation,
    stdin: &mut dyn Read,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), Failure> {
    let path = &invocation.hosts_file;
    let printed = |result: io::Result<()>| result.map_err(|error| Failure::new(exit::IO, error));
    match &invocation.command {
        Command::Version => printed(writeln!(
            stdout,
            "devlanes-hosts {} protocol={PROTOCOL} build={}",
            env!("CARGO_PKG_VERSION"),
            crate::BUILD
        )),
        Command::Status => {
            let original = write::read(path)?;
            if let Some(current) = block::current(&original.bytes)? {
                printed(stdout.write_all(current))?;
            }
            Ok(())
        }
        Command::Clear => {
            let outcome = write::update(path, |bytes| block::rewrite(bytes, &Default::default()))?;
            finish(invocation, outcome, 0, stdout, stderr)
        }
        Command::Apply { tld } => {
            let suffixes = Suffixes::new(tld.as_deref())?;
            let mut input = Vec::new();
            stdin
                .take(MAX_INPUT_BYTES as u64 + 1)
                .read_to_end(&mut input)
                .map_err(|error| Failure::new(exit::IO, format!("reading stdin: {error}")))?;
            let names = names::parse_names(&input, &suffixes)?;
            let outcome = write::update(path, |bytes| block::rewrite(bytes, &names))?;
            finish(invocation, outcome, names.len(), stdout, stderr)
        }
    }
}

/// Flushes caches after a change and prints the result line.
fn finish(
    invocation: &Invocation,
    outcome: Outcome,
    count: usize,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> Result<(), Failure> {
    if outcome == Outcome::Changed && invocation.flushes() {
        for warning in crate::flush::flush() {
            let _ = writeln!(stderr, "devlanes-hosts: warning: {warning}");
        }
    }
    let word = match outcome {
        Outcome::Changed => "changed",
        Outcome::Unchanged => "unchanged",
    };
    writeln!(stdout, "{word} names={count}").map_err(|error| Failure::new(exit::IO, error))
}

/// Entry point for the binary: reads real arguments, stdin and the environment.
pub fn main() -> ExitCode {
    let args: Result<Vec<String>, _> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.into_string())
        .collect();
    let mut stderr = io::stderr().lock();
    let Ok(args) = args else {
        let _ = writeln!(stderr, "devlanes-hosts: arguments must be UTF-8\n{USAGE}");
        return ExitCode::from(exit::USAGE);
    };
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print!("{USAGE}");
        return ExitCode::from(exit::OK);
    }
    // sudo always sets SUDO_UID and the caller cannot clear it through sudo's env_reset.
    let under_sudo = std::env::var_os("SUDO_UID").is_some();
    let invocation = match parse(&args, under_sudo) {
        Ok(invocation) => invocation,
        Err(message) => {
            let _ = writeln!(stderr, "devlanes-hosts: {message}\n{USAGE}");
            return ExitCode::from(exit::USAGE);
        }
    };
    let code = execute(
        &invocation,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
        &mut stderr,
    );
    ExitCode::from(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| (*arg).to_owned()).collect()
    }

    #[test]
    fn parses_commands() {
        let apply = parse(&args(&["apply", "--tld", "test"]), true).expect("apply");
        assert_eq!(
            apply.command,
            Command::Apply {
                tld: Some("test".into())
            }
        );
        assert_eq!(apply.hosts_file, PathBuf::from("/etc/hosts"));
        assert!(apply.flushes());
        assert_eq!(
            parse(&args(&["--tld", "test", "apply"]), false)
                .expect("flag first")
                .command,
            Command::Apply {
                tld: Some("test".into())
            }
        );
        for (list, command) in [
            (&["clear"][..], Command::Clear),
            (&["status"], Command::Status),
            (&["version"], Command::Version),
            (&["apply"], Command::Apply { tld: None }),
        ] {
            assert_eq!(parse(&args(list), true).expect("parse").command, command);
        }
    }

    #[test]
    fn rejects_bad_usage() {
        for list in [
            &[][..],
            &["apply", "clear"],
            &["apply", "--tld"],
            &["apply", "--tld", "a", "--tld", "b"],
            &["clear", "--tld", "test"],
            &["remove"],
            &["apply", "extra.localhost"],
        ] {
            assert!(parse(&args(list), false).is_err(), "{list:?}");
        }
    }

    #[cfg(debug_assertions)]
    #[test]
    fn hosts_file_override_is_debug_only_and_refused_under_sudo() {
        let parsed = parse(&args(&["--hosts-file", "/tmp/h", "clear"]), false).expect("override");
        assert_eq!(parsed.hosts_file, PathBuf::from("/tmp/h"));
        assert!(!parsed.flushes());
        assert_eq!(
            parse(&args(&["--hosts-file", "/tmp/h", "clear"]), true),
            Err("--hosts-file is refused under sudo".into())
        );
    }

    #[cfg(not(debug_assertions))]
    #[test]
    fn hosts_file_override_does_not_exist_in_release() {
        assert!(parse(&args(&["--hosts-file", "/tmp/h", "clear"]), false).is_err());
    }
}

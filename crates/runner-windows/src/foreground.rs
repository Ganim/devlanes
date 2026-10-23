//! Commands run in the foreground of this process's console, such as the test
//! command of `devlanes with`, resolved and quoted the way units are.

use std::collections::BTreeMap;
use std::io::Write;
use std::os::windows::io::{AsHandle, AsRawHandle};
use std::path::Path;

use devlanes_sys_windows::spawn::{
    self as win, Child, CreationFlags, Environment, Spawn, Stdio, Stream,
};

use crate::SpawnError;
use crate::command::{self, Quoting};
use crate::spawn::child_env;

/// `STATUS_CONTROL_C_EXIT`, the exit code of a process that Ctrl-C or
/// Ctrl-Break ended.
const STATUS_CONTROL_C_EXIT: u32 = 0xC000_013A;

/// Starts `program` with `args` in `cwd`, with exactly `env` (minus the
/// `=C:`-style drive entries), on this process's console and standard
/// handles. `program` is looked up on `env`'s `PATH` with `PATHEXT`, and batch
/// files run through `cmd.exe` with std's escaping, as for units; the cases
/// that refuse a unit refuse it here too. The child is in no job, like a
/// command a shell starts. Blocking: call it off async executor threads.
pub fn spawn_foreground(
    program: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
    cwd: &Path,
) -> Result<Child, SpawnError> {
    let env = child_env(env.clone());
    let system = devlanes_sys_windows::file::system_directory()?;
    let command = command::build(program, args, &env, cwd, &system, Quoting::Native)?;
    let environment = Environment::new(&env)?;
    // The child writes to the same handles; what this process printed must
    // come first.
    let _ = std::io::stdout().flush();
    let (stdin, stdout, stderr) = (std::io::stdin(), std::io::stdout(), std::io::stderr());
    let child = win::spawn(&Spawn {
        program: &command.application,
        command_line: &command.command_line,
        environment: Some(&environment),
        cwd: Some(cwd),
        stdio: Stdio::Handles {
            stdin: stream(&stdin),
            stdout: stream(&stdout),
            stderr: stream(&stderr),
        },
        inherit: &[],
        job: None,
        flags: CreationFlags::default(),
    })?;
    Ok(child)
}

/// The standard handle `handle` for the child, or `NUL` when this process has
/// none (started without a console or redirection).
fn stream(handle: &impl AsHandle) -> Stream<'_> {
    let handle = handle.as_handle();
    if handle.as_raw_handle().is_null() {
        Stream::Null
    } else {
        Stream::Handle(handle)
    }
}

/// The status a shell reports for a Windows exit `code`: 130 for
/// `STATUS_CONTROL_C_EXIT`, as for a Unix SIGINT, the code itself up to 255,
/// and 1 for any other code (NTSTATUS crash codes, large exit values).
pub fn shell_exit_code(code: u32) -> u8 {
    if code == STATUS_CONTROL_C_EXIT {
        return 130;
    }
    u8::try_from(code).unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Codes a shell can show stay, Ctrl-C exits read as 130, others as 1.
    #[test]
    fn exit_codes() {
        assert_eq!(shell_exit_code(0), 0);
        assert_eq!(shell_exit_code(3), 3);
        assert_eq!(shell_exit_code(255), 255);
        assert_eq!(shell_exit_code(256), 1);
        assert_eq!(shell_exit_code(0xC000_013A), 130);
        assert_eq!(shell_exit_code(0xC000_0005), 1);
    }
}

# devlanes-shell-windows

Finds Git for Windows' `sh.exe`, which runs `run:` strings and `ready.cmd` on Windows in place of `/bin/sh`, and builds its environment and command line. The binary looks the shell up once at daemon start; `devlanes-runner-windows` and the daemon's readiness checks start it. It is a crate of its own because both of those need it and neither may depend on the other. On other platforms the crate is empty.

## Usage

```rust
use std::collections::BTreeMap;

async fn command_line() -> Option<std::ffi::OsString> {
    let shell = devlanes_shell_windows::windows_shell().await?;
    let mut env = BTreeMap::from([("PATH".to_owned(), r"C:\Windows".to_owned())]);
    devlanes_shell_windows::shell_env(&mut env, &shell);
    Some(devlanes_shell_windows::shell_command_line(&shell, "echo ready"))
}
```

## API

- `windows_shell()`: `<root>\usr\bin\sh.exe` for the `git` on `PATH`, from `git --exec-path` or where `git.exe` sits; `None` without Git for Windows.
- `shell_env(env, shell)`: sets `MSYS_NO_PATHCONV` and `MSYS2_ARG_CONV_EXCL` and appends the shell's `usr\bin` to `PATH`.
- `shell_command_line(shell, script)`: `"<shell>" "-c" "<script>"`, quoted for the MSYS2 runtime.
- `quote_msys(arg)`: one argument quoted for the MSYS2 runtime's parser.

## MSYS2 and Cygwin

Only Git for Windows' shell is used. MSYS2's and Cygwin's own Git print POSIX paths from `git --exec-path`, and their `sh.exe` ignores `MSYS_NO_PATHCONV`, so they are skipped.

## License

Apache-2.0

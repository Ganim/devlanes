# devlanes-logon-launcher

Builds `devlanesw.exe`, the program the Windows logon entry from `devlanes daemon install` runs. It is a GUI-subsystem program, so logon opens no console window, and it starts `devlanes.exe daemon run` from its own directory without a console either, then exits.

## Usage

```text
devlanesw.exe [--home <path>]
```

`daemon install` writes `"<dir>\devlanesw.exe" --home "<home>"` to `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. Without `--home` the default devlanes home is used. Exit codes: 0 once the daemon was started (a daemon already running for the home makes the new one exit quietly), 1 when it could not be started, 2 for other arguments.

## Behaviour

The daemon gets the same environment the CLI's own spawn gives it (`devlanes_client::daemon_environment`, plus `DEVLANES_HOME`) and runs in the home directory, so it never holds a worktree open. It leaves the caller's job object when the job allows it.

Nobody sees the exit code at logon, so a failed start is also appended to `<home>\logs\daemon.log` in the daemon's own line format, where `devlanes daemon logs` and support bundles show it.

On other systems the binary only says that it is Windows-only and exits 1.

## License

Apache-2.0

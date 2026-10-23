# Windows

devlanes runs natively on Windows 11 24H2 (build 26100) or later, x64 or arm64. The daemon, its units, the proxy and the CA live on Windows, and the CLI works from PowerShell, cmd and Git Bash. To run the Linux build inside a WSL 2 distro instead, see [Windows (WSL2)](wsl.md).

## Install

devlanes needs [Git for Windows](https://gitforwindows.org): its `sh.exe` runs `run:` strings (see [shells](#shells)). Then, in PowerShell:

```powershell
irm https://github.com/devlanes-run/devlanes/releases/latest/download/install.ps1 | iex
```

`install.ps1` checks the archive against the release's `SHA256SUMS`, installs `devlanes.exe`, `devlanesw.exe` and `devlanes-hosts-service.exe` to `%LOCALAPPDATA%\Programs\devlanes` (`DEVLANES_INSTALL_DIR` changes it) and puts that directory first in your user `PATH`, so open a new terminal afterwards. `DEVLANES_VERSION=X.Y.Z` pins a release and `DEVLANES_NO_MODIFY_PATH=1` leaves `PATH` alone. npm and cargo binstall install the same executables:

```powershell
npm install -g devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes-logon-launcher
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes-hosts-service
```

binstall takes one crate per run. `daemon install` and `certs trust --hosts` look for `devlanesw.exe` and `devlanes-hosts-service.exe` next to `devlanes.exe`, so install all three.

Run `devlanes doctor` once after installing. The home is `%USERPROFILE%\.devlanes`, and the daemon listens on a named pipe only your account can open.

## Shells

`run:` strings and `ready.cmd` are POSIX `sh` on every OS, so a `devlanes.yaml` works unchanged on macOS, Linux and Windows. The daemon runs them with Git for Windows' `sh.exe`, which it looks for once at startup from the `git.exe` on its `PATH` (or `git --exec-path`). Without Git for Windows it falls back to `cmd.exe /d /s /c`: `devlanes config check` warns (`shell_fallback`), POSIX syntax may fail, and a multi-line `run:` string fails its unit because `cmd.exe` would run only its first line. After installing Git for Windows, run `devlanes daemon restart`. A list `run` is an argv and needs no shell.

The CLI itself runs in any shell. Hints and `more:` lines are quoted for PowerShell; words without quotes paste into any shell. To load a unit's environment:

```powershell
devlanes env export api --shell pwsh | Out-String | Invoke-Expression   # PowerShell
```

```sh
eval "$(devlanes env export api)"                                       # Git Bash
```

`devlanes env shell` starts the program named by `SHELL` when that variable is set, otherwise `%ComSpec%`, normally `cmd.exe`. For PowerShell completions, add `devlanes completions powershell | Out-String | Invoke-Expression` to your `$PROFILE`.

The tab progress bar works in Windows Terminal. The TUI copies to the Windows clipboard directly, since classic console windows ignore OSC 52, and in a classic console window `tui.glyphs: auto` picks ASCII because its default fonts lack some of the symbols.

### Git Bash path conversion

Git Bash rewrites arguments and environment variables that look like POSIX paths before it starts a Windows program. In `devlanes exec --name x -- cmd /c dir` the `/c` arrives as `C:/`, and a variable exported as `/api` reaches devlanes, and through it the services started from that shell, as `C:/Program Files/Git/api`. Prefix such commands with `MSYS_NO_PATHCONV=1`, export it in `~/.bashrc`, or run them from PowerShell. `run:` strings are not affected: devlanes runs `sh.exe` with conversion turned off.

## Terminals in units

Services run in a pseudo console by default (`pty: true`). On Windows that is ConPTY, which renders the program's output the way a console would: it rewrites line endings, cursor movement and colour sequences, so `devlanes logs`, the log files and `ready.log` patterns see the console's rendering rather than the bytes the program wrote. `devlanes attach` and the TUI show the screen as the console drew it. Set `pty: false` on a unit whose logs must hold exactly what it wrote.

## Long paths

Worktrees with deep `node_modules` trees can pass Windows' 260-character path limit, which makes Git fail to check out or remove them. Let Git for Windows use long paths:

```sh
git config --global core.longpaths true
```

Run `devlanes stop` in a worktree before `git worktree remove`; [getting started](../getting-started.md#start-and-inspect) explains why.

## Ports

Windows reserves TCP port ranges for Hyper-V, WSL and Docker Desktop, and nothing can bind inside them:

```powershell
netsh int ipv4 show excludedportrange protocol=tcp
```

New lane port blocks skip these ranges. They can change after a reboot, so `devlanes doctor` warns when `ports.range`, the proxy ports or ports mirrored from a box overlap one; set `ports.range` in `~/.devlanes/config.yaml` clear of them.

The proxy needs no privilege for ports 443 and 80. Port 80 is often held by http.sys (PID 4, System; `netsh http show servicestate` lists who registered it), and the proxy then falls back to 8080 as on Linux.

With `proxy.lan: true` the proxy listens on every address, and Windows Defender Firewall asks the first time whether `devlanes.exe` may accept connections from the network. Until you allow it, other machines cannot connect. Loopback-only listening (the default) does not prompt.

## HTTPS

```powershell
devlanes certs trust
```

adds the CA to your user root store, which Chrome, Edge and Firefox read, so one store covers the `system`, `chrome` and `firefox` targets of `tls.trust`. It needs no administrator rights, but Windows shows a security warning that you confirm with Yes, once per change; `devlanes certs untrust` asks again. Over SSH, or anywhere without a desktop, the command fails before any change and says to run it from a desktop session.

Units get the CA through `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and the other variables in [hostnames and HTTPS](https.md#programs-started-by-devlanes). `%USERPROFILE%\.devlanes\ca\bundle.pem` holds Mozilla's roots, the roots in your Windows store and the devlanes CA. For tools you run yourself:

- `curl.exe` uses the Windows store, but fails because the devlanes CA publishes no revocation list; add `--ssl-revoke-best-effort`, which still verifies the chain.
- Python's `requests` reads `REQUESTS_CA_BUNDLE`, not `SSL_CERT_FILE`: `$env:REQUESTS_CA_BUNDLE = "$HOME\.devlanes\ca\bundle.pem"`.
- Node does not read the Windows store: `$env:NODE_EXTRA_CA_CERTS = "$HOME\.devlanes\ca\ca.pem"`.

### Hostnames for programs other than browsers

Browsers resolve `*.localhost` themselves, but Windows resolves only `localhost`, so Node, Python, Go and test runners need the names elsewhere. `devlanes certs trust --hosts` installs the `devlanes-hosts` service, which writes your lanes' hostnames to `%SystemRoot%\System32\drivers\etc\hosts`. Windows asks for administrator approval once; `devlanes certs untrust --hosts` removes the names and the service. [`crates/hosts-service/README.md`](../../crates/hosts-service/README.md) describes what it accepts.

With `hostnames: { resolver: resolver, tld: test }`, `devlanes certs trust --resolver` adds a Name Resolution Policy Table rule that sends `.test` lookups to the devlanes DNS server on `127.53.0.1:53`, also after an administrator prompt. Group Policy NRPT rules make Windows ignore local ones, and `doctor` says so. [Hostnames and HTTPS](https.md#programs-that-cannot-resolve-localhost) covers browsers' secure DNS.

Both commands need the administrator prompt on your desktop, so agents, CI and SSH sessions get `terminal_required`.

## Starting at logon

The daemon starts on the first command and keeps running after the terminal closes. To start it at logon:

```powershell
devlanes daemon install
```

writes a `devlanes` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that runs `devlanesw.exe`, which starts the daemon without a console window. It appears in Task Manager's Startup apps; turning it off there keeps the entry, `daemon status` and `doctor` say it is off, and `daemon install` turns it back on. `devlanes daemon uninstall` removes it and leaves a running daemon alone.

Signing out or shutting down stops the daemon: units stop without their `lane.stop` tasks, and Windows waits about 5 seconds for that, so a unit with a longer `stop.timeout` may be ended by Windows instead.

## Elevated terminals

The daemon refuses to run elevated (as administrator), since every unit would inherit administrator rights. Start devlanes from a terminal that is not elevated; `doctor` flags an elevated one. `DEVLANES_ALLOW_ELEVATED=1` lifts the refusal for CI runners that only run elevated.

Some terminals end every process they started when they close. `doctor` warns when the current one does; start the daemon from another terminal, such as Windows Terminal, and run `devlanes daemon restart`.

## Upgrading

`install.ps1` can upgrade while the daemon runs: Windows refuses to overwrite a running executable, so the script renames it to `devlanes.exe.old-<timestamp>` and installs next to it. `devlanes daemon restart` then switches to the new version, and `devlanes doctor` deletes the old file once nothing runs it.

npm and cargo binstall cannot replace a running `devlanes.exe`. Run `devlanes daemon stop` first, which stops every unit, then the upgrade command. The update notice and `doctor` print the command for the way you installed.

## Limitations

- Windows has no POSIX signals. `stop.signal: SIGINT` sends Ctrl-C, `SIGKILL` terminates at once, and every other signal sends Ctrl-Break; visible windows also get `WM_CLOSE`. Whatever runs when `stop.timeout` ends is terminated. A unit ended by Ctrl-C or Ctrl-Break shows as `interrupted (Ctrl-C)`.
- Each unit runs in a job object it cannot leave. A program that insists on starting a child outside its job (`CREATE_BREAKAWAY_FROM_JOB`) fails with access denied there.
- Units with `pty: true` stop about 5 seconds after the daemon dies, because their pseudo console closes with it. Units with `pty: false` keep running, and the next daemon finds them as orphans, as on macOS and Linux.
- Trusting or untrusting the CA always shows a Windows dialog, and the hosts service and the NRPT rule need administrator approval once. None of them work from agents or over SSH.
- The DNS server for `--resolver` always uses port 53 on `127.53.0.1`, because an NRPT rule cannot name a port.
- A Windows machine can connect to remote boxes but cannot be one. Windows' OpenSSH has no ControlMaster, so every reconnect authenticates again; see [remote setup](remote-setup.md#windows-clients).

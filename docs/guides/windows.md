# Windows

devlanes runs natively on Windows 11 24H2 (build 26100) or later, on x64 and arm64. The daemon, its units, the proxy and the CA all live on Windows, and the CLI works from PowerShell, cmd and Git Bash. This page covers what differs from macOS and Linux; everything else in the other guides applies unchanged. To run the Linux build inside a WSL distro instead, see [WSL](wsl.md).

## Requirements

- Windows 11 24H2 (build 26100) or later. `install.ps1` refuses older builds.
- [Git for Windows](https://gitforwindows.org). Lanes are Git worktrees, and its `sh.exe` runs `run:` strings and `ready.cmd` checks (see [shells](#shells)). Without it, a unit with a `run:` string fails with `shell_missing`, and so does a `cmd` ready check; `devlanes doctor` reports it. A `run` list is an argv and needs no shell.

## Install

In PowerShell:

```powershell
irm https://devlanes.run/install.ps1 | iex
```

The script checks the archive against the release's `SHA256SUMS`, installs `devlanes.exe` to `%LOCALAPPDATA%\Programs\devlanes` and puts that directory first in your user `PATH`; terminals opened afterwards find `devlanes`. Options go after the script, inside the quotes:

```powershell
iex "& {$(irm https://devlanes.run/install.ps1)} -Version 0.4.0"
iex "& {$(irm https://devlanes.run/install.ps1)} -Channel next -Dir D:\tools\devlanes"
```

`-Channel next` installs the latest build of `main`. The script works in Windows PowerShell 5.1 and PowerShell 7.

cargo binstall downloads the same release build:

```powershell
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
```

`npm install -g devlanes`, `uv tool install devlanes`, `pipx install devlanes` and `pip install devlanes` install the same build. Homebrew has no Windows build. Building from source (`cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes`) needs the Visual Studio Build Tools. There is no `devlanes-hosts` helper on Windows.

Run `devlanes doctor` once after installing. The home is `%USERPROFILE%\.devlanes` (`DEVLANES_HOME` overrides it; `HOME`, which Git Bash sets, is ignored). It must be private: only your account, SYSTEM and Administrators may have access, and `doctor` prints the `icacls` commands that fix a home others can reach. The daemon listens on a named pipe, `\\.\pipe\devlanes-<hash>`, that only your account can open; there is no `daemon.sock`.

## Shells

`run:` strings and `ready.cmd` are POSIX `sh` on every OS, so one `devlanes.yaml` works on macOS, Linux and Windows. The daemon runs them with Git for Windows' `usr\bin\sh.exe`, which it looks up once when it starts, from the `git` on its `PATH` (`git --exec-path`, else where `git.exe` sits). MSYS2's and Cygwin's own Git are not used. After installing Git for Windows, run `devlanes daemon restart`.

Git's `usr\bin` is added to the end of the unit's `PATH`, so scripts find `sleep`, `grep` and `sed`, while programs found earlier on `PATH` still win.

The daemon runs `sh.exe` with `MSYS_NO_PATHCONV=1` and `MSYS2_ARG_CONV_EXCL=*`, so arguments and variables reach Windows programs exactly as written. MSYS paths are not converted either: inside a `run:` string, `$PWD` and `pwd` give `/c/src/acme`, which Windows programs do not understand. Use `pwd -W` (`C:/src/acme`) or `cygpath -w "$PWD"` when a Windows program needs the path. `${{ lane.data_path }}` is written with `/` and can be used as it is; `${{ lane.path }}` and `${{ project.primary_path }}` keep their `\`, which `sh` reads as escapes outside quotes, so put them in single quotes:

```yaml
services:
  api:
    run: node server.js --root '${{ lane.path }}'
```

A bare program name is looked up on the unit's `PATH` with `PATHEXT` (`.COM`, `.EXE`, `.BAT` and `.CMD` only). `.bat` and `.cmd` files, such as npm's shims, run through `cmd.exe`; an argument that `cmd.exe` cannot take safely (a line break) fails the unit with `unsafe_batch_argument`. A working directory of 260 characters or more fails with `cwd_too_long`.

### Commands you type

The CLI runs in any shell. Git Bash rewrites arguments and variables that look like POSIX paths before it starts a Windows program: in `devlanes exec --name dir -- cmd /c dir` the `/c` arrives as `C:/`. Prefix such commands with `MSYS_NO_PATHCONV=1`, or run them from PowerShell or cmd. Most commands that devlanes prints as hints use POSIX shell quoting.

Git Bash's own window (mintty) gives Windows programs no console: prompts are cancelled with a message and `attach` fails. Use Windows Terminal or a console window for those.

`devlanes env export` prints POSIX `export` lines for `eval` in Git Bash. In PowerShell, add `--shell pwsh`:

```powershell
devlanes env export api --shell pwsh | Out-String | Invoke-Expression
```

```sh
eval "$(devlanes env export api)"   # Git Bash
```

The PowerShell lines are plain ASCII whatever the console code page: a value with anything other than printable ASCII (a line break, `é`, `日本`) is written as Base64 and decoded when the line runs, and a name that `$env:NAME` cannot spell, such as `NEXT-PUBLIC`, is set through `Set-Item -LiteralPath Env:NEXT-PUBLIC`. Windows PowerShell 5.1 and PowerShell before 7.5 remove a variable that is assigned an empty value, so variables with empty values end up unset there.

`devlanes env shell` starts the program named by `SHELL` when that variable is set, otherwise `%ComSpec%` (normally `cmd.exe`).

Environment variable names are uppercased where they enter devlanes, since Windows treats `Path` and `PATH` as one variable: `env show` lists `PATH`, and `${{ env.Path }}` and `${{ env.PATH }}` read the same value.

For PowerShell completions, add this line to your `$PROFILE`:

```powershell
devlanes completions powershell | Out-String | Invoke-Expression
```

## Terminals in units

Services default to `pty: true`. On Windows that runs the service in a ConPTY pseudo console, which re-renders the program's output the way a console window would: it rewrites line endings, cursor movement and colour sequences. `devlanes logs`, the log files and `ready.log` patterns therefore see the console's rendering, not the bytes the program wrote. Set `pty: false` on a unit whose logs must hold exactly what it wrote.

`pty: true` units stop about 5 seconds after the daemon dies, because their pseudo console closes with it. `pty: false` units keep running, and the next daemon finds them as orphans, as on macOS and Linux.

While nobody is attached, a `pty: true` unit's console is 300 columns wide, so long log lines are not broken; `devlanes attach`, and the TUI once you type into the unit or zoom its pane, size it to your terminal. Git for Windows' `sh` ends a `read` when its terminal is resized, so a `run:` string like `while read line; do ...; done` stops the first time that happens; a program that reads its input itself is not affected.

## Stopping units

Windows has no POSIX signals. Console programs see Ctrl-C and Ctrl-Break, so `stop.signal` maps to those:

| `stop.signal` | On Windows |
| --- | --- |
| `SIGINT` | Ctrl-C; Ctrl-Break for a `run:` string, because programs that Git's `sh` starts ignore Ctrl-C |
| `SIGTERM` (default), `SIGHUP`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2` | Ctrl-Break |
| `SIGKILL` | the unit's processes are ended at once |

A unit whose signal is sent as something else gets a launch note that says so. A stop also asks the unit's visible windows to close (`WM_CLOSE`), for desktop apps that do not watch their console. Whatever still runs when `stop.timeout` ends is terminated. A process ended by a console event reports `interrupted by Ctrl-C or Ctrl-Break`, because Windows gives both the same exit code.

## Idle lanes

A throttled lane runs in efficiency mode, as Task Manager sets it, and a frozen lane's processes are suspended; devlanes does not trim their memory. [Lane lifecycle](lifecycle.md#windows) has the details.

## Long paths

Worktrees with deep `node_modules` trees can pass Windows' 260-character path limit, and Git then fails to check out or remove files. Let Git for Windows use long paths:

```sh
git config --global core.longpaths true
```

## Ports

Hyper-V, WSL and Docker Desktop reserve TCP port ranges, and nothing can bind inside them:

```powershell
netsh int ipv4 show excludedportrange protocol=tcp
```

New lane port blocks skip these ranges, so the default `ports.range` works whatever they are. The ranges can change after a reboot or when one of those programs starts, and a lane that got its ports before then cannot bind the ones that are now reserved. `devlanes doctor` warns when that happens to a lane, a proxy port or a port mirrored from a box; `devlanes lane clean` on that lane gives it new ports on its next start.

The proxy needs no privilege for ports 443 and 80, and listens on loopback only. Port 80 is often held by http.sys (PID 4, System; `netsh http show servicestate` lists who registered it); the proxy then falls back to 8080 as it does on Linux, and the fallback message names that command. Another reverse proxy listening on the wildcard address does not make it fall back: both run and requests to `localhost` reach devlanes, which `doctor` reports ([next to another reverse proxy](https.md#next-to-another-reverse-proxy)).

With `proxy.lan: true` the proxy listens on every address, and Windows Defender Firewall asks the first time whether `devlanes.exe` may accept connections from the network. Until you allow it, other devices cannot connect. Loopback-only listening, the default, does not prompt.

## HTTPS

```powershell
devlanes certs trust
```

adds the CA to your user root store (`Cert:\CurrentUser\Root`). Chrome and Edge read that store, and Firefox 120 and later reads it through `security.enterprise_roots.enabled`, so the `system`, `chrome`, `firefox` and `windows` targets of `tls.trust` all mean that one store. (`windows` exists for devlanes inside WSL, where it reaches this store from the distro; see [WSL](wsl.md).) It needs no administrator rights, but Windows shows a security warning for each change, which you confirm with Yes; `devlanes certs untrust` asks again. Over SSH, or anywhere without a desktop, the command fails before any change and says to run it from a desktop session. A CA that an administrator trusted for every user counts as trusted, and one listed under Untrusted Certificates (`Disallowed`) counts as missing.

Units get the CA through `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and the other variables in [hostnames and HTTPS](https.md#programs-started-by-devlanes). `%USERPROFILE%\.devlanes\ca\bundle.pem` holds Mozilla's roots, the roots of your Windows store that are valid for servers (minus `Disallowed`) and the devlanes CA. Windows downloads most public roots only when a program first needs them, so its store alone would miss many. For tools you run yourself:

- `curl.exe` uses the Windows store but fails, because the devlanes CA publishes no revocation list. Add `--ssl-revoke-best-effort`, which still verifies the chain.
- Python's `requests` reads `REQUESTS_CA_BUNDLE`: `$env:REQUESTS_CA_BUNDLE = "$env:USERPROFILE\.devlanes\ca\bundle.pem"`.
- Node does not read the Windows store: `$env:NODE_EXTRA_CA_CERTS = "$env:USERPROFILE\.devlanes\ca\ca.pem"`.

`devlanes env shell` opens a shell that already has these variables.

## Hostnames

Windows' DNS client answers every `*.localhost` name with loopback, so Node, Python, browsers and other programs that use the system resolver reach lane hostnames with no setup. devlanes never writes the hosts file on Windows: `devlanes certs trust --hosts`, `hostnames.hosts_file: always` and `hostnames.resolver: hosts_file` have nothing to install, and `--hosts` says so. `devlanes doctor` shows the lookup the daemon tried when it started.

The resolver strategy works as on macOS and Linux:

```yaml
# ~\.devlanes\config.yaml
hostnames:
  resolver: resolver
  tld: test
```

```powershell
devlanes certs trust --resolver
```

adds a Name Resolution Policy Table (NRPT) rule, `HKLM\SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig\devlanes-test`, that sends `.test` lookups to the daemon's DNS server on `127.53.0.1:53`. Windows asks for administrator approval once, and the rule applies at once, with no restart. `devlanes certs untrust --resolver` removes every devlanes rule. Because the approval prompt needs your desktop, a non-interactive session such as an agent, CI or SSH gets `terminal_required`, unless it already runs elevated. When Group Policy sets NRPT rules, Windows ignores local ones, so `--resolver` refuses and `doctor` says why; it also refuses when another program's rule covers the TLD.

## Starting at logon

The daemon starts on the first command and keeps running after the terminal closes. To start it at logon:

```powershell
devlanes daemon install
```

writes a `devlanes` value under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` that runs `"<path>\devlanes.exe" daemon run`; no console window opens. As on macOS and Linux it checks the user config first. The Run key passes no environment, so its daemon always uses the default home, and `daemon install` refuses while `DEVLANES_HOME` points elsewhere. The entry appears in Task Manager's Startup apps; turning it off there keeps the value, `doctor` reports it as off, and `daemon install` turns it back on. `devlanes daemon uninstall` removes it and leaves a running daemon alone.

Signing out or shutting down stops the daemon and its units. Windows waits about 5 seconds for that, so a unit with a longer `stop.timeout` may be ended by Windows first.

## Elevated terminals

devlanes runs in an elevated (administrator) terminal, but a daemon started from one runs every unit with administrator rights. `doctor` warns when the daemon or the current terminal is elevated; `devlanes daemon restart` from a terminal that is not elevated fixes it. The CLI in an unelevated terminal can still talk to an elevated daemon of the same user.

Some terminals end every process they started when they close. `doctor` warns when the current one does, since a daemon started there stops with it; run `devlanes daemon start` from another terminal, such as Windows Terminal.

## Upgrading

`install.ps1` upgrades while the daemon runs. Windows refuses to overwrite a running executable, so the script renames it to `devlanes.exe.old-<timestamp>` and installs next to it. `devlanes daemon restart` (which stops services) switches to the new version, and `devlanes doctor` or the next install deletes the old file once nothing runs it.

cargo binstall, `cargo install`, npm, uv, pipx and pip cannot replace a running `devlanes.exe`. Run `devlanes daemon stop` first, which stops every unit, then the upgrade command. The update notice and `doctor` print the command for the way you installed.

## Coding agents

Claude Code on Windows often runs commands through its PowerShell tool, and Codex runs every shell command in PowerShell. The [agent plugin](../../agent-plugin/README.md#hook)'s hook runs `devlanes __agent-hook` and reads those commands as PowerShell, so it rejects backgrounded dev servers there too (`pnpm dev &`, `Start-Process`, `Start-Job`, `cmd /c start`). It needs `devlanes` on the agent's `PATH`; when devlanes is missing or too old, the error goes to the hook's stderr and every call passes.

## Limitations

- Each unit runs in a job object it cannot leave. A program that insists on starting a child outside its job fails with access denied there.
- When a unit's main process exits, everything left in its job is ended, for tasks too. A setup task cannot start a background daemon that outlives it; make that program a service.
- Windows cannot delete or rename a folder that a running process works in or holds a file open in. Run `devlanes stop` in a worktree before `git worktree remove` or `git worktree move`. `devlanes lane clean` leaves a lane data folder that another program still holds, with a warning in the daemon's log; delete it by hand afterwards.
- `pty: true` units stop with the daemon (see [terminals in units](#terminals-in-units)).
- The DNS server for `--resolver` always listens on `127.53.0.1:53`, because an NRPT rule cannot name a port.
- Unit names that differ only in case (`web` and `Web`) would share a log folder, so `devlanes config check` rejects them on Windows.
- Windows cannot be a remote box; boxes are Linux or macOS. Connecting to one from Windows works, with the differences in [Windows clients](remote-setup.md#windows-clients).

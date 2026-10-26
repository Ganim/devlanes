# Remote setup

A box runs its own devlanes and its own lanes. Your machine connects over SSH and gets the box's hostnames and ports locally, nothing else: lanes are started and stopped on the box, and the connection never carries your environment or secrets there. [Remote boxes](../remote.md) describes mirroring once connected; this page is about getting both machines ready.

## On the box

devlanes on the box is an ordinary install. It needs:

- Linux with glibc 2.35 or newer (Debian 12, Ubuntu 22.04 and later, Fedora), x86_64 or arm64; or macOS. Alpine and other musl systems have no release build.
- `git`, and Docker if the project has `infra`.
- The `devlanes` binary on the `PATH` that a non-interactive ssh session gets, or at `~/.devlanes/bin/devlanes`. `remote connect` finds it with `sh -c 'command -v devlanes'`, which does not read `.bashrc` or `.zshrc`.

Install the same release as on the client, from an ssh shell on the box. `install.sh` can put it in `~/.devlanes/bin`, where `remote connect` always looks; replace `X.Y.Z` with the version `devlanes --version` prints on the client:

```sh
curl -fsSL https://devlanes.run/install.sh | sh -s -- --version X.Y.Z --dir ~/.devlanes/bin
```

Without `--dir` it installs to `~/.local/bin`, which a non-interactive ssh session may not have on its `PATH`; check with `ssh hetzner 'command -v devlanes'` from the client. Homebrew, npm and cargo binstall work too (the [README](../../README.md) lists them). When their directory is not on that `PATH`, as with Homebrew's `/opt/homebrew/bin` on macOS, link the binary into `~/.devlanes/bin` from an interactive shell on the box:

```sh
mkdir -p ~/.devlanes/bin && ln -sf "$(command -v devlanes)" ~/.devlanes/bin/devlanes
```

`remote connect` never installs or upgrades devlanes on a box. Without a binary the first connection fails with `devlanes is not installed on hetzner` and stops retrying.

Then, on the box:

```sh
ssh hetzner
git clone git@github.com:acme/acme.git && cd acme
devlanes start                      # or devlanes start --no-ui
devlanes certs trust                # optional: for curl and browsers running on the box itself
```

Services on the box must listen on loopback or a wildcard address. The box dials `127.0.0.1:<port>` for the mirror, as the proxy does locally.

### Keep the box daemon alive

The daemon starts on the first command and detaches from the ssh session, so it survives logging out. What kills it is systemd's cleanup of your user session on Linux. Enable linger once:

```sh
sudo loginctl enable-linger "$USER"
```

Without it, `devlanes remote ls` on the client warns `linger off` and the lanes vanish when your last ssh session ends. `devlanes daemon install` on the box adds a systemd user unit that restarts the daemon at boot, which is useful on top of linger but does not replace it: a user unit is stopped with the session too when linger is off.

On a macOS box there is no linger. The daemon keeps running after the ssh session closes. `devlanes daemon install` writes a LaunchAgent for login; it needs a GUI login session to load, so run it in a session where you are logged in on the console as well.

A client that connects while no daemon runs on the box starts one there. To keep the box daemon under your own control, set this in the box's `~/.devlanes/config.yaml`:

```yaml
remote:
  start_daemon: false
```

A client that connects while the daemon is stopped then shows `the devlanes daemon is not running on hetzner` in `remote ls` and keeps retrying. It connects once you run `devlanes daemon start` on the box. A daemon started on demand like that exits about 30 seconds after the last client or TUI leaves while nothing runs, so run `devlanes daemon install` to keep it running, or set `daemon.exit_when_idle: false` in the same file. An invalid value under `remote` counts as `false`. The setting is read at each connection, so it needs no restart.

### PATH for Docker and tools

The box daemon inherits the environment of whatever started it. Over ssh that is the non-interactive `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin` on macOS, the sshd default on Linux), and Compose, `psql`, `aws` and the rest are looked up there. Homebrew's `/opt/homebrew/bin` is not in it. Either put additions in a file the non-interactive shell reads (`~/.zshenv` for zsh, `~/.ssh/environment` with `PermitUserEnvironment`, or `/etc/environment`), or start the daemon from an interactive shell once (`devlanes daemon restart`), or install it as a service with the right `PATH` in the unit.

### Hidden units

```yaml
services:
  admin:
    run: ./admin
    hidden: true
infra:
  hidden: [postgres]
```

Hidden units stay on the box: their ports are not mirrored and the box refuses streams to them. Everything else that is running or starting is mirrored.

## On the client

The client runs macOS, Linux or Windows (see [Windows clients](#windows-clients)).

- A `Host` entry in `~/.ssh/config`. devlanes runs the system `ssh`, so `ProxyJump`, `ProxyCommand`, certificates, `IdentityFile` and `User` apply. The name you give `remote connect` is that `Host`. Boxes on a tailnet or behind Cloudflare Zero Trust have their own pages: [Tailscale](remote-tailscale.md) and [Cloudflare](remote-cloudflare.md).
- Your key in an agent. The ssh master is a child of the local daemon and uses the agent the daemon was started with; a daemon spawned from your shell inherits that shell's `SSH_AUTH_SOCK`. If the agent socket changes (new login, 1Password restart), `devlanes daemon restart` picks up the new one.
- `devlanes certs trust`, once. The box's hostnames are served by your local proxy with your local CA.
- Optionally a different `ports.range` than the box in `~/.devlanes/config.yaml`, so lanes created on both machines before they first connected do not overlap. Overlapping ports are reported, not mirrored, and the local allocator avoids the box's ports from then on.

```sh
devlanes remote connect hetzner             # stays attached: shows state, relays ssh prompts
devlanes remote connect hetzner --background
devlanes remote ls                          # state, mirrored ports, skipped lanes, warnings
devlanes remote disconnect hetzner
```

Passphrase, one-time-password and host-key prompts appear in the terminal running `remote connect`, prefixed with the box name; type `yes` to a host-key question even though the input is hidden. A reconnect that needs a prompt while no `remote connect` is attached fails as `Permission denied`, so keys in an agent are what make unattended reconnects work.

The connection belongs to your local daemon, not to the terminal: Ctrl-C detaches, the daemon reconnects with backoff after sleep or network loss and keeps the mirrored ports bound meanwhile. A daemon restart drops connections; run `remote connect` again afterwards.

### Windows clients

devlanes uses Windows' own OpenSSH client (`C:\Windows\System32\OpenSSH\ssh.exe`, the "OpenSSH Client" optional feature), or the first `ssh.exe` on `PATH` when that is not installed. It reads `%USERPROFILE%\.ssh\config` and `known_hosts` like any `ssh` you run yourself.

Windows' OpenSSH has no ControlMaster, so every connection and every reconnect authenticates again. Keep the key in the ssh-agent service so reconnects need no prompt. Once, in an elevated PowerShell:

```powershell
Get-Service ssh-agent | Set-Service -StartupType Automatic
Start-Service ssh-agent
```

Then, as yourself, `ssh-add $env:USERPROFILE\.ssh\id_ed25519`. The agent keeps the key across logins, and the daemon finds it without `SSH_AUTH_SOCK`. Passphrase and host-key prompts still appear in the terminal running `remote connect` (PowerShell, cmd or Windows Terminal; Git Bash's mintty has no console to prompt in).

Synced folders land under `%USERPROFILE%\.devlanes\sync`. Windows keeps only a read-only attribute, so exec bits are not synced. Files whose names Windows cannot hold (`con.txt`, `a:b`, a name ending in a dot or space) and the second of two names that differ only in case (`README.md` and `Readme.md`) are skipped and listed in `remote sync`. A box port inside a Windows reserved port range (`netsh int ipv4 show excludedportrange protocol=tcp`) cannot be mirrored and is reported with that hint.

## Checking

```sh
devlanes remote ls                                  # connected, reconnecting (with the error), failed
devlanes url ls                                     # the box's hostnames next to your local ones
devlanes status --json | jq .clients                # on the box: who is connected
curl -sS https://api.acme.localhost/healthz          # from the client, once the box lane is ready
```

| Message | Cause | Fix |
| --- | --- | --- |
| `devlanes is not installed on hetzner` | not on the ssh `PATH` or in `~/.devlanes/bin` | install it on the box, or move the binary there, then connect again |
| `the devlanes daemon is not running on hetzner` | `remote.start_daemon: false` on the box and its daemon is stopped | `devlanes daemon start` on the box, or `devlanes daemon install` to keep it running; the client keeps retrying |
| `hetzner: a connection to port 20012 (acme@0:api) failed: the unit is not running on the box` | an `open_failed` warning; `remote ls` shows the same as the port's `last_error` | start the unit on the box |
| `… failed: nothing on the box accepted it: connection refused` | the unit runs but does not listen on the box's loopback | make it listen on `127.0.0.1`, `::1` or a wildcard address |
| `… failed: the box did not answer in time` or `the link dropped while it opened` | a slow or dropped link | try again; `remote ls` shows the link state |
| `… failed: box hetzner is not connected: …` | the link stayed down for 10 seconds | the error after the colon says why |
| `the box runs devlanes 0.4.1 and this machine 0.4.3` | different releases, same protocol | works; upgrade the box when convenient |
| `the box runs … protocol 3 but this client speaks protocol 4` | releases too far apart | install the matching release on the box, then `devlanes daemon restart` there |
| `` `devlanes tunnel --stdio` did not answer within 20s; the box may run an older devlanes release `` | the box's devlanes predates protocol 4, whose tunnel announces itself first | install the same release on both machines |
| ``the box daemon runs an older devlanes than `devlanes tunnel` `` | the box's binary was upgraded but its daemon still runs the old one | `devlanes daemon restart` on the box |
| ``the box closed the connection before `devlanes tunnel --stdio` started; the box printed: …`` | the login shell exited or ran something else; its output follows | fix the shell's startup files, or the `ForceCommand` or `RemoteCommand` for this host |
| ``the box printed more than 64 KiB before `devlanes tunnel --stdio` started`` | a profile file prints a lot on every login | make it quiet for non-interactive sessions |
| `port 5432 is in use on this machine by postgres (pid 812); not mirrored` | a local process holds the port | stop it, or mark the box container `hidden` |
| `lane acme@1 is not mirrored; this machine already serves acme-1.localhost` | a local lane of the same project uses that hostname | stop the local lane, or give one side another `project` name |
| `linger off` | Linux box without `loginctl enable-linger` | run it on the box |
| `ssh to hetzner failed (255): Permission denied` | no agent or key for the daemon's ssh | load the key, `devlanes daemon restart`, connect again |
| `no ssh client found` | Windows without OpenSSH Client | add the optional feature, or put `ssh.exe` on `PATH` |

## Limits

TCP only: UDP ports are not mirrored. The connection is client to box only; the box never dials you. Commands are not forwarded (`devlanes --host`), so manage lanes in an ssh shell or an editor's remote terminal on the box.

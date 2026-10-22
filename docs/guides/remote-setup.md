# Remote setup

A box runs its own devlanes and its own lanes. Your machine connects over SSH and gets the box's hostnames and ports locally, nothing else: lanes are started and stopped on the box, and the connection never carries your environment or secrets there. [Remote boxes](../remote.md) describes mirroring once connected; this page is about getting both machines ready.

## On the box

devlanes on the box is an ordinary install. It needs:

- Linux with glibc 2.35 or newer (Debian 12, Ubuntu 22.04 and later, Fedora), x86_64 or arm64; or macOS. Alpine and other musl systems have no release build.
- `git`, and Docker if the project has `infra`.
- The `devlanes` binary on the `PATH` that a non-interactive ssh session gets, or at `~/.devlanes/bin/devlanes`. `remote connect` finds it with `sh -c 'command -v devlanes'`, which does not read `.bashrc` or `.zshrc`.

The quickest way is from the client:

```sh
devlanes remote connect hetzner --install
```

With `--install`, a box without devlanes gets your release downloaded over HTTPS (`curl` or `wget` on the box), checked against the release's SHA-256 and placed at `~/.devlanes/bin/devlanes`. An existing binary is never replaced, and a running box daemon is never upgraded. Add `~/.devlanes/bin` to the box's `PATH` yourself so that `devlanes start` works in your ssh shell too.

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

### PATH for Docker and tools

The box daemon inherits the environment of whatever started it. Over ssh that is the non-interactive `PATH` (`/usr/bin:/bin:/usr/sbin:/sbin` on macOS, the sshd default on Linux), and Compose, `psql`, `aws` and the rest are looked up there. Homebrew's `/opt/homebrew/bin` is not in it. Either put additions in a file the non-interactive shell reads (`~/.zshenv` for zsh, `~/.ssh/environment` with `PermitUserEnvironment`, or `/etc/environment`), or start the daemon from an interactive shell once (`devlanes daemon restart`), or install it as a service with the right `PATH` in the unit.

### Hidden units

```yaml
services:
  admin: { run: ./admin, hidden: true }
infra:
  hidden: [postgres]
```

Hidden units stay on the box: their ports are not mirrored and the box refuses streams to them. Everything else that is running or starting is mirrored.

## On the client

- A `Host` entry in `~/.ssh/config`. devlanes runs the system `ssh`, so `ProxyJump`, certificates, `IdentityFile` and `User` apply. The name you give `remote connect` is that `Host`.
- Your key in an agent. The ssh master is a child of the local daemon and uses the agent the daemon was started with; a daemon spawned from your shell inherits that shell's `SSH_AUTH_SOCK`. If the agent socket changes (new login, 1Password restart), `devlanes daemon restart` picks up the new one.
- `devlanes certs trust`, once. The box's hostnames are served by your local proxy with your local CA.
- Optionally a different `ports.range` than the box in `~/.devlanes/config.yaml`, so lanes created on both machines before they first connected do not overlap. Overlapping ports are reported, not mirrored, and the local allocator avoids the box's ports from then on.

### Windows clients

- The OpenSSH Client (Settings, System, Optional features, or `Add-WindowsCapability -Online -Name OpenSSH.Client~~~~0.0.1.0` in an administrator PowerShell). devlanes runs `%SystemRoot%\System32\OpenSSH\ssh.exe`, else the first `ssh.exe` on `PATH`. It reads `%USERPROFILE%\.ssh\config` and uses the Windows `ssh-agent` service (`Get-Service ssh-agent`, then `ssh-add`).
- There is no ControlMaster: each connection and each reconnect is a new ssh login. With a key in the agent that costs nothing; with a passphrase typed at the prompt, every reconnect while `remote connect` is not attached fails as `Permission denied`.
- Prompts work as on macOS and Linux: ssh runs `devlanes.exe` as its askpass program, and the question appears in the terminal running `remote connect`.
- Synced folders keep only the read-only attribute. Box files Windows cannot name, and files that differ from another only in letter case, are skipped and listed by `devlanes remote sync`.
- Box ports inside the ranges Windows reserves (`netsh int ipv4 show excludedportrange protocol=tcp`) are not mirrored; set the box's `ports.range` clear of them.

```sh
devlanes remote connect hetzner             # stays attached: shows state, relays ssh prompts
devlanes remote connect hetzner --background
devlanes remote ls                          # state, mirrored ports, skipped lanes, warnings
devlanes remote disconnect hetzner
```

Passphrase, one-time-password and host-key prompts appear in the terminal running `remote connect`, prefixed with the box name; type `yes` to a host-key question even though the input is hidden. A reconnect that needs a prompt while no `remote connect` is attached fails as `Permission denied`, so keys in an agent are what make unattended reconnects work.

The connection belongs to your local daemon, not to the terminal: Ctrl-C detaches, the daemon reconnects with backoff after sleep or network loss and keeps the mirrored ports bound meanwhile. A daemon restart drops connections; run `remote connect` again afterwards.

## Checking

```sh
devlanes remote ls                                  # connected, reconnecting (with the error), failed
devlanes url ls                                     # the box's hostnames next to your local ones
devlanes status --json | jq .clients                # on the box: who is connected
curl -sS https://api.acme.localhost/healthz          # from the client, once the box lane is ready
```

| Message | Cause | Fix |
| --- | --- | --- |
| `devlanes is not installed on hetzner` | not on the ssh `PATH` or in `~/.devlanes/bin` | `--install`, or move the binary |
| `the box runs devlanes 0.4.1 and this machine 0.4.3` | different releases, same protocol | works; upgrade the box when convenient |
| `the box runs … protocol 2 but this client speaks protocol 3` | releases too far apart | install the matching release on the box, then `devlanes daemon restart` there |
| `port 5432 is in use on this machine by postgres (pid 812); not mirrored` | a local process holds the port | stop it, or mark the box container `hidden` |
| `lane acme@1 is not mirrored; this machine already serves acme-1.localhost` | a local lane of the same project uses that hostname | stop the local lane, or give one side another `project` name |
| `linger off` | Linux box without `loginctl enable-linger` | run it on the box |
| `ssh to hetzner failed (255): Permission denied` | no agent or key for the daemon's ssh | load the key, `devlanes daemon restart`, connect again |

## Limits

TCP only: UDP ports are not mirrored. The connection is client to box only; the box never dials you. A box runs macOS or Linux; Windows machines connect as clients only. Commands are not forwarded (`devlanes --host`), so manage lanes in an ssh shell or an editor's remote terminal on the box. A relay transport that needs no inbound SSH is planned.

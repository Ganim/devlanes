# Windows (WSL2)

devlanes runs inside a WSL 2 distro the same way it runs on Linux: the daemon, its units, the proxy and the CA live in the distro. The browser usually runs on Windows, and that is where most of the differences come from. Everything on this page is detected at run time; the Linux build has no separate WSL mode.

## Install

Install the Linux build inside a glibc distro such as Ubuntu or Debian, for example with `install.sh` from the [README](../../README.md#install); it refuses musl distros such as Alpine. Keep your repositories on the Linux filesystem (`~/src`), not under `/mnt/c` (see [worktrees](#worktrees-belong-on-the-linux-filesystem)).

Run `devlanes doctor` once after installing. Inside WSL it adds a `wsl` group that checks interop, systemd, whether `tls.trust` includes `windows`, and worktrees on Windows drives.

## Keep the daemon running

The daemon starts on the first command, as everywhere. To start it with the distro instead, `devlanes daemon install` writes a systemd user unit, which needs systemd in the distro. Without it, `daemon install` stops before changing anything and says so. To turn systemd on, add this to `/etc/wsl.conf` inside the distro, then run `wsl.exe --shutdown` from Windows and open the distro again:

```ini
[boot]
systemd=true
```

WSL also stops a distro that has been idle for 15 seconds (`instanceIdleTimeout` under `[general]` in `%UserProfile%\.wslconfig`), and with it the daemon and every lane. Set it to `-1` to keep the distro running after the last terminal closes:

```ini
[general]
instanceIdleTimeout=-1
```

## Ports 443 and 80

A normal user cannot bind ports below 1024 in the distro, so the proxy falls back to 8080 for HTTP and 8443 for HTTPS, and URLs carry the port (`https://web.acme-2.localhost:8443`). To get 80 and 443:

```sh
sudo sysctl -w net.ipv4.ip_unprivileged_port_start=80
echo net.ipv4.ip_unprivileged_port_start=80 | sudo tee /etc/sysctl.d/50-devlanes.conf
devlanes daemon restart
```

## Reaching the proxy from Windows

Chrome, Edge and Firefox on Windows answer `*.localhost` with loopback themselves, and WSL forwards loopback connections from Windows into the distro. In the default NAT networking mode this works for both `127.0.0.1` and `::1`, because the proxy listens on each of them (checked with WSL 2.7). Mirrored networking (`networkingMode=mirrored` under `[wsl2]` in `.wslconfig`, Windows 11 22H2 or later) shares localhost both ways and is the mode in which `proxy.lan: true` lets other machines in. `wslinfo --networking-mode` prints the active mode.

Windows programs that resolve names through Windows, such as Node or Python installed on Windows, do not know `*.localhost` subdomains, and Windows never reads the distro's `/etc/hosts`. Call those services from inside the distro, or by `127.0.0.1:<port>`.

## Trusting the CA in Windows

The `windows` trust target puts the CA into your Windows user root store, the one Edge, Chrome and Firefox read (Firefox 120 and later read it through `security.enterprise_roots.enabled`). Inside WSL with interop on, an unset `tls.trust` means `[system, windows]`, so

```sh
devlanes certs trust
```

trusts the CA in the distro and in Windows. It runs Windows' `certutil.exe -user -addstore Root` through WSL interop, and Windows shows a "Security Warning" dialog on the desktop that you confirm with Yes. A No is reported as declined and changes nothing. `devlanes certs untrust` removes the CA again (Windows asks once more), and `doctor` shows `CA trusted by windows (Windows user root store)` when it is in place. A CA an administrator added to the machine's root store counts as trusted too; `untrust` cannot remove that copy.

With `tls.trust` set explicitly, add `windows` to it; `doctor` warns when Windows browsers are left out:

```yaml
# ~/.devlanes/config.yaml
tls:
  trust: [system, chrome, windows]
```

## URLs and the tab progress bar

`devlanes url open` and the TUI's `o` open the Windows default browser through Windows' `rundll32.exe url.dll,FileProtocolHandler`, so neither needs a Linux browser. Without interop, or when no mounted Windows drive holds `System32`, they run `xdg-open`.

In Windows Terminal the tab progress bar works by itself, because Windows Terminal adds `WT_SESSION` to `WSLENV` and the variable reaches the distro. A shell started another way (VS Code's terminal, `wsl.exe` from another console) has no `WT_SESSION`; set `terminal.progress: always` in the user config there.

## Worktrees belong on the Linux filesystem

Windows drives are mounted under `/mnt/<letter>` through a file server: file access is slower than on the Linux filesystem, and inotify does not see edits that Windows programs make, so devlanes notices new or removed worktrees only at its periodic rescan, and `sync` to a remote box misses those edits. A start in such a worktree warns, and `doctor` lists every lane on a Windows drive. Move the repository to the Linux filesystem and open it from Windows editors through `\\wsl.localhost\<distro>\home\<you>\src`, or with VS Code's WSL extension (`code .` from the distro).

## Docker Desktop

Turn on Docker Desktop's WSL integration for the distro (Settings, Resources, WSL integration). That puts `docker` and the Docker socket into the distro, and Compose infra works as on Linux. Without it `doctor` reports Docker as not reachable.

## /etc/hosts

The `/etc/hosts` fallback writes the distro's file, which WSL rewrites when the distro starts (`generateHosts`, on by default). The daemon writes its names again when it starts, so after a restart of WSL they are back once the daemon runs. Windows programs never read this file.

## Limitations

- `certs trust` and `untrust` for `windows` need someone at the Windows desktop to answer the dialog. After 5 minutes without an answer the command stops and says a dialog may be waiting; `doctor` gives up on a Windows store lookup after 20 seconds and reports the store as unknown.
- With interop off (`[interop] enabled=false` in `/etc/wsl.conf`), the `windows` target and opening URLs in Windows are unavailable; `doctor` says so.

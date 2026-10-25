# WSL

devlanes runs inside a WSL distro the same way it runs on Linux: the daemon, its units, the proxy and the CA live in the distro. The browser usually runs on Windows, and most of the differences on this page come from that. The Linux build detects WSL at run time (from the kernel release, WSL's interop entry in `/proc/sys/fs/binfmt_misc` and `WSL_DISTRO_NAME`); there is no separate WSL build or setting. To run devlanes on Windows itself instead, see [Windows](windows.md).

## Install

Install the Linux build inside a glibc distro such as Ubuntu or Debian, for example with `install.sh` from the [README](../../README.md#install). Keep your repositories on the Linux filesystem (`~/src`), not under `/mnt/c` (see [worktrees](#worktrees-on-windows-drives)).

Run `devlanes doctor` once after installing. Inside WSL it adds a `wsl` group with the distro, the WSL version and whether Windows interop is on, and warns about the things below: interop off, systemd off, a `tls.trust` without `windows`, and lanes on Windows drives.

## Keep the daemon running

The daemon starts on the first command, as everywhere. To start it with the distro instead, `devlanes daemon install` writes a systemd user unit, which needs systemd in the distro. Without systemd, `daemon install` stops before writing anything and says so. To turn systemd on, add this to `/etc/wsl.conf` inside the distro, then run `wsl.exe --shutdown` from Windows and open the distro again:

```ini
[boot]
systemd=true
```

WSL stops a distro some seconds after its last terminal closes, and with it the daemon and every lane. `instanceIdleTimeout` under `[general]` in `%UserProfile%\.wslconfig` controls that; `-1` keeps the distro running.

## Ports 443 and 80

A normal user cannot bind ports below 1024 in the distro, so the proxy falls back to 8443 for HTTPS and 8080 for HTTP, and URLs carry the port (`https://web.acme-2.localhost:8443`). [Ports 443 and 80](https.md#ports-443-and-80) shows how to allow them.

## Reaching the proxy from Windows

Chrome, Edge and Firefox on Windows resolve `*.localhost` to loopback themselves, and WSL forwards loopback connections from Windows into the distro, so `https://web.acme-2.localhost:8443` opened on Windows reaches the proxy in the distro. Windows programs that resolve names through Windows itself never read the distro's `/etc/hosts`, so the [hosts file fallback](https.md#programs-that-cannot-resolve-localhost) does not help them; call those services by `127.0.0.1:<port>` instead.

## Trusting the CA in Windows

Browsers on Windows read the Windows certificate store, not the distro's. The `windows` trust target puts the CA into your Windows user root store, which Edge and Chrome read, and Firefox too when `security.enterprise_roots.enabled` is on (the default since Firefox 120). It is opt-in, so add it to `tls.trust`:

```yaml
# ~/.devlanes/config.yaml
tls:
  trust: [system, windows]
```

Then run:

```sh
devlanes certs trust
```

For `windows` this runs Windows' `certutil.exe -user -addstore Root` through WSL interop, and Windows shows a security warning on the desktop that you confirm with Yes. A No fails the command with `ERROR_CANCELLED` and changes nothing. `devlanes certs untrust` removes the CA again (Windows asks once more). `doctor` reads the store with `certutil.exe -user -verifystore Root`, which shows no dialog, and reports `CA trusted by windows` once the CA is there. Without `windows` in `tls.trust`, `doctor` warns that browsers on Windows do not trust the CA.

With interop off, and on macOS and Linux outside WSL, the `windows` target is reported as unavailable. On Windows itself it is the same store as `system` ([Windows](windows.md#https)).

## Opening URLs

`devlanes url open` and the TUI's `o` open the Windows default browser: with `wslview` (from wslu) when it is on `PATH`, otherwise with Windows' `rundll32.exe url.dll,FileProtocolHandler` from the `System32` of a mounted Windows drive (`/mnt/c/Windows/System32` when none is found). With interop off they run `xdg-open`, as on Linux.

## Worktrees on Windows drives

Windows drives are mounted under `/mnt/<letter>` through a file server. File access there is slower than on the Linux filesystem, and inotify does not see edits Windows programs make, so devlanes' Git watcher, `sync` to a remote box and file-watching dev servers miss them. `devlanes start` in such a worktree warns (`windows_drive`), and `doctor` lists every lane on a Windows drive. Move the repository to the Linux filesystem and open it from Windows editors through `\\wsl.localhost\<distro>\home\<you>\src`, or with VS Code's WSL extension (`code .` from the distro).

## Docker Desktop

Turn on Docker Desktop's WSL integration for the distro (Settings, Resources, WSL integration). That puts `docker` and the Docker socket into the distro, and Compose infra works as on Linux. Without it `doctor` reports Docker as not reachable.

## Limitations

- `certs trust` and `certs untrust` for `windows` wait for someone at the Windows desktop to answer the dialog. `doctor` gives up on a store lookup after 20 seconds and reports the state as unknown.
- With interop off (`[interop] enabled=false` in `/etc/wsl.conf`), the `windows` target and opening URLs in Windows are unavailable; `doctor` says so.
- A daemon started by systemd has no `WSL_DISTRO_NAME`. It still detects WSL through the interop entry, but with interop off it does not, and then neither starts nor `doctor` warn about lanes on Windows drives.

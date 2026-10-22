# WSL: the Linux build inside a WSL 2 distro

The Linux build detects WSL at run time and works with the Windows side: `devlanes url open` and the TUI's `o` open the Windows browser, the `windows` trust target puts the CA into the Windows user root store (the default inside WSL together with `system`), the TUI's `O` opens a Windows Terminal tab in the same distro, `doctor` gains a `wsl` group, a start in a worktree under `/mnt/<letter>` warns, and `daemon install` without systemd stops with the `wsl.conf` fix. [Windows (WSL2)](../guides/wsl.md) is the user guide.

## Try it

In the distro, with the Linux binary on `PATH` and an isolated home:

```sh
export DEVLANES_HOME=$(mktemp -d)
devlanes doctor                  # wsl group: distro, interop, networking, ports
mkdir -p /mnt/c/tmp/acme && cd /mnt/c/tmp/acme
git init -q && printf 'project: acme\nservices:\n  idle:\n    run: [sleep, "300"]\n    ready: started\n' > devlanes.yaml
git add . && git commit -qm init
devlanes start                   # warns that the worktree is on the Windows drive C:\
devlanes doctor                  # lists the lane under wsl, and the windows trust store
devlanes certs trust             # confirm the Windows "Security Warning" dialog
devlanes stop --yes && devlanes daemon stop
```

Then open `https://<host>.localhost:8443` in Edge on Windows; it trusts the certificate. `devlanes certs untrust` removes the CA from Windows again.

## Checked

- WSL 2.7.13, Ubuntu 26.04 on Windows 11 build 26300: detection, `doctor` with and without a daemon, the start warning and `windows_drive_worktrees` for a lane on `/mnt/d`, and `windows_scratch_store_round_trip` (`certutil.exe` add, read and delete in a scratch CurrentUser store through interop, store removed afterwards).
- From Windows in NAT mode, `curl.exe` reaches a distro listener bound to `127.0.0.1` through `127.0.0.1` and one bound to `::1` through `::1`; the proxy binds both.
- `WT_SESSION` and `WT_PROFILE_ID` reach the distro from a Windows Terminal tab through `WSLENV`.
- Not run here: the real `Root` dialog, Edge trusting the proxy's certificate, `wslview`, and opening a Windows Terminal tab.

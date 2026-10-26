# Remote boxes over Tailscale

To devlanes, a box on your tailnet is an ordinary SSH host. `devlanes remote connect` runs the system `ssh` from your local daemon, so a MagicDNS name, a Tailscale IP or a `Host` entry in `~/.ssh/config` all work. Tailscale carries the packets and, if you want, handles SSH authentication. Prepare the box as in [remote setup](remote-setup.md); this page covers what Tailscale changes. [Remote boxes](../remote.md) describes mirroring once connected.

## Names

With [MagicDNS](https://tailscale.com/docs/features/magicdns) on (the default for tailnets created since October 2022), every device has a short name such as `devbox` and a full name such as `devbox.example.ts.net`. The short name relies on the search domain Tailscale adds to the client's DNS settings; the full name and the Tailscale IP (`100.x.y.z`) do not. A `Host` entry keeps the name you type short and puts the account in one place:

```text
Host devbox
  HostName devbox.example.ts.net
  User acme
```

```sh
devlanes remote connect devbox
```

A device keeps its Tailscale IP when it changes networks, so after sleep or a move to another Wi-Fi the daemon reconnects to the same name and address.

## OpenSSH keys over the tailnet

The simplest setup leaves the box's own `sshd` in charge: Tailscale carries the connection and your key authenticates it, as on any other network. The tailnet policy only has to let the client reach TCP port 22 on the box. The default policy allows all traffic between your devices; with your own policy, one grant is enough (`tag:devbox` stands for however you select the box, for example a tag or `autogroup:self`):

```json
{
  "grants": [
    { "src": ["autogroup:member"], "dst": ["tag:devbox"], "ip": ["tcp:22"] }
  ]
}
```

Mirrored ports, hostnames and synced folders all travel inside that one SSH connection, so no other port needs a grant. Unattended reconnects need the key in an agent, as in [remote setup](remote-setup.md#on-the-client). A macOS box serves SSH through Remote Login (System Settings, General, Sharing).

## Tailscale SSH

With [Tailscale SSH](https://tailscale.com/docs/features/tailscale-ssh), `tailscaled` answers port 22 on the box's Tailscale address and authenticates the connection by your tailnet identity. No key, agent or `authorized_keys` entry is involved. The server side runs on Linux, and on macOS only with the open-source `tailscaled`, not the App Store or standalone app. Turn it on, on the box:

```sh
tailscale set --ssh
```

SSH connections to the box's Tailscale address that are open at that moment hang. devlanes' keepalives end its connection within about 45 seconds and the daemon reconnects. Once Tailscale SSH is on, it takes every connection to port 22 on the Tailscale address; `sshd` still serves the box's other addresses.

Tailscale SSH needs an `ssh` rule in the policy as well as network access to port 22:

```json
{
  "grants": [
    { "src": ["autogroup:member"], "dst": ["tag:devbox"], "ip": ["tcp:22"] }
  ],
  "ssh": [
    {
      "action": "accept",
      "src": ["autogroup:member"],
      "dst": ["tag:devbox"],
      "users": ["acme"]
    }
  ]
}
```

`users` lists the accounts on the box you may log in as, and the `User` in your `Host` entry must be one of them. With `accept`, devlanes needs nothing else: connects and reconnects authenticate without a prompt, with or without anyone at the terminal.

Restarting or upgrading `tailscaled` on the box ends every Tailscale SSH session, and so does a policy change that removes your access. The daemon reconnects afterwards. When access is gone, the box waits in `needs_auth` with `Permission denied` until you run `devlanes remote connect` again.

On Linux, Tailscale SSH runs commands through `su -l` when it can, which starts a login shell; OpenSSH runs them in a plain non-interactive shell. The box then reads `~/.profile` (or your shell's login files) for devlanes' commands too, so the `PATH` that finds `devlanes`, and that a box daemon started over this connection inherits, comes from those files. Keep them from printing anything, since devlanes reads the command's output.

### Check mode

An `ssh` rule with `"action": "check"` asks you to sign in again in a browser before a connection is allowed, and accepts further connections from that device for the rule's `checkPeriod` (12 hours by default; `always` asks every time). The default policy of a new tailnet uses `check` for SSH to your own devices. When a check rule and an accept rule both match a connection, the check rule wins, so adding an `accept` rule next to the default one changes nothing; edit or narrow the default rule instead.

Tailscale sends the sign-in URL as an SSH banner while the connection authenticates, and OpenSSH prints banners to its standard error. devlanes does not pass ssh's standard error to your terminal; it relays only the prompts ssh asks through askpass, so `remote connect` never shows the URL. The attempt waits for a sign-in that nobody can complete. On macOS and Linux devlanes gives up on it after 120 seconds and the daemon tries again with backoff, each time with a new URL.

Either use `accept` for the box and the account devlanes logs in as (the rule above), or pass the check yourself first:

```sh
ssh devbox true                   # prints the sign-in URL; open it and sign in
devlanes remote connect devbox    # within the check period
```

Reconnects within the check period go through. A reconnect after it has ended waits again until you repeat both steps. `"checkPeriod": "always"` cannot work with devlanes.

## Windows clients

With Tailscale for Windows running, `ssh.exe` reaches MagicDNS names and Tailscale IPs like any other program. Windows' OpenSSH has no ControlMaster, so every connection and every reconnect authenticates again ([Windows clients](remote-setup.md#windows-clients)). With OpenSSH keys, keep the key in the ssh-agent service. With Tailscale SSH and `accept` there is nothing to load. With `check`, pass the check from a terminal first as above; `remote connect` does not show the URL on Windows either.

## Headscale

[Headscale](https://headscale.net) runs the same Tailscale clients against a self-hosted control server, and plain OpenSSH over the tailnet works the same way. MagicDNS names come from `dns.magic_dns` and `dns.base_domain` in Headscale's configuration, so the full name is `devbox.<base_domain>`. Headscale accepts `ssh` rules in its policy file. It supports `check` from 0.29, with approval through OIDC or `headscale auth approve`. Earlier releases treated `checkPeriod` as a session length and closed the session when it ran out, which devlanes sees as a dropped connection. Use `accept` with Headscale unless you run 0.29 or later and pass the check as described above.

## No Serve or Funnel

devlanes exposes nothing on the tailnet, so `tailscale serve`, Funnel and binding services to the box's Tailscale address are not needed. Services listen on the box's loopback, the client binds the same port numbers on its own loopback, and the box's hostnames go through your local HTTPS proxy with your local CA (`devlanes certs trust`). URLs stay `https://<name>.localhost`, the origins the box's services were configured with, and browsers treat them as secure contexts.

## Checking

```sh
tailscale status                                 # both machines online in the tailnet
ssh devbox true                                  # same name, same account as devlanes uses
devlanes remote ls                               # connected, reconnecting or needs_auth (with the error), failed
devlanes status --json | jq .clients             # on the box: who is connected
```

| Message or symptom | Cause | Fix |
| --- | --- | --- |
| `Could not resolve hostname devbox` | the client does not use Tailscale DNS (MagicDNS off, or `tailscale set --accept-dns=false`) | put the full name or the Tailscale IP in `HostName`, then connect again |
| `remote connect` waits and never connects; the box uses Tailscale SSH | a `check` rule; the sign-in URL is not shown | `accept` for that account and box, or `ssh devbox true` first |
| `Permission denied` with Tailscale SSH | no `ssh` rule allows this user, account and box | `ssh devbox true` prints Tailscale's reason; fix the policy, connect again |
| `Permission denied (publickey)` with OpenSSH | no key in the agent the daemon uses | load the key, `devlanes daemon restart`, connect again |
| stays `reconnecting` with a timeout | the box is offline in the tailnet, or the policy does not allow port 22 | `tailscale status`, `tailscale ping devbox`, check the grant |
| `REMOTE HOST IDENTIFICATION HAS CHANGED` | the name now belongs to a reinstalled or different machine | confirm it is your box, `ssh-keygen -R devbox.example.ts.net`, connect again |

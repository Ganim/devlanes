# Remote boxes through Cloudflare

devlanes needs only a working `ssh` to the box. Ports, hostnames and synced folders all travel over one SSH session, the box's services stay on its loopback, and the client binds the same port numbers on its own loopback. The box therefore needs one route for SSH and no public hostname per service. This page covers reaching the box's sshd through [Cloudflare Tunnel](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/get-started/create-remote-tunnel/), either with `cloudflared` as a `ProxyCommand` behind a Cloudflare Access application, or with the Cloudflare One Client (formerly WARP) on a private network route. [Remote setup](remote-setup.md) covers the rest of preparing both machines.

## On the box

Install `cloudflared` on the box and create a tunnel. For the `ProxyCommand` setup, add a published application route on a hostname in your Cloudflare zone, such as `ssh.example.com`, with the service type SSH and `localhost:22` ([Cloudflare's SSH guide](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/use-cases/ssh/ssh-cloudflared-authentication/)). A locally managed tunnel does the same in its [configuration file](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/do-more-with-tunnels/local-management/configuration-file/):

```yaml
tunnel: <tunnel-uuid>
credentials-file: /etc/cloudflared/<tunnel-uuid>.json
ingress:
  - hostname: ssh.example.com
    service: ssh://localhost:22
  - service: http_status:404
```

Protect the hostname with a [self-hosted Access application](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/self-hosted-public-app/) and a policy that allows the people who connect. Cloudflare lists this step as recommended; without it, sshd's own authentication is the only check. Run the tunnel as a service so it comes back after a reboot.

sshd keeps its usual configuration. `AllowTcpForwarding no` is fine, because devlanes opens only session channels. Once SSH through the tunnel works, port 22 can be closed to the internet. devlanes itself is installed and kept alive as in [remote setup](remote-setup.md#on-the-box).

## On the client

Install `cloudflared` on the client too, and add a `Host` entry that uses it:

```sshconfig
Host acme-box
  HostName ssh.example.com
  User acme
  ProxyCommand /usr/local/bin/cloudflared access ssh --hostname %h
```

`%h` is replaced with the `HostName`. Write the full path to `cloudflared`: the `ssh` that devlanes runs is a child of the local daemon and sees the daemon's `PATH`, which may not include Homebrew's `/opt/homebrew/bin`. `cloudflared access ssh-config --hostname ssh.example.com` prints a block with the path of the `cloudflared` you ran it from. Then:

```sh
ssh acme-box true                   # in a terminal first; see below
devlanes remote connect acme-box
```

### Log in before connecting

`cloudflared access ssh` needs an Access token for the hostname. Without one it starts a login: it tries to open a browser, prints the login URL on its stderr and waits until you finish. That works in a terminal but not under devlanes. The `ProxyCommand` runs inside the background daemon with stderr discarded, so the URL never reaches the terminal running `remote connect`, and whether a browser opens depends on the OS and the session the daemon runs in. devlanes relays only the prompts ssh itself asks (passphrase, one-time password, host key), not cloudflared's login. ssh also gives up when the box has not answered within 15 seconds, which is rarely enough to finish a login even when the browser does open.

Get the token in a terminal first. `ssh acme-box true` logs in to Access when needed and also checks the host key and your key. `cloudflared access login https://ssh.example.com` gets the token without ssh. cloudflared stores tokens under `~/.cloudflared` (`%USERPROFILE%\.cloudflared` on Windows) and every later run reuses them, including the runs devlanes starts.

### Session expiry

The token lasts for the session duration set on the Access application or policy, 24 hours by default. When it expires, cloudflared gets a new one without a browser as long as your Access global session is still valid; once that has expired too, the next run needs the browser login again ([session management](https://developers.cloudflare.com/cloudflare-one/access-controls/access-settings/session-management/)).

An expired token does not end a connection that is already up, since Access decides when an SSH connection may start, not how long it lasts. It matters when devlanes connects again: after sleep, a network change or a dropped connection, after a daemon restart, and on Windows on every reconnect. Cloudflare also notes that long-lived `cloudflared` connections can close unexpectedly, so expect the odd reconnect. A reconnect without a valid token fails and is retried with backoff, and every retry runs cloudflared again, which may open another login page each time.

To recover, log in again in a terminal with `ssh acme-box true`, then run `devlanes remote connect acme-box`. `devlanes remote disconnect acme-box` stops the retries in the meantime. A longer session duration for this application makes this rarer. A client that must reconnect with nobody at a browser can use an Access [service token](https://developers.cloudflare.com/cloudflare-one/access-controls/service-credentials/service-tokens/) with a Service Auth policy, or the Cloudflare One Client below.

### Windows clients

Win32-OpenSSH runs the `ProxyCommand` as well. Write the full path to `cloudflared.exe` (`where.exe cloudflared` prints it), in double quotes when it contains spaces:

```sshconfig
Host acme-box
  HostName ssh.example.com
  User acme
  ProxyCommand "C:\tools\cloudflared\cloudflared.exe" access ssh --hostname %h
```

Windows' OpenSSH has no ControlMaster, so every connect and every reconnect starts a new `ssh.exe` and a new cloudflared, and each one needs a valid token. Both run in a job with no window and end with the connection, so a login started there cannot show its URL either. Run `ssh acme-box true` in PowerShell or Windows Terminal before connecting, and again after the session expires.

## Cloudflare One Client instead of a ProxyCommand

With the [Cloudflare One Client](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/use-cases/ssh/ssh-device-client/) enrolled in your Zero Trust organization, the box is reachable at a private IP or private hostname and `ssh` connects to it directly. On the box, the tunnel gets a CIDR route for the box's address or a private hostname route instead of a published application. On the client, Split Tunnels must send that address through the client. Gateway network policies decide who may connect, and sshd checks your usual keys.

```sshconfig
Host acme-box
  HostName 10.0.0.12
  User acme
```

There is no per-connection Access login, so reconnects after sleep or a network change need nobody at the keyboard as long as the client is connected. Connections fail while it is disconnected.

[Access for Infrastructure](https://developers.cloudflare.com/cloudflare-one/networks/connectors/cloudflare-tunnel/use-cases/ssh/ssh-infrastructure-access/) builds on the same route and adds per-user short-lived certificates signed by a Cloudflare SSH CA that the box trusts. It does not support port forwarding, `ProxyJump` or agent forwarding. devlanes needs none of them, but a `ProxyJump` in the `Host` entry does not work there. An infrastructure application closes connections when its session duration ends (24 hours by default), and Cloudflare guarantees Gateway connections for 10 hours; devlanes reconnects after either.

## Short-lived certificates

devlanes runs the system `ssh`, so certificates work as they do in a terminal. With Access for Infrastructure nothing changes on the client. For a published application, Cloudflare's older [short-lived certificates](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/non-http/short-lived-certificates-legacy/) are set up with:

```sh
cloudflared access ssh-config --hostname ssh.example.com --short-lived-cert
```

It prints a `Match host ssh.example.com exec "... access ssh-gen --hostname %h"` block. Before each connection ssh runs `ssh-gen`, which needs a valid Access token like `access ssh` does and writes a new certificate to `~/.cloudflared`. Since the block matches the hostname, connect with `devlanes remote connect ssh.example.com`, or adapt the block to your alias. On Windows, check the block with `ssh ssh.example.com true` before using it with devlanes.

sshd checks a certificate only when ssh authenticates. On macOS and Linux the devlanes connection stays up after its certificate expires, and a new certificate is needed only when it connects again; on Windows every reconnect authenticates. With `ssh-gen` in the `Match exec` line that happens by itself while the token is valid. Certificates you renew some other way must be renewed before they expire: a reconnect with an expired certificate fails with `Permission denied`, and the box waits in `needs_auth` until you renew the certificate and run `devlanes remote connect` again.

## Checking

```sh
ssh acme-box true                     # Access login, host key and key, in a terminal
devlanes remote connect acme-box
devlanes remote ls                    # state; the ssh error while reconnecting
devlanes status --json | jq .clients  # on the box: who is connected
```

| Message or symptom | Cause | Fix |
| --- | --- | --- |
| `remote connect` keeps reconnecting with `Connection timed out during banner exchange` or `Connection closed` | no valid Access token, so cloudflared is waiting for a login nobody sees | `ssh acme-box true` in a terminal, then `remote connect` again |
| an Access login page opens on its own | a reconnect ran cloudflared after the session expired | finish the login, or `remote disconnect`, `ssh acme-box true`, connect again |
| `cloudflared: not found` or `command not found` | the `ProxyCommand` has no full path and the daemon's `PATH` lacks cloudflared | write the full path in `~/.ssh/config` |
| `Permission denied` | Access let you through but sshd refused the key or certificate; devlanes stops retrying | fix the key or renew the certificate, `ssh acme-box true`, connect again |
| `Host key verification failed` | the box's key is not in `known_hosts` under this name | answer the prompt in the `remote connect` terminal, or run `ssh acme-box true` |
| connection drops after 10 or 24 hours | Gateway's connection limit or the infrastructure application's session duration | nothing to do; devlanes reconnects |
| timeouts with the Cloudflare One Client | client disconnected, or Split Tunnels do not include the box's address | connect the client; check the device profile's Split Tunnels |

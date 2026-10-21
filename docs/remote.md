# Remote boxes

A box is any machine you can reach with `ssh` that runs devlanes for its own lanes. Another machine connects to it and gets the box's lanes locally: the same hostnames, the same port numbers and the synced folders.

## Connect

```sh
devlanes remote connect hetzner              # any ~/.ssh/config host; streams connection state
devlanes remote connect hetzner --background # returns once connected; the connection stays up
devlanes remote ls                           # boxes, state, mirrored ports, skipped lanes, syncs
devlanes remote sync                         # synced folders of running box lanes
devlanes remote disconnect hetzner           # or `devlanes remote disconnect` for every box
```

The connection belongs to your local daemon, not to the terminal. Ctrl-C on `remote connect` only detaches the CLI. The daemon reconnects with backoff after network loss or sleep. While it is down, mirrored ports stay bound and the proxy answers the box's hostnames with a "Reconnecting" page. After a reconnect the box's current state replaces what the client knew.

devlanes uses the system `ssh` with one ControlMaster per box, so `~/.ssh/config`, agents, ProxyJump and certificates apply. Passphrase, OTP and host-key prompts appear in the terminal that runs `remote connect`. Over that master it runs `devlanes tunnel --stdio` on the box, which needs only a session channel: `AllowTcpForwarding no` on the box is fine. devlanes must be on the box's `PATH` or in `~/.devlanes/bin`. It is never upgraded on a running box. A protocol mismatch fails with instructions instead.

Any command runs against the box with `--host`:

```sh
devlanes --host hetzner status
devlanes --host hetzner logs web -f
devlanes --host hetzner --lane app@2 restart api
```

The box's daemon answers through the existing connection. Your working directory and environment are not sent, so name lanes with `--lane`. `status` on the box lists the clients connected to it. `secrets unlock` is refused remotely; run it on the box.

## Mirroring

Every allocated port of the box's services and containers is bound on `127.0.0.1` and `::1` under the same number. `psql -p 20012`, debugger ports, gRPC and websockets work unchanged. If a local process already holds a port, `devlanes remote ls` and a `remote.warning` event name the port and the holding PID. That port is retried on the next registry change, and the box's hostnames keep working through a private relay port.

The box's hostnames join your local proxy. TLS is terminated with your local CA, so `devlanes certs trust` on the client is enough, and the traffic goes to the box as a plain stream through the connection. Hostnames belong to the box: its services got their public URLs when they started. When one of a box lane's hostnames is already served by a local lane, `remote connect` warns and skips that whole box lane (its ports too) instead of renaming it, since a renamed host would break CORS origins and cookies.

New lanes, services and containers on the box appear live; removed ones are unbound.

## Hidden

Mark what must stay on the box:

```yaml
services:
  admin:
    run: ./admin-server
    hidden: true        # not mirrored, and the box refuses streams to its ports
infra:
  hidden: [postgres]    # containers by name
```

Hidden units still show in `devlanes --host <box> status`.

## Sync

`services.<name>.sync` entries of running box lanes stream to the connected machine: a full copy first (files at the destination that the box does not have are removed, except ignored ones), then changes and deletions. A `to` under the box user's home lands at the same place under your home; any other `to` falls back to `~/devlanes/sync/<project>/<lane>/<folder>`. Typical uses are a browser extension built on the box and loaded unpacked locally, or an APK to `adb install`.

## GUI tools as services

GUI streaming is not part of v1. Tools that already serve HTTP or TCP from the box work as ordinary services through `remote connect`:

```yaml
services:
  desktop:
    # Xpra's HTML5 client at https://desktop.<lane domain>
    run: "xpra start :100 --daemon=no --html=on --bind-tcp=127.0.0.1:${{ self.port }} --start=gedit"
    hosts: ["desktop.${{ lane.domain }}"]
  adb:
    # Android emulator on the box; locally: ANDROID_ADB_SERVER_PORT=<port> adb devices
    run: "adb -P ${{ self.port }} nodaemon server"
```

serve-sim (iOS simulators on a macOS box) and ws-scrcpy (Android screens in the browser) follow the same pattern: bind the service's port and give it a hostname. Check each tool's own flag for its listen port.

## Limitations

TCP only. UDP ports (DNS servers, WebRTC/TURN, QUIC, game servers) are not mirrored, because the connection has no datagram frames yet. The relay transport (`devlanes login`, no inbound SSH) comes later.

# Remote boxes

A box is any machine you can reach with `ssh` that runs devlanes for its own lanes. Another machine connects to it and gets the box's lanes locally: the same hostnames, the same port numbers and the synced folders. Remote is mirroring only. You start, stop and inspect lanes on the box itself (over `ssh`, or in an editor's remote terminal); the connection never runs commands there and never carries environments or secrets.

## Connect

```sh
devlanes remote connect hetzner              # any ~/.ssh/config host; streams connection state
devlanes remote connect hetzner --background # returns once connected; the connection stays up
devlanes remote ls                           # boxes, state, mirrored ports, skipped lanes, syncs, warnings
devlanes remote sync                         # synced folders of running box lanes
devlanes remote disconnect hetzner           # or `devlanes remote disconnect` for every box
```

The connection belongs to your local daemon, not to the terminal. Ctrl-C on `remote connect` only detaches the CLI. The daemon reconnects with backoff after network loss or sleep. While it is down, mirrored ports stay bound; requests to the box's hostnames wait up to 15 seconds for the reconnect, then get a [box reconnecting](errors/box-reconnecting.md) page. After a reconnect the box's current state replaces what the client knew. When the connection fails for good (rejected credentials, a host name that never resolved), its ports and hostnames are removed and `remote ls` shows the error.

devlanes uses the system `ssh` with one ControlMaster per box, so `~/.ssh/config`, agents, ProxyJump and certificates apply. Passphrase, OTP and host-key prompts appear in the terminal that runs `remote connect`, prefixed with the box's name. An idle master exits after 10 minutes. Over that master devlanes runs `devlanes tunnel --stdio` on the box, which needs only a session channel: `AllowTcpForwarding no` on the box is fine. Windows' OpenSSH has no ControlMaster, so a Windows client runs one `ssh` per connection and authenticates on every reconnect; [remote setup](guides/remote-setup.md#windows-clients) has the details.

Install devlanes on the box yourself, with the same install methods as on any machine; it must be on the `PATH` of a non-interactive ssh session or in `~/.devlanes/bin`. Without it, `remote connect` fails with `devlanes is not installed on <box>` and stops retrying. devlanes never installs or upgrades anything on the box. A protocol mismatch fails with instructions, and a different release on the box is shown as a warning. [Remote setup](guides/remote-setup.md) walks through preparing a box.

When no daemon runs on the box, the connection starts one. Set `remote.start_daemon: false` in the box's user config to keep that under your control: the client then reports that the box daemon is not running and keeps retrying until you start it there (`devlanes daemon install`, or any command that starts it, such as `devlanes start`). On Linux, run `sudo loginctl enable-linger "$USER"` on the box so its daemon survives logout; `remote ls` warns when linger is off. `devlanes daemon install` adds a user unit for boot, but a user unit is also stopped with the session unless linger is on.

`devlanes status --json` on the box lists the clients connected to it under `clients`.

## Mirroring

Every allocated port of the box's running, non-hidden services and containers is bound on `127.0.0.1` and `::1` under the same number. A stopped unit's ports are bound once it starts. `psql -p 20012`, debugger ports, gRPC and websockets work unchanged. When a local process already holds a port, that port is not mirrored and `remote ls` says so, for example `port 5432 is in use on this machine by postgres (pid 812); not mirrored`. It is tried again when the box's lanes change. The box's hostnames keep working through a private relay port.

Both machines allocate ports from the same default pool, so while a box is connected, new port blocks on this machine skip every port its lanes hold, stopped units included. A port that already belongs to a local lane's block stays local: it is not mirrored (`port 20004 is allocated to local lane acme@0; not mirrored`). Lanes allocated on both machines before they first connected often overlap like this; giving one machine a different `ports.range` in its user config keeps them apart. A local service with a literal `PORT` that the box mirrors refuses to start and names the box.

The box's hostnames join your local proxy. TLS is terminated with your local CA, so `devlanes certs trust` on the client is enough, and the traffic goes to the box as a plain stream through the connection. Only names under `.localhost` (or your `hostnames.tld`) are accepted; others are listed under `skipped_hosts` and never get a route or a certificate. Hostnames belong to the box: its services got their public URLs when they started. When one of a box lane's hostnames is already served by a local lane, `remote connect` warns and skips that whole box lane (its ports too) instead of renaming it, since a renamed host would break CORS origins and cookies.

New lanes, services and containers on the box appear live; removed ones are unbound. Connections to a unit that stops or becomes hidden are closed.

A connection to a mirrored port that cannot reach the box's service is closed and reported: the box refused it (the unit is not running, is hidden or no longer listed), the box could not dial the port, the box did not answer, or the link stayed down for 10 seconds (`box hetzner is not connected`). `remote ls` shows the latest one as the port's `last_error` until a connection gets through, and an `open_failed` warning goes to `remote connect` and event subscribers, at most once a minute per port and reason.

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

Hidden units still show in `devlanes status` on the box.

## Sync

`services.<name>.sync` entries of running box lanes stream to `~/.devlanes/sync/<box>/<project>/<lane>/<folder>` on the connected machine (under `$DEVLANES_HOME` when set); the box's `to:` is not used remotely. On every connection the client sends a list of the files it already holds, in batches of 2,000, and the box compares each batch with its source as it arrives. Once the list is complete the box sends only new and changed files, then deletes the client's files that the source lacks. The first connection copies everything; a reconnect over an unchanged folder of 50,000 files writes nothing. A dropped connection during the comparison starts over on the next one. `remote sync` shows the state as `comparing` meanwhile. After that, changes and deletions follow live. devlanes marks that folder as its own and refuses to write into an existing folder it did not create. Files larger than 256 MiB are reported and skipped. Two entries in one box lane whose folders have the same name (replicas of one service included) would share a destination, so neither is synced and `remote sync` and `remote ls` show the error. Typical uses are a browser extension built on the box and loaded unpacked locally, or an APK to `adb install`.

## Processes and data path

Two daemons take part, one on each machine. The client daemon owns the connection: it runs `ssh`, binds the mirrored ports, adds the box's routes to its proxy and writes synced folders. The box daemon owns the box's lanes, as it would without any client. Between them, sshd on the box starts one `devlanes tunnel --stdio` per client connection. The tunnel talks to the client over its stdin and stdout, multiplexed into streams (yamux), and to the box daemon over its local socket like any other devlanes command. On Unix clients all of it runs over one ssh ControlMaster per box; Windows clients run one `ssh.exe` per connection.

A request from a browser on the client takes this path:

```text
browser -> client proxy (TLS with the local CA)
        -> mirror listener on 127.0.0.1:<port> (or a relay port)
        -> one stream over ssh
        -> devlanes tunnel on the box
        -> 127.0.0.1:<port> on the box (then [::1]:<port>)
```

Raw TCP clients such as `psql` skip the proxy and start at the mirror listener.

Each check happens on one side:

- The client daemon checks the protocol version in the handshake and stops on a mismatch. A different release with the same protocol, and linger off on a Linux box, become warnings from the box's version label.
- The tunnel follows the box daemon's events, reads `status --all` and `hosts` again after each change, and sends the client only the differences: lanes, units, states, ports and hostnames, never environments.
- The tunnel authorizes every stream: the port must belong to a running unit that is not hidden. It then dials loopback on the box.
- The client daemon decides what is bound and routed. Local lanes win: a box port inside a local port block is not bound, and a box lane with a hostname a local lane serves is skipped whole. Box hostnames outside `.localhost` and `hostnames.tld` are refused.
- For a sync, the client sends the list of files it holds and the box compares it with its source, so only differences cross the link.

Reconnects are the client daemon's job. It pings every 10 seconds and treats 20 seconds without an answer as a lost session, then retries with a backoff from half a second up to 30 seconds. Mirrored ports stay bound meanwhile, proxy routes show "Reconnecting", and a connection accepted while the link is down waits up to 10 seconds for it to come back. Each new session starts from the box's current registry and a fresh sync comparison. The link stops retrying only for failures that need you: rejected credentials, a changed host key, a host name that never resolved, devlanes missing on the box, or a protocol mismatch.

## GUI tools as services

GUI streaming is not part of v1. Tools that already serve HTTP or TCP from the box work as ordinary services through `remote connect`:

```yaml
services:
  desktop:
    # Xpra's HTML5 client at https://desktop.<lane domain>
    run: "xpra start :100 --daemon=no --html=on --bind-tcp=127.0.0.1:${{ self.port }} --start=gedit"
    hosts: "desktop.${{ lane.domain }}"
  adb:
    # Android emulator on the box; locally: ANDROID_ADB_SERVER_PORT=<port> adb devices
    run: "adb -P ${{ self.port }} nodaemon server"
```

serve-sim (iOS simulators on a macOS box) and ws-scrcpy (Android screens in the browser) follow the same pattern: bind the service's port and give it a hostname. Check each tool's own flag for its listen port.

## Limitations

TCP only. UDP ports (DNS servers, WebRTC/TURN, QUIC, game servers) are not mirrored, because the connection has no datagram frames yet. The relay transport (`devlanes login`, no inbound SSH) comes later.

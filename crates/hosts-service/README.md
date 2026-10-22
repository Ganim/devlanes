# devlanes-hosts-service

This package builds `devlanes-hosts-service.exe`, the Windows counterpart of
`devlanes-hosts`: a service that runs as LocalSystem and keeps the devlanes block
in `%SystemRoot%\System32\drivers\etc\hosts` in sync for one user's daemon. Windows
resolves only the literal `localhost` by itself, so Node, Python, Go and other
programs that are not browsers need the exact names in the hosts file. The daemon
talks to the service over `\\.\pipe\devlanes-hosts` (client in
`devlanes-resolver-hosts`); `devlanes certs trust --hosts` installs it through one
UAC prompt.

Like `crates/hosts-helper/README.md`, this README documents the security model at
length on purpose.

## Usage

```text
devlanes certs trust --hosts      # installs or updates the service; UAC prompt
devlanes certs untrust --hosts    # clears the block, removes the service; UAC prompt
devlanes doctor                   # shows whether the service answers
sc.exe query devlanes-hosts       # its state, and its exit code after a refusal
```

The binary takes no arguments besides `--version`, which prints
`devlanes-hosts-service <version> protocol=<n> build=<profile>` and exits. Started
from a console it says it is a service and exits 2.

The library is what the binary runs, plus the installer and the parts tests drive:

```rust
use devlanes_hosts_service::handler::{Target, handle};
use devlanes_hosts_service::server::{ServerConfig, serve};
use devlanes_hosts_helper::protocol::Request;

// One request against a hosts file, as the service handles it.
let target = Target { hosts: temp_dir.join("hosts"), flush: false };
let response = handle(&Request::Status, &target);

// The pipe server, for a user, until `shutdown` completes.
serve(ServerConfig { pipe_name: name.into(), user: sid, target }, shutdown).await?;
```

## API

- `handler::handle`: one `version`, `status`, `clear` or `apply` request against a hosts file, with the helper's validation and exit codes.
- `server::serve`, `server::pipe_sddl`: the pipe, its access list and the caller check.
- `service::main`: the binary's entry point (service dispatcher, install checks, configuration from the registry).
- `install::install`, `install::uninstall`, `install::service_state`: the elevated installer `devlanes.exe` runs, and a state query that needs no elevation.
- `checks::check_install`: the install location check the service runs on every start.

## What gets installed

| Item | Value |
|---|---|
| Binary | `%ProgramFiles%\devlanes\devlanes-hosts-service.exe` |
| Directory access list | owner Administrators; full control for SYSTEM and Administrators, read and execute for Users; inherited by the binary |
| Service | `devlanes-hosts`, "devlanes hosts file writer", own process, LocalSystem, automatic start, no arguments; the path is quoted because it has spaces |
| Setting | `HKLM\SYSTEM\CurrentControlSet\Services\devlanes-hosts\Parameters\UserSid` (`REG_SZ`), the installing user's SID |

The binary comes from `devlanes-hosts-service.exe` next to `devlanes.exe`, as
`devlanesw.exe` does. A development build works the same way:
`cargo build -p devlanes -p devlanes-hosts-service` puts both in `target\debug`.

## Install procedure

`devlanes certs trust --hosts` checks that the session can show a UAC prompt (a
terminal, not agent or CI output, and a visible desktop; SSH sessions and
services have none) and otherwise fails with `terminal_required`. It then runs
`devlanes.exe __install-hosts-service --user-sid <SID> --source <path> --report <pipe>`
through `ShellExecuteExW` with the `runas` verb, hidden, and waits for it. The
elevated child:

1. Creates `%ProgramFiles%\devlanes` with the access list above, or checks an
   existing one (see the install location check below), and refuses to go on
   otherwise.
2. Refuses a `--source` other than the service binary next to its own
   `devlanes.exe`, reads it once into memory (at most 64 MiB, a Windows program),
   and from then on trusts only those bytes.
3. Writes them to `.devlanes-hosts-service.new.exe` in that directory, owned by
   Administrators, runs that copy's `--version` and checks the pipe protocol, then
   reads the copy back and compares it byte for byte.
4. Stops the service if it runs, moves the old binary to
   `.devlanes-hosts-service.old.exe` and the new one into place, and runs the
   install location check on the result.
5. Creates the service, or updates its configuration, and writes `UserSid`.
6. Starts it, waits until it runs, and asks `version` over the pipe (when the
   approving administrator is the same user, the usual UAC consent; otherwise the
   service rightly refuses that account, and the unelevated CLI asks instead).

A failure after step 4 puts back the previous binary, `UserSid` and running
state, and deletes a service this run created. The child reports its result as
JSON over a pipe only the user can open; the CLI then asks `version` itself and
makes the daemon retry.

`devlanes certs untrust --hosts` clears the block over the pipe as the user, then
runs `__uninstall-hosts-service` elevated, which stops and deletes the service,
removes the `Parameters` key, the binaries and the directory if it is empty. When
the service was never installed, nothing is elevated.

## Security model

Any process running as the installing user can open the pipe and send any
request. The service does not check that its caller is devlanes, so its own checks
are the boundary, as with the sudo helper. It accepts:

- one request per connection: `version`, `status`, `clear`, `apply` or
  `apply --tld <tld>`, followed for `apply` by up to 4096 distinct names (at most
  1,044,480 bytes), one per line, each a lowercase hostname under `localhost` or
  under the `--tld` value;
- a `--tld` of `localhost`, `test`, `internal` or `home.arpa`, or a domain under
  one of them.

Validation is the helper's own code (`devlanes-hosts-helper`'s `names` and
`block`), so the rules and the 4096-name and 1,044,480-byte limits are the same as
on Unix. One bad line fails the whole request. A request header longer than 512
bytes, an unknown command, or names after a command other than `apply` are
refused with the usage code, as is a frame announcing more than 1,044,993 bytes,
before anything is allocated for it. A client that sends nothing for 10 seconds
is disconnected.

Who can talk to it:

- The pipe's access list is `O:SYD:P(A;;GA;;;SY)(A;;0x120083;;;<user-sid>)` with a
  medium mandatory label (no read up, no write up). The user gets read and write
  data, read attributes, read control and synchronize, and not `GENERIC_WRITE`:
  for pipes that would include `FILE_CREATE_PIPE_INSTANCE`, which lets a client
  create server instances of the same pipe and answer later clients. Other users
  cannot open it.
- `PIPE_REJECT_REMOTE_CLIENTS`: nothing reaches it over the network.
- `FILE_FLAG_FIRST_PIPE_INSTANCE`: if any process created the name first, the
  service fails to start instead of sharing the name.
- After reading each request the service impersonates the caller just long
  enough to read its token. It answers only the SID in `UserSid`, from a process
  that is not an AppContainer, not restricted, and at medium integrity or above.
  Everything else gets the code 1 (`service_denied` in devlanes), which the helper
  never uses.

What it will not do:

- It never takes a file path from anyone. The hosts file is `hosts` in the
  expanded `HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\DataBasePath`
  directory (`%SystemRoot%\System32\drivers\etc` on a stock system), and the user
  comes from `UserSid`; both keys are writable only by administrators and SYSTEM.
  There is no `--hosts-file` in any build. Tests drive the request handler and the
  pipe server in-process against temp files and temp pipe names, as the current
  user, never through the service binary.
- It refuses to run unless it runs as SYSTEM, its binary is
  `%ProgramFiles%\devlanes\devlanes-hosts-service.exe`, and that file and its
  directory pass the install location check: no symlink or junction, owned by
  SYSTEM, Administrators or TrustedInstaller, and no access entry (other than
  inherit-only ones) granting any write, delete, access-list or owner right to
  another account. This is the counterpart of the helper's "root-owned, not group
  or world writable". A refusal stops the service with a service-specific exit
  code (1 not SYSTEM, 2 unsafe install, 3 missing or bad settings, 4 pipe name
  taken, 5 runtime), which Windows records in the System event log (event 7024).
- It refuses a hosts file (or `etc` directory) that is a symlink or junction, or a
  hosts file that is not a regular file, and malformed markers.

The write: the new content goes to `etc\.hosts.devlanes-<pid>.tmp`, created
exclusively, then `FlushFileBuffers`. Just before the swap the service reads the
hosts file again and starts over from the new content if another program changed
it, up to three attempts. `ReplaceFileW` then swaps the content in, keeping the
hosts file's access list and attributes (a rename would leave the temp file's
inherited access list), with the old content passing through
`etc\.hosts.devlanes-<pid>.bak`, which is removed afterwards. Sharing and lock
violations and access denied (antivirus scanners and editors holding the file) are
retried after 50, 100, 200, 400 and 800 ms. After a change the service runs
`System32\ipconfig.exe /flushdns` (path from the API, `CREATE_NO_WINDOW`, at most
10 s); the DNS Client service reloads the file by itself, and a failed flush does
not fail the update. Requests are handled one at a time.

The daemon's side checks the service too: before sending anything it requires the
pipe's owner to be SYSTEM (only SYSTEM or an administrator can create an object
owned by SYSTEM), and when it may read the serving process's token, that the
process runs as SYSTEM. A pipe name taken by another program fails with
`service_untrusted`, and nothing is sent to it.

Limits of this model:

- Any process running as the user can replace the block: clear it, or add any
  names under `.localhost` and the accepted TLDs. It can also make the service
  rewrite the file and flush the DNS cache over and over. Same as on Unix.
- A process of the user that creates `\\.\pipe\devlanes-hosts` while the service
  is stopped keeps the service from starting, and only fools that user's own
  daemon, which refuses it as not SYSTEM.
- The service parses caller input as SYSTEM. It is written without `unsafe`;
  Win32 calls go through `devlanes-sys` and the service plumbing through the
  `windows-service` crate.
- There is no event log line per change; the service manager logs starts, stops
  and refusals.
- Changing the binary, its directory or the service configuration needs
  administrator rights.

## Troubleshooting

- `service_missing`: nothing serves the pipe. Run `devlanes certs trust --hosts`,
  or check `sc.exe query devlanes-hosts`; a stopped service shows the exit code of
  its refusal.
- `service_denied`: the service was installed by another user, or the caller is
  sandboxed. Run `devlanes certs trust --hosts` as the user who runs the daemon.
- `service_untrusted`: another program holds the pipe name. Restart the service
  (`sc.exe stop devlanes-hosts`, `sc.exe start devlanes-hosts` from an elevated
  terminal) or reboot.
- `helper_outdated`: the installed service speaks another pipe protocol. Run
  `devlanes certs trust --hosts` again.
- `terminal_required`: the session cannot show the UAC prompt. Run the command
  from a normal terminal on the desktop.

To check that a name resolves through the file, use
`Resolve-DnsName web.acme-2.localhost -Type A` or
`node -e "require('dns').lookup('web.acme-2.localhost', console.log)"`.
`nslookup` queries DNS directly and never reads the hosts file.

## Development

```text
cargo test -p devlanes-hosts-service
cargo build --release -p devlanes-hosts-service
set DEVLANES_HOSTS_SERVICE=%CD%\target\release\devlanes-hosts-service.exe
cargo test -p devlanes-resolver-hosts --test it -- --ignored   # elevated terminal; installs for real
```

Bump `PIPE_PROTOCOL` in `crates/hosts-helper/src/protocol.rs` whenever requests or
responses change in a way an older daemon or service would misread.

## License

Apache-2.0

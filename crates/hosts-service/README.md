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
sc.exe query devlanes-hosts       # its state, and its exit code after a failed start
```

The service manager starts the binary as `devlanes-hosts-service.exe --user-sid
<SID>`. `--version` prints
`devlanes-hosts-service <version> protocol=<n> build=<profile>` and exits. Any
other arguments, or a start from a console, print that it is a service and exit 2.

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
- `server::serve`, `server::pipe_sddl`: the pipe and its access list.
- `service::main`: the binary's entry point (arguments, service dispatcher).
- `install::install`, `install::uninstall`, `install::service_state`: the elevated installer `devlanes.exe` runs, and a state query that needs no elevation. `install::exit_failure` maps the installer's exit code back to an error.
- `write::update`: the hosts file replace.

## Installed items

| Item | Value |
|---|---|
| Binary | `%ProgramFiles%\devlanes\devlanes-hosts-service.exe` |
| Access lists | directory and binary: owner Administrators; full control for SYSTEM and Administrators, read and execute for Users; set when each is created, not inherited or applied afterwards |
| Service | `devlanes-hosts`, "devlanes hosts file writer", own process, LocalSystem, automatic start, arguments `--user-sid <SID>` (the installing user's); the path is quoted because it has spaces |
| Recovery | restart after 5 s, three times within a day, also after a failed start (a service-specific exit code) |

The binary comes from `devlanes-hosts-service.exe` next to `devlanes.exe`, as
`devlanesw.exe` does. A development build works the same way:
`cargo build -p devlanes -p devlanes-hosts-service` puts both in `target\debug`.

## Install procedure

`devlanes certs trust --hosts` checks that the session can show a UAC prompt (a
terminal, not agent or CI output, and a visible desktop; SSH sessions and
services have none) and otherwise fails with `terminal_required`. It then runs
`devlanes.exe __install-hosts-service --user-sid <SID>` through `ShellExecuteExW`
with the `runas` verb, hidden, and waits for it. The elevated child:

1. Refuses a SID that does not look like a person's account (`S-1-5-21-` or, for
   Microsoft Entra ID, `S-1-12-1-`, then four numbers).
2. Checks that `%ProgramFiles%` is admin-only, then creates
   `%ProgramFiles%\devlanes` with the access list above or checks an existing
   one the same way (see the install location check below).
3. Reads `devlanes-hosts-service.exe` next to its own `devlanes.exe` once into
   memory (at most 64 MiB, a Windows program) and from then on trusts only those
   bytes. It takes no source path from the command line.
4. Writes them to `.devlanes-hosts-service.new.exe` in that directory, created
   with its owner and access list in one call, so it never has another owner
   (under the "default owner: object creator" policy that would be the user, for
   a moment), runs that copy's `--version` and checks the pipe protocol, then
   reads the copy back and compares it byte for byte.
5. Stops the service if it runs, moves the old binary to
   `.devlanes-hosts-service.old.exe` and the new one into place, and checks that
   the binary is admin-only.
6. Creates the service, or updates its configuration, sets the recovery actions
   above, starts it and waits until it runs.

A failure after step 5 puts back the previous binary and running state, and
deletes a service this run created. The child's console is hidden, so it reports
a failure only as its exit code (10 and up, one per error code); the CLI turns
that into the error code and a fixed message. The CLI then asks `version` over
the pipe itself, which also checks that the service answers this user when
another administrator approved the prompt, and makes the daemon retry.

`devlanes certs untrust --hosts` clears the block over the pipe as the user, then
runs `__uninstall-hosts-service` elevated, which stops and deletes the service and
removes the binaries and the directory. The CLI warns if the directory is still
there afterwards. When the service was never installed, nothing is elevated.

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
before anything is read; a request body takes memory only as it arrives. A
client that sends nothing for 10 seconds, or does not read its answer within 10
seconds, is disconnected. Connections are handled one at a time; later clients
wait.

Windows decides who can talk to it when a client opens the pipe; the service
adds no check of its own:

- The pipe's access list is `O:SYD:P(A;;GA;;;SY)(A;;0x120083;;;<user-sid>)` with a
  medium mandatory label (no read up, no write up). The user gets read and write
  data, read attributes, read control and synchronize, and not `GENERIC_WRITE`:
  for pipes that would include `FILE_CREATE_PIPE_INSTANCE`, which lets a client
  create server instances of the same pipe and answer later clients. Other
  users cannot open it, nor can AppContainer processes (nothing grants them) or
  processes below medium integrity (the label). A restricted token gets in only
  when its restricting SIDs include the user.
- `PIPE_REJECT_REMOTE_CLIENTS`: nothing reaches it over the network.
- `FILE_FLAG_FIRST_PIPE_INSTANCE`: if any process created the name first, the
  service fails to start (exit code 1) instead of sharing the name; the
  recovery actions start it again three times.

What it will not do:

- It never takes a file path from anyone. The hosts file is
  `drivers\etc\hosts` under System32 (the directory comes from the API), and the
  user is the `--user-sid` in its service configuration, which only
  administrators can change. There is no `--hosts-file` in any build. Tests
  drive the request handler and the pipe server in-process against temp files
  and temp pipe names, as the current user, never through the service binary.
- The install location check: `%ProgramFiles%`, `%ProgramFiles%\devlanes` and
  the binary have no symlink or junction, are owned by SYSTEM, Administrators or
  TrustedInstaller, and have no access entry (other than inherit-only ones)
  granting any write, delete, access-list or owner right to another account.
  This is the counterpart of the helper's "root-owned, not group or world
  writable", and the installer runs it before starting the service. The service
  does not repeat it: code able to change the binary would not run the check,
  and changing those entries afterwards needs administrator rights.
- It refuses a hosts file that is a symlink or junction or not a regular file,
  and malformed markers.

The write: the new content goes to `etc\.hosts.devlanes-<pid>.tmp`, created
exclusively, then `FlushFileBuffers`. Just before the rename the service reads
the hosts file again and starts over from the new content if another program
changed it, up to three attempts. The temp file is then renamed over `hosts`.
A program holding the hosts file open without delete sharing makes the rename
fail with access denied, which is retried once after 10 ms; the update then
fails, and the daemon tries again with its next change or
`devlanes certs trust --hosts`. The temp file inherits
the `etc` directory's access list, which is exactly what a stock hosts file has
(it has no entries of its own). A hosts file with the read-only attribute is
refused at once with a message saying so. After a change the service runs
`System32\ipconfig.exe /flushdns` (path from the API, `CREATE_NO_WINDOW`, at most
10 s); the DNS Client service reloads the file by itself, and a failed flush does
not fail the update.

The daemon's side checks the service too: before sending anything it requires the
pipe's owner to be SYSTEM (only SYSTEM or an administrator can create an object
owned by SYSTEM). A pipe name taken by another program fails with
`service_untrusted`, and nothing is sent to it.

Limits of this model:

- Any process running as the user can replace the block: clear it, or add any
  names under `.localhost` and the accepted TLDs. It can also make the service
  rewrite the file and flush the DNS cache over and over. Same as on Unix.
- Any process of any local account can create `\\.\pipe\devlanes-hosts` while
  the service is stopped (at boot before it starts, or after a crash). That
  keeps the service from starting while the name is held; the service manager
  restarts it three times, 5 seconds apart. It is a denial of service only: the
  daemon refuses a pipe not owned by SYSTEM and sends nothing to it.
- `devlanes certs trust --hosts` asks UAC to elevate `devlanes.exe` from wherever
  it is installed, usually a directory the user can write, and that copy installs
  the service binary next to it, also user-writable until it is copied. A process
  already running as the user before the prompt can replace either, and the
  user's consent then runs its code as administrator and installs it as SYSTEM.
  This is the same exposure as the sudo install on Unix, where the user's shell
  runs the installer.
- The rename gives the hosts file the `etc` directory's inherited access list
  and the service's default owner (SYSTEM or Administrators). Entries an
  administrator added to the hosts file itself are lost on the first write.
- The service parses caller input as SYSTEM, with all of LocalSystem's
  privileges. It is written without `unsafe`; Win32 calls go through
  `devlanes-sys` and the service plumbing through the `windows-service` crate.
- There is no event log line per change; the service manager logs starts, stops
  and failed starts.
- Changing the binary, its directory or the service configuration needs
  administrator rights.

A failed start stops the service with a service-specific exit code, which
Windows records in the System event log (event 7024): 1 the pipe name is taken,
2 System32 could not be found or the async runtime did not start.

## Troubleshooting

- `service_missing`: nothing serves the pipe. Run `devlanes certs trust --hosts`,
  or check `sc.exe query devlanes-hosts`; a stopped service shows the exit code of
  its failed start.
- `service_denied`: the pipe refused this account. The service was installed by
  another user, or the caller is sandboxed. Run `devlanes certs trust --hosts` as
  the user who runs the daemon.
- `service_untrusted`: another program holds the pipe name. Restart the service
  (`sc.exe stop devlanes-hosts`, `sc.exe start devlanes-hosts` from an elevated
  terminal) or reboot.
- `helper_outdated`: the installed service speaks another pipe protocol. Run
  `devlanes certs trust --hosts` again.
- `hosts_unsafe_directory` during install: `%ProgramFiles%`,
  `%ProgramFiles%\devlanes` or the binary can be changed by other accounts;
  `icacls` shows their access lists.
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

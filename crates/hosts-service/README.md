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
let target = Target { hosts: temp_dir.join("hosts"), flush: false, check_directory: false };
let response = handle(&Request::Status, &target);

// The pipe server, for a user, until `shutdown` completes.
serve(ServerConfig { pipe_name: name.into(), user: sid, target }, shutdown).await?;
```

## API

- `handler::handle`: one `version`, `status`, `clear` or `apply` request against a hosts file, with the helper's validation and exit codes.
- `server::serve`, `server::pipe_sddl`: the pipe, its access list and the caller check.
- `service::main`: the binary's entry point (service dispatcher, install checks, configuration from the registry).
- `install::install`, `install::uninstall`, `install::service_state`: the elevated installer `devlanes.exe` runs, and a state query that needs no elevation.
- `checks::check_install`: the install location check.
- `write::update`, `write::recover`: the hosts file replace, and putting the file back from a backup a failed or interrupted replace left.

## What gets installed

| Item | Value |
|---|---|
| Binary | `%ProgramFiles%\devlanes\devlanes-hosts-service.exe` |
| Access lists | directory and binary: owner Administrators; full control for SYSTEM and Administrators, read and execute for Users; set when each is created, not inherited or applied afterwards |
| Service | `devlanes-hosts`, "devlanes hosts file writer", own process, LocalSystem, automatic start, no arguments; the path is quoted because it has spaces |
| Recovery | restart after 5 s, three times within a day, also after a refusal (a service-specific exit code) |
| Privileges | `SeChangeNotifyPrivilege` only; the service manager removes LocalSystem's others |
| Setting | `HKLM\SYSTEM\CurrentControlSet\Services\devlanes-hosts\Parameters\UserSid` (`REG_SZ`), the installing user's SID; the key is recreated owned by Administrators |

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

1. Refuses a `--user-sid` that is not a person's account: it must look like one
   (`S-1-5-21-` or, for Microsoft Entra ID, `S-1-12-1-`, then four numbers) and
   the account database must call it a user, not a group.
2. Checks that `%ProgramFiles%` is admin-only, creates `%ProgramFiles%\devlanes`
   with the access list above or checks an existing one (see the install
   location check below), and refuses any file in it besides the service binary
   and its `.new.exe` and `.old.exe` copies, such as a planted DLL.
3. Refuses a `--source` other than the service binary next to its own
   `devlanes.exe`, opens it without following links, refuses it with more than
   one name (hard link), reads it once into memory (at most 64 MiB, a Windows
   program), and from then on trusts only those bytes.
4. Writes them to `.devlanes-hosts-service.new.exe` in that directory, created
   with its owner and access list in one call, so it never has another owner
   (under the "default owner: object creator" policy that would be the user, for
   a moment), runs that copy's `--version` and checks the pipe protocol, then
   reads the copy back and compares it byte for byte.
5. Stops the service if it runs, moves the old binary to
   `.devlanes-hosts-service.old.exe` and the new one into place, and runs the
   install location check on the result.
6. Creates the service, or updates its configuration, sets the recovery actions
   and privileges above, and recreates the `Parameters` key with `UserSid`.
7. Starts it, waits until it runs, and asks `version` over the pipe (when the
   approving administrator is the same user, the usual UAC consent; otherwise the
   service rightly refuses that account, and the unelevated CLI asks instead).

A failure after step 5 puts back the previous binary, `UserSid` and running
state, and deletes a service this run created. The child reports its result as
JSON over a single-instance pipe on which the user may read and write but not
add instances; the CLI uses the report only when the pipe's client process is
the elevated child it started (same PID), then asks `version` itself and makes
the daemon retry.

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
before anything is read; a request body takes memory only as it arrives. A
client that sends nothing for 10 seconds, or does not read its answer within 10
seconds, is disconnected. At most four connections are handled at once; later
clients wait for a free one.

Who can talk to it:

- The pipe's access list is `O:SYD:P(A;;GA;;;SY)(A;;0x120083;;;<user-sid>)` with a
  medium mandatory label (no read up, no write up). The user gets read and write
  data, read attributes, read control and synchronize, and not `GENERIC_WRITE`:
  for pipes that would include `FILE_CREATE_PIPE_INSTANCE`, which lets a client
  create server instances of the same pipe and answer later clients. Other users
  cannot open it.
- `PIPE_REJECT_REMOTE_CLIENTS`: nothing reaches it over the network.
- `FILE_FLAG_FIRST_PIPE_INSTANCE`: if any process created the name first, the
  service retries for 5 seconds, then fails to start (exit code 4) instead of
  sharing the name; the recovery actions start it again three times.
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
- The install location check: the binary is
  `%ProgramFiles%\devlanes\devlanes-hosts-service.exe`, and that file, its
  directory and `%ProgramFiles%` have no symlink or junction, are owned by
  SYSTEM, Administrators or TrustedInstaller, and have no access entry (other
  than inherit-only ones) granting any write, delete, access-list or owner right
  to another account; the directory holds nothing else. This is the counterpart
  of the helper's "root-owned, not group or world writable". The installer runs
  it before starting the service, and that is what protects the binary. The
  service runs it again on every start, but that only reports a later
  misconfiguration: code that could change the binary or plant a DLL next to it
  would not run the check. A refusal stops the service with a service-specific
  exit code (1 not SYSTEM, 2 unsafe install, 3 missing or bad settings,
  including a `UserSid` that is not a user account's, 4 pipe name taken, 5
  runtime, 6 the hosts file's directory is not admin-only), which Windows
  records in the System event log (event 7024).
- It refuses a hosts file (or `etc` directory) that is a symlink or junction, a
  hosts file that is not a regular file or has more than one name (hard link),
  and malformed markers. At start and before each write it checks that the
  `etc` directory passes the same admin-only check as the install directory.
- It holds `SeChangeNotifyPrivilege` only. Reading the caller's token needs no
  more: the daemon connects at identification level, and Windows lowers any
  other client's impersonation to identification for a server without
  `SeImpersonatePrivilege`, which is still enough to read the token.

The write: the new content goes to `etc\.hosts.devlanes-<pid>.tmp`, created
exclusively, then `FlushFileBuffers`. Just before the swap the service reads the
hosts file again and starts over from the new content if another program changed
it, up to three attempts. `ReplaceFileW` then swaps the content in, keeping the
hosts file's access list and attributes (a rename would leave the temp file's
inherited access list), with the old content passing through
`etc\.hosts.devlanes-<pid>.bak`. Sharing and lock violations and access denied
(antivirus scanners and editors holding the file) are retried after 50, 100,
200, 400 and 800 ms. A hosts file with the read-only attribute is refused at
once with a message saying so. After a change the service runs
`System32\ipconfig.exe /flushdns` (path from the API, `CREATE_NO_WINDOW`, at most
10 s); the DNS Client service reloads the file by itself, and a failed flush does
not fail the update. Requests are handled one at a time.

The backup is deleted only while `hosts` exists. If `ReplaceFileW` fails after
moving the hosts file to the backup name and moving it back fails too, the error
says `hosts file left at <backup>` and the backup stays. Before every write and
at every start, a missing hosts file with exactly one `.hosts.devlanes-*.bak`
next to it is put back from it; with several, the write is refused and names
them. With the hosts file present, leftover temp and backup files are old copies
and are removed.

`ReplaceFileW` keeps the access list but not the owner: the result is owned by
the service's default owner (SYSTEM or Administrators). Both have full control
through the stock access list anyway, so nobody gains access; a hosts file owned
by someone else only loses that owner's implicit right to change its access
list.

The daemon's side checks the service too: before sending anything it requires the
pipe's owner to be SYSTEM (only SYSTEM or an administrator can create an object
owned by SYSTEM), and when it may read the serving process's token, that the
process runs as SYSTEM. A pipe name taken by another program fails with
`service_untrusted`, and nothing is sent to it.

Limits of this model:

- Any process running as the user can replace the block: clear it, or add any
  names under `.localhost` and the accepted TLDs. It can also make the service
  rewrite the file and flush the DNS cache over and over. Same as on Unix.
- Any process of any local account can create `\\.\pipe\devlanes-hosts` while
  the service is stopped (at boot before it starts, or after a crash). That
  keeps the service from starting while the name is held; the service waits 5
  seconds for it and the service manager restarts it three times, 5 seconds
  apart. It is a denial of service only: the daemon refuses a pipe not owned by
  SYSTEM and sends nothing to it.
- `devlanes certs trust --hosts` asks UAC to elevate `devlanes.exe` from wherever
  it is installed, usually a directory the user can write, and that copy installs
  the service binary next to it, also user-writable until it is copied. A process
  already running as the user before the prompt can replace either, and the
  user's consent then runs its code as administrator and installs it as SYSTEM.
  This is the same exposure as the sudo install on Unix, where the user's shell
  runs the installer. Checking the Authenticode signature of both files in
  release builds is future work (packaging and signing, plan step 13).
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
- `hosts file left at ...`: a replace failed halfway and the content is under
  the backup name. The next update puts it back; to do it by hand, rename the
  file to `hosts` from an elevated terminal.
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

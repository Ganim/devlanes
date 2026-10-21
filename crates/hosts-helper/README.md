# devlanes-hosts-helper

This package builds `devlanes-hosts`, a small program that runs as root and keeps one block of
lines in `/etc/hosts` in sync with the hostnames devlanes routes. It exists for
programs that cannot resolve `*.localhost` names on their own. The daemon only
calls it when a project needs names written, or when the helper is installed and
a block from an earlier run has to be replaced or removed.

## Background

devlanes gives every lane hostnames such as `web.acme-2.localhost`. RFC 6761
reserves `localhost` and says resolvers should answer every name under it with a
loopback address, but support is uneven:

| Client | Resolves `*.localhost` |
|---|---|
| Chrome, Edge, Firefox 84+ | yes, built into the browser |
| macOS 26 and later, system resolver (`getaddrinfo`, curl, Node, Python, Rust) | yes |
| macOS 15 and older, system resolver | no; the query goes to the upstream DNS server, and most return nothing |
| Safari and WKWebView on macOS 15 and older | no |
| Linux with glibc and `nss-myhostname` or systemd-resolved | yes |
| Linux with glibc and neither | no |
| musl (Alpine and most small containers) | no; musl only reads `/etc/hosts` and DNS |
| Go programs using the pure-Go resolver (`CGO_ENABLED=0`, static Linux builds, `GODEBUG=netdns=go`) | no; the name is sent to DNS |
| workerd (the runtime behind `wrangler dev`) | no |

`/etc/hosts` is read by every one of these. Its format maps an address to exact
names, and no implementation (glibc `nss-files`, macOS, musl, Go) supports
patterns, so a wildcard such as `*.acme-2.localhost` cannot be written there.
devlanes writes every exact name it routes instead. Wildcard routes (for example
`*.pages-2.acme.localhost`) are left out, so only the exact names devlanes knows
about resolve through the file.

`/etc/hosts` is owned by root. Writing it from the daemon would mean running the
daemon as root or asking for a password on every route change. The helper is the
smallest root-owned piece that can do the write: the daemon runs it through
`sudo -n` with a rule that allows this one program and nothing else.

## Config example

A user setting and a project setting decide whether a project's names are written.

In `~/.devlanes/config.yaml`:

```yaml
hostnames:
  # auto, never or always; default: auto.
  hosts_file: auto
```

In a project's `devlanes.yaml`:

```yaml
lanes:
  # Default: the user's setting.
  hosts_file: always
```

- `never` in the user config turns the feature off for every project. A
  repository cannot override it.
- Otherwise the project's `lanes.hosts_file`, when set, replaces the user's
  setting for that project. Projects use `always` when they run something that
  ignores the OS resolver, such as workerd.
- `always` writes the project's names.
- `auto` writes them only when the system resolver fails a probe. The daemon
  resolves a random name like `devlanes-probe-4812-93021.localhost` with
  `getaddrinfo`, and the probe passes only if every answer is a loopback
  address and arrives within the timeout. The probe tests the OS resolver, so
  it cannot detect musl, pure-Go or workerd inside a project. Those need the
  project setting.
- `hostnames.resolver: hosts_file` behaves like `hosts_file: always`.

With the `resolver` strategy, names under the configured `hostnames.tld` are
written as well, provided the TLD is one the helper accepts (see the security
model below).

Entries stay in `/etc/hosts` after the daemon stops. They point at loopback
and only matter while the devlanes proxy listens there.

## Usage

```sh
devlanes certs trust --hosts
```

This installs the devlanes CA in the trust stores, as `devlanes certs trust`
does, and then the helper and its sudo rule.

The helper comes from the `devlanes-hosts` binary next to the `devlanes`
binary, or from the path in `DEVLANES_HOSTS_HELPER`. The installer reads it
once into memory and writes that copy to a private temp directory (`0700`),
together with the rule. It runs `<copy> version` and refuses anything that
reports `build=debug` or another protocol. From then on only the private copy
is used, so rebuilding or replacing the original file mid-install has no effect.

It then asks for your password once (sudo caches it for the remaining commands)
and runs the commands below, shown for macOS. Linux uses group `root` instead
of `wheel`.

```sh
sudo /usr/bin/test -e /etc/sudoers.d/devlanes    # is there a rule to back up?
sudo /usr/bin/install -d -o root -g wheel -m 0755 -- /usr/local/libexec    # only if missing
sudo /usr/bin/install -o root -g wheel -m 0755 -- <copy> /usr/local/libexec/.devlanes-hosts.new
sudo /usr/bin/install -o root -g wheel -m 0440 -- <rule> /etc/sudoers.d/.devlanes.new
sudo /usr/sbin/visudo -c -f /etc/sudoers.d/.devlanes.new
sudo /bin/cat -- /etc/sudoers.d/.devlanes.new    # compared with the intended rule
sudo /bin/cp -p -- /etc/sudoers.d/devlanes /etc/sudoers.d/.devlanes.old    # only on reinstall
sudo /bin/cp -p -- /usr/local/libexec/devlanes-hosts /usr/local/libexec/.devlanes-hosts.old    # only on reinstall
sudo /bin/mv -f -- /etc/sudoers.d/.devlanes.new /etc/sudoers.d/devlanes
sudo /usr/sbin/visudo -c
sudo /bin/mv -f -- /usr/local/libexec/.devlanes-hosts.new /usr/local/libexec/devlanes-hosts
sudo /bin/rm -f -- /usr/local/libexec/.devlanes-hosts.old /etc/sudoers.d/.devlanes.old
sudo -n -k /usr/local/libexec/devlanes-hosts version    # proves the rule works without a password
```

Both files are first copied to root-owned dot-names. sudo ignores files in
`sudoers.d` whose names contain a dot, so a staged rule is never read. Before
anything is moved into place, the installer compares the staged helper byte for
byte with the copy it checked, and the staged rule with the rule it meant to
write; `visudo` only checks syntax, so this comparison is what catches a file
swapped or edited after it was checked. On a mismatch, or if the syntax check
fails, the staged files are removed and the installed helper and rule are left
as they were.

The rule moves into place before the helper, so the helper is only replaced
after the full `visudo -c` passes. If that check or a later move fails, or the
installed helper does not have the expected owner and mode, the previous rule
and helper are restored from the `.old` copies (on a first install, the new
files are removed). If everything is in place but `sudo -n` still asks for a
password, the installer reports it and leaves the new files installed; see
`sudo_denied` below.

Before running anything, the installer checks that `/`, `/usr`, `/usr/local`,
`/usr/local/libexec` (those that exist) and `/etc/sudoers.d` are real
directories, owned by root, and not writable by their group or by everyone. It
refuses to install otherwise.

### Installed files

| Path | Owner | Mode | Content |
|---|---|---|---|
| `/usr/local/libexec/devlanes-hosts` | `root:wheel` (macOS), `root:root` (Linux) | `0755` | this program |
| `/etc/sudoers.d/devlanes` | same | `0440` | the rule below |

The rule, for a user named `alice`:

```
# Installed by `devlanes certs trust --hosts`; remove with `devlanes certs untrust --hosts`.
# Lets alice run the devlanes /etc/hosts helper as root without a password.
# The helper only edits the block between `# devlanes begin` and `# devlanes end`.
Defaults!/usr/local/libexec/devlanes-hosts !requiretty
alice ALL=(root) NOPASSWD: /usr/local/libexec/devlanes-hosts
```

The user name comes from `id -un` and may only contain letters, digits, `_`,
`-` and `.`, so it cannot carry sudoers syntax. `!requiretty` lets the daemon
run the helper without a terminal on systems that still set `requiretty`.

## The block

The helper only touches the lines from `# devlanes begin` to `# devlanes end`:

```
# devlanes begin
# Managed by devlanes-hosts. Lines between these markers are replaced on every update.
127.0.0.1 api.acme-2.localhost
::1 api.acme-2.localhost
127.0.0.1 web.acme-2.localhost
::1 web.acme-2.localhost
# devlanes end
```

- Names are sorted, one per line, each mapped to `127.0.0.1` and then `::1`.
- If the block is missing it is appended at the end of the file. If the file
  does not end with a line break, one is added first; that is the only byte
  ever added outside the block.
- An empty name list removes the block, markers included.
- Every byte outside the block is kept as is, including comments in any
  encoding and CRLF line breaks. A new block uses the file's line-break style.
- If a marker appears more than once, the markers are out of order, or one is
  missing its pair, the helper refuses to write and names the lines to fix.
- When the new content equals the old, nothing is written and no cache is
  flushed.

## Security model

Any process running as the installing user can run
`sudo -n /usr/local/libexec/devlanes-hosts` with any arguments and any stdin.
The helper does not check that its caller is devlanes, so its own checks are
the boundary. It accepts:

- the subcommands `apply`, `clear`, `status` and `version`, plus `--tld <tld>`
  for `apply`;
- on stdin for `apply`, up to 4096 distinct names (at most 1,044,480 bytes),
  one per line, each a lowercase hostname: labels of 1 to 63 letters, digits
  and inner hyphens, at most 253 characters in total, no trailing dot, no
  wildcard;
- only names strictly below `localhost`, or below the `--tld` value.

`--tld` must be `localhost`, `test`, `internal` or `home.arpa`, or a domain
under one of them, such as `acme.test` or `dev.internal`. These are reserved
for local or private use (RFC 6761, RFC 8375, and ICANN's 2024 reservation of
`.internal`), so no public site lives under them. The value comes from the
unprivileged caller, so checking that it matches the user's configured TLD
would prove nothing. Without this list, any process running as the user could
point `example.com` or an update server at loopback for every account on the
machine. A configured `hostnames.tld` outside the list still works through the
DNS resolver strategy, but its names are not written to `/etc/hosts`.

It refuses, without writing anything:

- any invalid name, reporting its line number. One bad line fails the whole
  input, so a caller bug never leaves a partial block;
- a symlinked `/etc/hosts`, or one that is not a regular file. Writing through
  a link would replace the link, not its target;
- malformed markers.

The write is atomic. The new content goes to `/etc/.hosts.devlanes-<pid>.tmp`,
created exclusively with mode `0600`; the temp file then gets the original
file's mode, owner and group, is synced to disk, and is renamed over
`/etc/hosts`. Just before the rename the helper reads `/etc/hosts` again. If
another program changed it in the meantime, the helper starts over from the new
content, up to three attempts.

After a change it flushes resolver caches, using absolute paths and an empty
environment: `/usr/bin/dscacheutil -flushcache` and
`/usr/bin/killall -HUP mDNSResponder` on macOS, and `resolvectl flush-caches`
on Linux when `resolvectl` exists in `/usr/bin` or `/bin`. A failed flush is
printed as a warning and does not fail the update.

Changing what the helper does requires root already. The binary and every
directory above it are root-owned and not writable by others (the installer
refuses to install otherwise), and the rule names the full path, so the user
cannot swap the program or edit the rule without root access.

Limits of this model:

- Any process running as the user can replace the block: clear it, or add any
  names under `.localhost` and the accepted TLDs. It can also make the helper
  rewrite the file and flush caches over and over.
- The helper is a root process parsing caller input. It uses only the Rust
  standard library, no `unsafe` and no async runtime, and is about 1,100 lines
  of non-test code including comments.
- sudo logs each run to the system log.
- Debug builds accept `--hosts-file <path>` so tests can edit a temp file. The
  flag is compiled out of release builds (`cfg(debug_assertions)`). A debug
  build also refuses it when `SUDO_UID` is set, which sudo always sets, and the
  installer refuses debug builds. Do not copy a debug build to the install path
  by hand: behind the rule, that flag would let the user overwrite any file as
  root.

## Inspecting

```sh
/usr/local/libexec/devlanes-hosts status     # prints the block; no root needed
/usr/local/libexec/devlanes-hosts version    # devlanes-hosts 0.1.0 protocol=1 build=release
sudo -l                                      # lists your sudo rules, including this one
ls -l /usr/local/libexec/devlanes-hosts /etc/sudoers.d/devlanes
```

To check that a name resolves through the file, use
`dscacheutil -q host -a name web.acme-2.localhost` on macOS or
`getent hosts web.acme-2.localhost` on Linux. `dig` and `nslookup` query DNS
directly and never read `/etc/hosts`.

## Removing

```sh
devlanes certs untrust --hosts
```

This clears the block, removes the rule and the helper, then removes the devlanes
CA from the trust stores, undoing `devlanes certs trust --hosts`. Use
`devlanes certs untrust --hosts --keep-ca` to remove only the helper and keep the
CA trusted. If the helper is already gone, the block cannot be cleared that way;
delete the lines between the markers by hand. To remove everything manually:

```sh
sudo /usr/local/libexec/devlanes-hosts clear
sudo rm -f /etc/sudoers.d/devlanes /usr/local/libexec/devlanes-hosts
```

`/usr/local/libexec` itself is left in place.

## Command reference

```
devlanes-hosts apply [--tld <tld>]   names on stdin, one per line
devlanes-hosts clear
devlanes-hosts status
devlanes-hosts version
```

`apply` and `clear` print `changed names=<n>` or `unchanged names=<n>`. Errors
go to stderr as `devlanes-hosts: <message>`.

| Exit | Meaning |
|---|---|
| 0 | success |
| 2 | bad arguments |
| 3 | invalid name or TLD, or more than 4096 names |
| 4 | `/etc/hosts` refused: symlink, not a regular file, malformed markers, or it kept changing |
| 5 | I/O error |

The helper never exits with 1. Under `sudo -n`, exit 1 always comes from sudo
refusing to run it.

## Troubleshooting

### `helper_missing`

`/usr/local/libexec/devlanes-hosts` does not exist. Run `devlanes certs trust --hosts`.

### `sudo_denied`

sudo asked for a password or refused. Check with
`sudo -n -k /usr/local/libexec/devlanes-hosts version`. If that fails right
after an install, `/etc/sudoers` probably does not read `/etc/sudoers.d`; it
needs `#includedir /etc/sudoers.d` or `@includedir /etc/sudoers.d`. After a
denial the daemon waits five minutes before trying sudo again, so a missing
rule does not fill the auth log. Running `devlanes certs trust --hosts` makes it
retry at once.

### `helper_outdated`

The installed helper speaks a different protocol version from this devlanes.
Run `devlanes certs trust --hosts` again.

### `hosts_file_refused`

`/etc/hosts` is a symlink or has broken markers. The message names the lines;
fix them by hand. Inside containers `/etc/hosts` is often a bind mount: the
rename fails with "resource busy", and the helper does not fall back to writing
in place.

### `hosts_unsafe_directory` during install

A directory on the install path is not root-owned, or is writable by its group
or by everyone. Known cases are Homebrew on Intel Macs, which can leave
`/usr/local` subdirectories owned by your user, and older Debian systems, where
`/usr/local` is group-writable by `staff`. Fix the ownership, or keep using the
`localhost` strategy without the helper.

### `helper_source_rejected` during install

The binary to install is a debug build or speaks another protocol. Build it
with `cargo build --release -p devlanes-hosts-helper` and set
`DEVLANES_HOSTS_HELPER` to the resulting `target/release/devlanes-hosts`.

### A name is in the block but a program still cannot reach it

Browsers keep their own DNS cache for about a minute; Chrome's can be cleared
at `chrome://net-internals/#dns`. On Linux, `nscd` caches `/etc/hosts` answers;
run `sudo nscd -i hosts`. Programs inside containers read the container's
`/etc/hosts`, not the host's.

## Development

```sh
cargo test -p devlanes-hosts-helper
```

The tests run the debug binary with `--hosts-file` against files in a temp
directory, and are compiled out of release test runs. The full install, which
uses sudo for real, is an ignored test in `devlanes-resolver-hosts`:

```sh
cargo build --release -p devlanes-hosts-helper
DEVLANES_HOSTS_HELPER=$PWD/target/release/devlanes-hosts \
  cargo test -p devlanes-resolver-hosts --test it -- --ignored
```

Bump `PROTOCOL` in `src/lib.rs` whenever arguments, input or output change in a
way an older daemon or helper would misread.

## License

Apache-2.0

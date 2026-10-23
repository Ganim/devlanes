# devlanes-resolver-hosts

This crate implements the `/etc/hosts` fallback for resolvers that cannot handle
`*.localhost` names (macOS 15 and older, musl, pure-Go binaries, workerd). The
daemon uses it to decide which routed names to write and to send them to the
root helper from `devlanes-hosts-helper` through `sudo -n`. The CLI uses it for
`devlanes certs trust --hosts` and `devlanes certs untrust --hosts`.

## Usage

```rust
use std::sync::Arc;
use std::time::Duration;

use devlanes_resolver_hosts::{
    HostsClient, HostsFileMode, HostsFilePolicy, HostsSync, SyncConfig, SyncState,
    SystemExecutor, SystemFs, install, probe_localhost,
};

// Daemon
let policy = HostsFilePolicy::new(Some(HostsFileMode::Auto)); // hostnames.hosts_file
let client = HostsClient::new(Arc::new(SystemExecutor), Arc::new(SystemFs), None)?;
let resolution = probe_localhost(Duration::from_secs(2)).await;

// One entry per project: its lanes.hosts_file hint and its routed hostnames.
let desired = policy.desired(
    resolution,
    client.suffixes(),
    [(Some(HostsFileMode::Always), ["web.acme-2.localhost", "api.acme-2.localhost"])],
);

let sync = HostsSync::spawn(client, SyncConfig::default());
sync.update(desired.names); // on every route change; bursts are coalesced
if let SyncState::Failed(error) = &*sync.state().borrow() {
    eprintln!("{}: {error}", error.code()); // helper_missing, sudo_denied, ...
}
sync.shutdown().await;

// CLI, in the user's terminal
let source = install::locate_helper_source(
    &SystemFs,
    std::env::var_os(install::HELPER_SOURCE_ENV).map(Into::into),
    &std::env::current_exe()?,
)?;
install::install(&SystemExecutor, &SystemFs, &source).await?; // sudo prompts here
let warnings = install::uninstall(&SystemExecutor, &SystemFs).await?;
```

## API

- `HostsFilePolicy`: the user's `hosts_file` mode plus project hints; `desired` returns the names to write and the ones skipped, `needs_probe` says whether the probe matters.
- `probe_localhost`: resolves a random `*.localhost` name with `getaddrinfo` on the blocking pool.
- `filter_names`: lowercases names and drops wildcards, names outside the allowed suffixes and anything over the 4096-name cap.
- `HostsClient`: `apply`, `clear` (through `sudo -n`), `status` and `version` (without sudo); `HostsError::code` gives a stable error string.
- `HostsSync`: background task that debounces updates, skips unchanged sets, and backs off after `sudo_denied`; `retry` clears the backoff.
- `install`: `install`, `uninstall`, and the plain-data `install_plan` and `uninstall_plan` with the exact argv.
- `Executor` and `FsProbe`: injectable command runner and `lstat`; the `fake` feature adds `FakeExecutor` and `FakeFs` for argv tests.

## Config example

```yaml
# ~/.devlanes/config.yaml
hostnames:
  # Write exact hostnames to /etc/hosts: auto only when the system resolver fails a probe.
  hosts_file: auto
```

```yaml
# devlanes.yaml
lanes:
  # This project always needs the file, for workerd, which cannot resolve *.localhost.
  hosts_file: always
```

With both, the project's routed names are written on every route change, even where `*.localhost` resolves; another project on the same machine is written only when the probe fails.

## Behaviour

- `never` in the user config wins over every project hint. Otherwise a project's `lanes.hosts_file` replaces the user's mode, and `auto` writes names only when the probe fails.
- The helper only accepts names under `localhost`, plus a TLD under `localhost`, `test`, `internal` or `home.arpa`. `HostsClient::new` rejects any other TLD before running sudo.
- `install` refuses debug builds of the helper, and refuses directories on the install path that are not root-owned or that others can write.
- `install` works from a private copy of the helper and compares the root-owned staged helper and rule with what it checked before activating either; a failure during activation restores the previous helper and rule.
- On Windows only `probe_localhost` and the policy are used: Windows answers `*.localhost` itself, the daemon never writes the hosts file, and the helper commands fail as unsupported.

The helper's README covers what gets installed, the sudo rule and the security
model: [`crates/hosts-helper/README.md`](../hosts-helper/README.md).

## License

Apache-2.0

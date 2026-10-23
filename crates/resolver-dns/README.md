# devlanes-resolver-dns

The embedded DNS server behind the `resolver` hostname strategy, plus the OS files (Windows: NRPT rules) that send one TLD to it. The daemon runs the server; `devlanes certs trust --resolver` writes the generated file with sudo, or on Windows adds the rule through PowerShell after the administrator prompt.

## Usage

```rust
use devlanes_resolver_dns::{DnsError, DnsServer, Zone, default_listen, macos_resolver};
use tokio_util::sync::CancellationToken;

async fn serve_test_tld(shutdown: CancellationToken) -> Result<(), DnsError> {
    let zone = Zone::new("test")?;
    let file = macos_resolver(&zone, default_listen());
    println!("write {} as root:\n{}", file.path.display(), file.contents);

    let server = DnsServer::bind(default_listen(), zone).await?;
    server.serve(shutdown).await;
    Ok(())
}
```

## API

- `Zone`: a validated TLD. It must be `test`, `internal` or `home.arpa`, a domain under one of them, or a domain under `localhost`.
- `DnsServer`: UDP and TCP on one port, `default_listen()`: `127.0.0.1:15353` on macOS and Linux, `127.53.0.1:53` on Windows. Port 0 picks a free port for tests.
- `answer`: builds the response to one DNS message, without sockets.
- `macos_resolver`: `/etc/resolver/<tld>`. macOS picks it up without a reload.
- `systemd_resolver`: a `dns-delegate.d` file on systemd 258 and later, `DnsError::SystemdTooOld` before that.
- `ResolverFile`, `is_managed` and `generated_file`: the file to write, and checks that an existing file or a recorded path belongs to devlanes.
- `nrpt` (Windows): the PowerShell scripts that add, remove and list NRPT rules, and `NrptRules::parse` for the listing.

## Config example

```yaml
# ~/.devlanes/config.yaml
hostnames:
  # Serve lane hostnames from the embedded DNS server instead of *.localhost.
  resolver: resolver
  # TLD sent to that server; required with the resolver strategy.
  tld: test
```

Lane domains then default to `<project>.test` and `<project>-<N>.test` unless a project sets `lanes.vars.domain`.

## Behaviour

Only names under the configured TLD are answered: A with `127.0.0.1`, AAAA with `::1`, and empty NOERROR for other record types. Every other name gets REFUSED. Other TLDs are refused because they can belong to real sites.

On Linux the strategy needs systemd 258 or newer. Older systemd-resolved can only add the server as a global DNS server, so anyone holding port 15353 while the daemon is down would answer every lookup on the machine.

Every generated file starts with `# Managed by devlanes`. The CLI refuses to overwrite or remove a file without that line, such as an `/etc/resolver/test` written by Valet or puma-dev.

`dig` and `nslookup` skip `/etc/resolver` on macOS. Query the server directly with `dig @127.0.0.1 -p 15353 app.lane.test`, or check the system path with `dscacheutil -q host -a name app.lane.test`.

## Windows

Windows routes the TLD with a Name Resolution Policy Table rule: namespace `.<tld>`, name server `127.53.0.1`, comment starting with `# Managed by devlanes`. NRPT rules cannot name a port, so the server listens on port 53 of its own loopback address, since `127.0.0.1:53` is often taken by Internet Connection Sharing, Docker or WSL.

The rule is added and removed in Windows PowerShell started through the administrator prompt; reading it for `doctor` needs no admin. A rule for the same namespace without the devlanes comment belongs to someone else: install refuses and removal leaves it. While Group Policy NRPT rules exist, Windows ignores every local rule, and `doctor` says so.

`nslookup` ignores NRPT. Check the system path with `Resolve-DnsName app.lane.test`, or query the server directly with `nslookup app.lane.test 127.53.0.1`.

The elevated scripts pin `PSModulePath` to `$PSHOME\Modules` and import DnsClient by path, so a module planted in the user's Documents folder cannot run as administrator. A profiler named by `COR_PROFILER` in the user's environment still loads into the elevated PowerShell; closing that needs a native writer, which is future work.

Chrome and Edge in their default "automatic" secure DNS mode use the system resolver while any NRPT rule exists (read from the Chromium source, not tried in a browser). With a secure DNS provider chosen explicitly, `*.<tld>` names fail.

## License

Apache-2.0

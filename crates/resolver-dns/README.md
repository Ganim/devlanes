# devlanes-resolver-dns

The embedded DNS server behind the `resolver` hostname strategy, plus the OS files that send one TLD to it. The daemon runs the server; `devlanes certs trust --resolver` writes the generated file with sudo.

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
- `DnsServer`: UDP and TCP on one port, `127.0.0.1:15353` by default. Port 0 picks a free port for tests.
- `answer`: builds the response to one DNS message, without sockets.
- `macos_resolver`: `/etc/resolver/<tld>`. macOS picks it up without a reload.
- `systemd_resolver`: a `dns-delegate.d` file on systemd 258 and later, `DnsError::SystemdTooOld` before that.
- `ResolverFile`, `is_managed` and `generated_file`: the file to write, and checks that an existing file or a recorded path belongs to devlanes.

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

## License

Apache-2.0

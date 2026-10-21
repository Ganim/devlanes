# devlanes-resolver-dns

The embedded DNS server behind the `resolver` hostname strategy, plus the OS files that send one TLD to it. The daemon runs the server; the CLI writes the generated file with sudo.

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

- `Zone`: a validated TLD such as `test` or `dev.internal`. `local` and `localhost` are refused.
- `DnsServer`: UDP and TCP on one port, `127.0.0.1:15353` by default. Port 0 picks a free port for tests.
- `answer`: builds the response to one DNS message, without sockets.
- `macos_resolver`: `/etc/resolver/<tld>`. macOS picks it up without a reload.
- `systemd_resolver`: a `dns-delegate.d` file on systemd 258 and later, a `resolved.conf.d` drop-in on 246 to 257, and `DnsError::SystemdTooOld` below that.
- `ResolverFile`: the target path, the contents and the command to run after writing it.

## Answers

Only names under the configured TLD are answered: A with `127.0.0.1`, AAAA with `::1`, and empty NOERROR for other record types. Every other name gets REFUSED.

`dig` and `nslookup` skip `/etc/resolver` on macOS. Query the server directly with `dig @127.0.0.1 -p 15353 app.lane.test`, or check the system path with `dscacheutil -q host -a name app.lane.test`.

## License

Apache-2.0

# devlanes-pki

Per-user CA storage and TLS certificate issuance for devlanes. The proxy supplies route authorization to the certificate resolver; CLI trust integrations consume the public root certificate.

## Usage

```rust
use devlanes_pki::{CertificateAuthority, CertificatePolicy, DynamicCertResolver};
use std::{path::Path, sync::Arc};

fn tls_config(
    ca_directory: &Path,
    routes: Arc<dyn CertificatePolicy>,
) -> Result<Arc<rustls::ServerConfig>, devlanes_pki::Error> {
    let ca = Arc::new(CertificateAuthority::load_or_create(ca_directory)?);
    Arc::new(DynamicCertResolver::new(ca, routes)).server_config()
}
```

CA loading performs blocking filesystem I/O. Run it on a blocking thread when called from async code; the CA directory's parent must already exist.

## API

- `CertificateAuthority::load_or_create_with_tld` also permits a configured TLD when it creates a CA.
- `CertificateAuthority::from_mkcert` loads an existing mkcert CAROOT without invoking mkcert.
- `certificate_pem` and `certificate_der` expose the public root for trust configuration.
- `combined_bundle` appends the root to caller-supplied PEM roots without writing a file.
- `CertificatePolicy::lookup` supplies authorized SANs, the owning `project@lane` and a route generation for cache invalidation.

## Config example

```yaml
# ~/.devlanes/config.yaml
tls:
  # devlanes keeps its own CA under ~/.devlanes/ca; mkcert reuses mkcert's CAROOT.
  ca: mkcert
```

With `mkcert`, the daemon loads mkcert's root from `mkcert -CAROOT` through `CertificateAuthority::from_mkcert` and the proxy signs leaf certificates with it; `devlanes certs trust` installs that same root. The combined CA bundle for child processes is still written under `~/.devlanes/ca`.

## Behaviour

New CA directories use mode `0700`; key files use `0600`. Loading rejects mismatched pairs and a certificate without its key. The key is written before the certificate, so a key without a certificate is treated as an interrupted creation and replaced by a new CA. This crate does not modify system trust stores.

## Name constraints

New devlanes CAs carry X.509 name constraints that permit DNS names under `localhost`, `test`, `internal` and `home.arpa`, plus the TLD passed to `load_or_create_with_tld`. Clients that enforce constraints reject leaves for any other name, so a leaked CA key cannot impersonate public sites. CAs created before this change, and mkcert roots, have no constraints and keep working. To get the constraints, or to pick up a TLD configured later, recreate the CA: run `devlanes certs untrust`, delete the `ca` directory under the devlanes home, run `devlanes certs trust` to create and trust the new root, then `devlanes daemon restart` so the proxy signs with it. `certs untrust` alone keeps the old root on disk.

The dynamic resolver issues leaf certificates only for names authorized by its policy and refuses unknown or missing SNI. Direct callers of `CertificateAuthority::issue` must enforce authorization themselves. Leaves expire within 30 days, and the resolver rechecks authorization on cache hits.

## Certificate subjects

Leaves use the first SAN (the requested host) as common name, `devlanes` as organization and the owning `project@lane` as organizational unit, or `local development` when the policy names no owner. New CAs keep the common name `devlanes local development CA`, which macOS untrust searches by, with organization `devlanes`, organizational unit `user@hostname` and `https://devlanes.run` as a URI subject alternative name. An existing CA keeps its old subject and keeps working; recreate it as described under name constraints to pick up the new one.

## License

Apache-2.0

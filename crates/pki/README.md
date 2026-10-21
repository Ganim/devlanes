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

- `CertificateAuthority::from_mkcert` loads an existing mkcert CAROOT without invoking mkcert.
- `certificate_pem` and `certificate_der` expose the public root for trust configuration.
- `combined_bundle` appends the root to caller-supplied PEM roots without writing a file.
- `CertificatePolicy::lookup` supplies authorized SANs and a route generation for cache invalidation.

## Config example

```yaml
# ~/.devlanes/config.yaml
tls:
  # devlanes keeps its own CA under ~/.devlanes/ca; mkcert reuses mkcert's CAROOT.
  ca: mkcert
```

With `mkcert`, `devlanes trust` installs mkcert's root through `CertificateAuthority::from_mkcert`. The daemon still loads or creates its own CA under `~/.devlanes/ca` for the proxy, so the setting does not change which root signs leaf certificates yet.

## Behaviour

New CA directories use mode `0700`; key files use `0600`. Loading rejects incomplete or mismatched certificate/key pairs. This crate does not modify system trust stores.

The dynamic resolver issues leaf certificates only for names authorized by its policy and refuses unknown or missing SNI. Direct callers of `CertificateAuthority::issue` must enforce authorization themselves. Leaves expire within 30 days, and the resolver rechecks authorization on cache hits.

## License

Apache-2.0

# devlanes-trust-windows

Windows implementation of `devlanes-trust`: the current user's `Root` certificate store for the `system`, `chrome`, `firefox` and `windows` targets, plus the roots of `bundle.pem`. The composition root registers it on Windows; the CLI uses the store for `devlanes certs trust`, and the daemon calls `bundle_roots` when it writes the CA bundle.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustTarget};
use devlanes_trust_windows::{DEVLANES_CA_SUBJECT, WindowsTrust};

async fn trust_user_root(ca_pem: &str) -> Result<(), TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    let trust = WindowsTrust::default().with_ca_subject(DEVLANES_CA_SUBJECT);
    trust.store(TrustTarget::System)?.install(&ca).await
}

fn bundle(own_der: &[u8]) -> String {
    devlanes_trust_windows::bundle_roots(own_der).pem
}
```

## API

- `WindowsTrust`: builds the store for each `TrustTarget`. `with_stores` points it at other CurrentUser stores, which tests use for scratch stores.
- `STORE_ID`: shared by `system`, `chrome`, `firefox` and `windows`, so the CLI changes the store once per command.
- `bundle_roots`: Mozilla's roots plus the usable roots of the Windows store, minus `Disallowed` and the devlanes CA.

## Behaviour

Status reads the logical `Cert:\CurrentUser\Root`, which includes the machine's roots, and counts a CA that `Disallowed` (Untrusted Certificates) also holds as missing. Adds and deletes go only to the user's registry store, so even an elevated run never changes `Cert:\LocalMachine\Root`. Uninstall also removes older devlanes CAs found by subject.

Windows shows a security warning for every add and delete in the user's `Root`. Without a desktop (SSH, a service) the change fails with `TrustError::InteractionRequired` before any dialog, and so does a No in the dialog. Chrome and Edge read this store; Firefox 120 and later reads it through `security.enterprise_roots.enabled`. `ios-simulator` and `android-emulator` are unsupported.

The tests change only scratch stores under `HKCU\Software\Microsoft\SystemCertificates`, which never prompt. The one test that changes the real store is ignored with "needs a desktop session".

The crate is empty on other platforms.

## License

Apache-2.0

# devlanes-trust-windows

Windows implementation of `devlanes-trust`: the current user's `Root` certificate store for the `system`, `chrome` and `firefox` targets. The composition root registers it on Windows, and the CLI uses it for `devlanes certs trust` and `devlanes certs untrust`. The crypt32 calls live in `devlanes-sys`.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustTarget};
use devlanes_trust_windows::{DEVLANES_CA_SUBJECT, WindowsTrust};

async fn trust_user_root(ca_pem: &str) -> Result<(), TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    let store = WindowsTrust::from_env()
        .with_ca_subject(DEVLANES_CA_SUBJECT)
        .store(TrustTarget::System)?;
    store.install(&ca).await
}
```

## API

- `WindowsTrust`: builds the store for each `TrustTarget` from a store name (`ROOT` in production, a scratch store in tests) and the Firefox profile roots.
- `UserRootStore`: one CurrentUser store. `with_subject` also removes older devlanes CAs.
- `FirefoxStore`: the same store reported as Firefox, with a hint when a profile turned `security.enterprise_roots.enabled` off.
- `STORE_ID`: shared by `system` and `chrome`, so the CLI changes the store once.

## Behaviour

- install: adds the CA to `Cert:\CurrentUser\Root` unless it is already trusted, then deletes older certificates whose subject contains `devlanes local development CA`.
- uninstall: deletes the CA (and, for devlanes' own CA, every older one) from the user's own registry store only.
- status: looks the CA up by SHA-1 thumbprint in the logical store, which also holds `Cert:\LocalMachine\Root`. A CA an administrator trusted machine-wide counts as installed, and `uninstall` reports that it stays trusted.

Windows shows a security warning for every root added to or deleted from the user's `Root` store and waits for Yes. No flag suppresses it and no elevation is needed. A No fails with `TrustError::Cancelled`. Without a desktop (SSH, services, containers) the change fails with `TrustError::InteractionRequired` before any dialog.

Chrome and Edge use the Windows store. Firefox imports it at startup while `security.enterprise_roots.enabled` is true, its default; profiles under `%APPDATA%\Mozilla\Firefox\Profiles` are only read, never changed. An enterprise policy (`Certificates.ImportEnterpriseRoots`) can also turn the import off; that is not detected. `ios-simulator` and `android-emulator` are unsupported.

The crate is empty on other platforms.

## License

Apache-2.0

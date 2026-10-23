# devlanes-trust-windows

Windows implementation of `devlanes-trust`: the current user's `Root` certificate store for the `system`, `chrome`, `firefox` and `windows` targets. The composition root registers it on Windows, and the CLI uses it for `devlanes certs trust` and `devlanes certs untrust`. The crypt32 calls live in `devlanes-sys`.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustTarget};
use devlanes_trust_windows::{DEVLANES_CA_SUBJECT, WindowsTrust};

async fn trust_user_root(ca_pem: &str) -> Result<(), TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    let store = WindowsTrust::from_env()
        .with_ca_subject(DEVLANES_CA_SUBJECT)
        .store(TrustTarget::System)?;
    if let Some(dialog) = store.pending_approval(&ca, true).await {
        println!("{dialog}");
    }
    store.install(&ca).await
}
```

## API

- `WindowsTrust`: builds the store for each `TrustTarget` from a store name (`ROOT` in production, a scratch store in tests).
- `UserRootStore`: one CurrentUser store. `with_subject` makes uninstall also remove older devlanes CAs.
- `STORE_ID`: the id every target shares, so the CLI changes the store and shows its dialog once.

## Behaviour

Chrome, Edge and Firefox all read this store (Firefox 120 and later read it through `security.enterprise_roots.enabled`), so every browser target is the same store. A Firefox profile or policy that turns `security.enterprise_roots.enabled` off does not trust the devlanes CA, and `devlanes doctor` cannot see that. Status counts a CA trusted in `Cert:\LocalMachine\Root` too, and reports one the `Disallowed` store holds as distrusted. Adds and deletes touch only the user's registry store, so an elevated run cannot change the machine's.

Windows shows a security warning for every root added to or deleted from `Root` and waits for Yes; no elevation is needed. A No is `TrustError::Cancelled`, and without a desktop (SSH, services) the change fails with `TrustError::InteractionRequired` before any dialog.

Tests use scratch `devlanes-test-*` stores, which never prompt. The one test against the real `Root` is ignored: it needs a desktop and a click on each dialog.

The crate is empty on other platforms.

## License

Apache-2.0

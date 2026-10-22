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
    if let Some(dialog) = store.pending_approval(&ca, true).await {
        println!("{dialog}");
    }
    store.install(&ca).await
}
```

## API

- `WindowsTrust`: builds the store for each `TrustTarget` from a store name (`ROOT` in production, a scratch store in tests) and the Firefox folders.
- `UserRootStore`: one CurrentUser store. `with_subject` makes uninstall also remove older devlanes CAs.
- `FirefoxStore`: the same store reported as Firefox, with a hint when a profile turned `security.enterprise_roots.enabled` off.
- `FirefoxDirs`: `%APPDATA%` and `%LOCALAPPDATA%`, where Firefox profiles are found.
- `STORE_ID`: shared by `system` and `chrome`, and what `firefox` changes (`TrustStore::changes`), so the CLI changes the store and shows its dialog once.

## Behaviour

- install: adds the CA to `Cert:\CurrentUser\Root` unless it is already in a root store this user trusts. It removes nothing; older devlanes CAs stay until uninstall, as on macOS.
- uninstall: deletes the CA (and, for devlanes' own CA, every certificate whose subject contains `devlanes local development CA`) from the user's own registry store. A CA that stays trusted through `Cert:\LocalMachine\Root` or a group policy store fails with `TrustError::StillTrusted`.
- status: looks the CA up by SHA-1 thumbprint in the logical store, which also holds `Cert:\LocalMachine\Root`, so a CA an administrator trusted machine-wide counts as installed. A CA the logical `Disallowed` store also holds is `TrustStatus::Distrusted`.

Adds and deletes go through `CERT_STORE_PROV_SYSTEM_REGISTRY_W`, which opens only the user's `HKEY_CURRENT_USER` store, so an elevated run cannot change the machine's `Root`. The logical view is read-only.

Windows shows a security warning for every root added to or deleted from the user's `Root` store and waits for Yes: "Security Warning" to add, "Root Certificate Store" to delete. No flag suppresses it and no elevation is needed. A No fails with `TrustError::Cancelled`. Without a desktop (SSH, services, containers) the change fails with `TrustError::InteractionRequired` naming `devlanes certs trust` or `untrust`, before any dialog. That these dialogs appear for an add through the registry view is confirmed only by the ignored `real_root_install_status_uninstall` test, which needs a desktop and a click on each dialog.

Chrome and Edge use the Windows store. Firefox imports it at startup while `security.enterprise_roots.enabled` is true, its default. Profiles are those `profiles.ini` lists (or every folder under `Profiles` without it) in `%APPDATA%\Mozilla\Firefox` and, for the Microsoft Store build, `%LOCALAPPDATA%\Packages\Mozilla.Firefox_*\LocalCache\Roaming\Mozilla\Firefox`; they are only read, never changed. A `user_pref` in `user.js` wins over one in `prefs.js`, and either wins over `pref` or `sticky_pref`. An enterprise policy (`Certificates.ImportEnterpriseRoots`) can also turn the import off; that is not detected. `ios-simulator` and `android-emulator` are unsupported.

## Tests

The tests use scratch stores named `devlanes-test-*`, created under `HKCU\Software\Microsoft\SystemCertificates` and deleted by a guard when the test ends or panics. Only `Root` prompts, so they run in the normal test suite without elevation or dialogs. The one test against the real `Root` is ignored; run it from a desktop session and choose Yes in each dialog. It also removes every devlanes CA from your user root store, so run `devlanes certs trust` afterwards.

The crate is empty on other platforms.

## License

Apache-2.0

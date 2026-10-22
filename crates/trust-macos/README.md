# devlanes-trust-macos

macOS implementations of `devlanes-trust`: the login keychain for the `system` and `chrome` targets, and NSS for Firefox profiles. The composition root registers it on macOS, and the CLI uses it for `devlanes certs trust`.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustTarget};
use devlanes_trust_macos::MacosTrust;

async fn trust_login_keychain(ca_pem: &str) -> Result<(), TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    let store = MacosTrust::from_env()?.store(TrustTarget::System)?;
    store.install(&ca).await
}
```

## API

- `MacosTrust`: builds the store for each `TrustTarget` from an executor and a home directory.
- `KeychainStore`: one keychain plus user trust settings. Tests pass a throwaway keychain.
- `KEYCHAIN_ID`: shared by `system` and `chrome`, so the CLI installs into the keychain once.

## Behaviour

All commands run `/usr/bin/security` directly:

- install: `add-trusted-cert -r trustRoot -k ~/Library/Keychains/login.keychain-db <ca.pem>`
- uninstall: `delete-certificate -Z <sha1> -t <keychain>`, which also drops the trust settings. With `with_ca_label`, every certificate found by `find-certificate -a -c <label> -Z` is deleted too, so older devlanes CAs do not stay trusted.
- status: `find-certificate -a -Z <keychain>`, then `verify-cert -c <ca.pem> -l -L -q`

Trust settings go to the user domain, so no sudo is needed. macOS still asks for the account password in a dialog on install and removal. Over SSH without a desktop session the change fails with `TrustError::InteractionRequired`.

Firefox needs `certutil` (`brew install nss`). Firefox 120 and later also reads user-added keychain roots.

The crate builds on every Unix platform so its argv tests run in Linux CI too. It is empty on Windows.

## License

Apache-2.0

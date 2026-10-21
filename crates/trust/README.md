# devlanes-trust

The `TrustStore` trait and shared types for installing the devlanes CA into OS and browser trust stores. `devlanes-trust-macos` and `devlanes-trust-linux` implement it, and the CLI calls it for `devlanes trust` and `devlanes untrust`. The NSS store used by Firefox and Chromium on both platforms is also here.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore};

async fn trust(store: &dyn TrustStore, ca_pem: &str) -> Result<TrustStatus, TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    println!("installing into {}", store.describe());
    store.install(&ca).await?;
    store.status(&ca).await
}
```

## API

- `TrustStore`: `install`, `uninstall` and `status` for one store. Repeating an install or removing an absent CA succeeds without changes.
- `TrustTarget`: the `tls.trust` names (`system`, `firefox`, `chrome`, `ios-simulator`, `android-emulator`).
- `TrustStatus`: `Installed`, `Missing` or `Unknown { reason }`.
- `CaCertificate`: the public CA plus the name stores file it under. It keeps only the first certificate of the PEM, so a key in the input never reaches a store.
- `CommandExecutor` and `SystemExecutor`: every external command goes through this trait. The `fake` feature adds `fake::FakeExecutor`, which records argv for tests.
- `nss::NssStore`: Firefox profiles or a shared NSS database, changed through `certutil`.
- `TrustError`: typed failures, including `Unsupported`, `ToolMissing { hint }` and `InteractionRequired`.

## Notes

Trust changes run in the CLI process because they prompt for a password. The daemon never calls a `TrustStore`.

`ios-simulator` and `android-emulator` are not supported in v1 and always return `TrustError::Unsupported`.

NSS stores need `certutil`. When it is missing they fail with `ToolMissing` and an install hint, and `status` reports `Unknown`.

## License

Apache-2.0

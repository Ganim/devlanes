# devlanes-trust

The `TrustStore` trait and shared types for installing the devlanes CA into OS and browser trust stores. `devlanes-trust-macos`, `devlanes-trust-linux` and `devlanes-trust-windows` implement it, and the CLI calls it for `devlanes certs trust` and `devlanes certs untrust`. The NSS store used by Firefox and Chromium on macOS and Linux is also here.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustStatus, TrustStore};

async fn trust(store: &dyn TrustStore, ca_pem: &str) -> Result<TrustStatus, TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    println!("installing into {}", store.describe());
    if let Some(approval) = store.pending_approval(&ca, true).await {
        println!("{approval}");
    }
    store.install(&ca).await?;
    store.status(&ca).await
}
```

## API

- `TrustStore`: `install`, `uninstall` and `status` for one store. Repeating an install or removing an absent CA succeeds without changes. `location`, `approval`, `pending_approval` and `hint` add text for the CLI: where the CA goes, the dialog to expect (in general, and for the change about to run), and a setting that keeps the store from working. `id` names the store for report lines and `changes` what `install` and `uninstall` change, so the CLI runs a shared change once.
- `TrustTarget`: the `tls.trust` names (`system`, `firefox`, `chrome`, `windows`, `ios-simulator`, `android-emulator`). `windows` is the Windows user root store: reached through interop from WSL, the same store as `system` on Windows, `Unavailable` elsewhere.
- `TrustStatus`: `Installed`, `Missing`, `Distrusted { reason }` (installed but overridden by a deny list) or `Unknown { reason }`.
- `CaCertificate`: the public CA plus the name stores file it under. It keeps only the first certificate of the PEM, so a key in the input never reaches a store. `is_name_constrained` tells an older, unconstrained devlanes CA apart.
- `find_system_program`: absolute path of a root-run tool from the system directories (`system_program_dirs`), never `PATH`. On Windows that is the System32 directory from `GetSystemDirectoryW`, and names may leave out `.exe` or `.com`.
- `CommandExecutor` and `SystemExecutor`: every external command goes through this trait. The `fake` feature adds `fake::FakeExecutor`, which records argv for tests.
- `nss::NssStore` (Unix): Firefox profiles or a shared NSS database, changed through `certutil`.
- `TrustError`: typed failures, including `Unsupported`, `Unavailable { reason }` (a target this machine cannot reach), `ToolMissing { hint }`, `InteractionRequired`, `Cancelled` and `StillTrusted`.

## Config example

```yaml
# ~/.devlanes/config.yaml
tls:
  # Stores that devlanes certs trust installs the CA into and devlanes certs untrust removes it from.
  trust: [system, firefox, chrome]
```

Without `tls.trust`, `devlanes certs trust` uses only the `system` store, and inside WSL with interop `system` and `windows`.

## Behaviour

Trust changes run in the CLI process because they prompt for a password or a confirmation. The daemon never calls a `TrustStore`.

`ios-simulator` and `android-emulator` are not supported in v1 and always return `TrustError::Unsupported`.

NSS stores need `certutil`. When it is missing they fail with `ToolMissing` and an install hint, and `status` reports `Unknown`.

## License

Apache-2.0

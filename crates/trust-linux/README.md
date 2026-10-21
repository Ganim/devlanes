# devlanes-trust-linux

Linux implementations of `devlanes-trust`: the distribution CA bundle for `system`, `~/.pki/nssdb` for `chrome`, and Firefox profiles for `firefox`. The composition root registers it on Linux, and the CLI uses it for `devlanes trust`.

## Usage

```rust
use devlanes_trust::{CaCertificate, TrustError, TrustTarget};
use devlanes_trust_linux::LinuxTrust;

async fn trust_system(ca_pem: &str) -> Result<(), TrustError> {
    let ca = CaCertificate::from_pem(ca_pem, "devlanes development CA")?;
    let store = LinuxTrust::from_env()?.store(TrustTarget::System)?;
    store.install(&ca).await
}
```

## API

- `LinuxTrust`: builds the store for each `TrustTarget`. `with_sysroot` lets tests lay out anchor directories in a tempdir.
- `Privilege`: `Root`, or `Sudo { interactive }` chosen by `Privilege::detect`.
- `CaStoreLayout`: Fedora, Debian, Arch and openSUSE anchor directories, detected in that order.
- `SystemStore`: an anchor file plus the distribution's refresh command.

## Behaviour

The system store runs, through sudo unless already root:

- Debian and Ubuntu: `install -m 0644 <ca.pem> /usr/local/share/ca-certificates/<name>.crt`, then `update-ca-certificates`
- Fedora and RHEL: `install -m 0644 <ca.pem> /etc/pki/ca-trust/source/anchors/<name>.pem`, then `update-ca-trust extract`
- uninstall: `rm -f <anchor>`, then the same refresh command

With a terminal on stdin, sudo prompts for a password. Without one it runs `sudo -n`, so CI with passwordless sudo works and a required password fails with `TrustError::InteractionRequired` instead of hanging.

The NSS stores run `certutil` as the user and need `libnss3-tools` (Debian/Ubuntu), `nss-tools` (Fedora) or `nss` (Arch). The `chrome` target creates `~/.pki/nssdb` when it is missing. Playwright's Chromium reads it too.

## License

Apache-2.0

# devlanes-trust-linux

Linux implementations of `devlanes-trust`: the distribution CA bundle for `system`, `~/.pki/nssdb` for `chrome`, Firefox profiles for `firefox`, and inside WSL the Windows user root store for `windows`. The composition root registers it on Linux, and the CLI uses it for `devlanes certs trust`.

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
- `WindowsStore`: the Windows user root store through Windows' `certutil.exe`. `with_windows_certutil` replaces the WSL detection in tests.

## Behaviour

The system store runs, through sudo unless already root:

- Debian and Ubuntu: `install -m 0644 <ca.pem> /usr/local/share/ca-certificates/<name>.crt`, then `update-ca-certificates`
- Fedora and RHEL: `install -m 0644 <ca.pem> /etc/pki/ca-trust/source/anchors/<name>.pem`, then `update-ca-trust extract`
- uninstall: `rm -f <anchor>`, then the same refresh command

`sudo` and every root-run tool are resolved to absolute paths in `/usr/sbin`, `/usr/bin`, `/sbin` and `/bin`; the user's `PATH` is never searched for them.

With a terminal on stdin, sudo prompts for a password. Without one it runs `sudo -n`, so CI with passwordless sudo works and a required password fails with `TrustError::InteractionRequired` instead of hanging.

The NSS stores run `certutil` as the user and need `libnss3-tools` (Debian/Ubuntu), `nss-tools` (Fedora) or `nss` (Arch). The `chrome` target creates `~/.pki/nssdb` when it is missing. Playwright's Chromium reads it too.

The `windows` target works only inside WSL with interop on. It runs Windows' `certutil.exe -user` from the System32 of a mounted drive: `-verifystore Root <sha1>` for status, which shows no dialog, and `-addstore Root` (with the temp PEM's path from `wslpath -w`) or `-delstore Root <sha1>` for changes, which Windows confirms in a security warning on the desktop. On native Linux and without interop it fails with `TrustError::Unavailable`.

The crate builds on every Unix, so its argv tests also run on macOS, and compiles to an empty library on Windows.

## License

Apache-2.0

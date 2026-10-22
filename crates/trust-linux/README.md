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
- `WindowsInterop`, `WindowsInteropStore`: the `windows` target. `WindowsInterop::detect` finds `certutil.exe` under the WSL drive mounts, or says why there is none; `LinuxTrust::with_windows` hands it in.

## Behaviour

The system store runs, through sudo unless already root:

- Debian and Ubuntu: `install -m 0644 <ca.pem> /usr/local/share/ca-certificates/<name>.crt`, then `update-ca-certificates`
- Fedora and RHEL: `install -m 0644 <ca.pem> /etc/pki/ca-trust/source/anchors/<name>.pem`, then `update-ca-trust extract`
- uninstall: `rm -f <anchor>`, then the same refresh command

`sudo` and every root-run tool are resolved to absolute paths in `/usr/sbin`, `/usr/bin`, `/sbin` and `/bin`; the user's `PATH` is never searched for them.

With a terminal on stdin, sudo prompts for a password. Without one it runs `sudo -n`, so CI with passwordless sudo works and a required password fails with `TrustError::InteractionRequired` instead of hanging.

The NSS stores run `certutil` as the user and need `libnss3-tools` (Debian/Ubuntu), `nss-tools` (Fedora) or `nss` (Arch). The `chrome` target creates `~/.pki/nssdb` when it is missing. Playwright's Chromium reads it too.

## The windows target under WSL

The `windows` store runs Windows' own `certutil.exe` from the `System32` mount (normally `/mnt/c/Windows/System32`, never `PATH`) through WSL interop:

- status: `certutil.exe -user -store Root <sha1>`, then `certutil.exe -store Root <sha1>` for a copy an administrator put in the machine store
- install: `wslpath -w <temp pem>`, then `certutil.exe -user -addstore Root <\\wsl.localhost\...\pem>`; Windows shows its "Security Warning" dialog
- uninstall: `certutil.exe -user -delstore Root <sha1>`; Windows asks again

Only exit codes, the thumbprint and HRESULT names (`ERROR_CANCELLED`, `NTE_NOT_FOUND`) are read, because the rest of certutil's output is translated. A No in the dialog is `TrustError::Cancelled`. Outside WSL, or with interop off, the target is `TrustError::Unavailable`. Tests point it at a scratch CurrentUser store with `WindowsInterop::with_store`; certutil creates such a store only with `-f`, and it never prompts. `windows_scratch_store_round_trip` (ignored, needs WSL with interop) adds, reads and deletes a CA there and removes the store with `reg.exe`.

## License

Apache-2.0

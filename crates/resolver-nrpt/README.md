# devlanes-resolver-nrpt

The Windows side of the `resolver` hostname strategy: one NRPT (Name Resolution Policy Table) rule that sends lookups under `hostnames.tld` to the daemon's DNS server on `127.53.0.1:53`. `devlanes certs trust --resolver` and `certs untrust --resolver` change the rule; the daemon's `doctor` facts read it. The registry calls go through `devlanes-sys-windows`.

## Usage

```rust
use devlanes_resolver_nrpt::{Nrpt, NrptError, RuleState, zone};

fn trust_resolver(tld: &str, home: &std::path::Path) -> Result<(), NrptError> {
    let zone = zone(tld)?;
    let nrpt = Nrpt::default();
    if let Some(change) = nrpt.plan_install(&zone)? {
        // Shows the UAC prompt, runs `reg.exe import`, then reads the rule back.
        nrpt.apply(&change, home)?;
    }
    assert_eq!(nrpt.status(&zone).map(|s| s.state).ok(), Some(RuleState::Installed));
    Ok(())
}
```

## API

- `Nrpt`: the machine's rules by default; `Nrpt::under` points at other keys, which tests use for scratch keys of the current user.
- `Nrpt::status`: the rule's state (`installed`, `outdated`, `missing`), other programs' rules covering the TLD, devlanes rules for an earlier TLD, and whether Group Policy rules exist. Registry reads only.
- `Nrpt::plan_install` and `Nrpt::plan_removal`: the `.reg` change to make, or `None` when there is nothing to do.
- `Nrpt::apply`: imports a change as administrator and confirms it.
- `zone`: the TLD, validated as `devlanes-resolver-dns` validates it and required in its stored form.

## The rule

The key is `HKLM\SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig\devlanes-<tld>` with the values `Add-DnsClientNrptRule` writes: `Version` 2, `Name` `.<tld>`, `GenericDNSServers` `127.53.0.1`, `ConfigOptions` 8, an empty `IPSECCARestriction`, plus `DisplayName` and `Comment`. The DNS client picks up a change at once, without PowerShell, a cache flush or a service restart. NRPT rules have no port, so the server listens on port 53 on Windows; `127.53.0.1` keeps clear of other local DNS servers on `127.0.0.1:53`.

Windows answers every `*.localhost` name itself, so there is no rule for `localhost`.

Install writes the rule fresh (its key is deleted first) and removes devlanes rules for other TLDs; removal deletes every `devlanes-*` rule. Install refuses when another rule covers the TLD or a name under it, and when Group Policy rules exist under `HKLM\SOFTWARE\Policies\Microsoft\Windows NT\DNSClient\DnsPolicyConfig`, because Windows then ignores local rules.

## Elevation

The change is a generated `.reg` file (UTF-16LE with a byte order mark) imported by `<System32>\reg.exe` through `ShellExecuteExW` with the `runas` verb, which shows the UAC prompt. The TLD can only hold lowercase letters, digits, `-` and `.`, so it cannot add anything to the file. The file is written to the devlanes home, held open read-only with readers-only sharing and checked byte for byte while `reg.exe` runs, so no other program can change what the elevated import reads, and deleted afterwards.

## Tests

The unit tests import generated files with an unelevated `reg.exe` into scratch keys under `HKCU\Software`. The real round trip changes `HKEY_LOCAL_MACHINE`, shows two UAC prompts and needs `127.53.0.1:53` free (no devlanes daemon):

```sh
cargo test -p devlanes-resolver-nrpt --test it -- --ignored
```

The crate is empty on other platforms.

## License

Apache-2.0

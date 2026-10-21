//! OS resolver configuration that forwards the TLD to the embedded server.
//!
//! Only content and target paths are produced. Writing them needs root, which
//! the CLI handles; the daemon never touches these files.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use crate::{DnsError, Zone};

/// systemd release that added `*.dns-delegate` files, the only Linux form devlanes
/// writes. Older releases can only add the server to the global DNS scope, which
/// would hand every lookup on the machine to whoever holds the port while the
/// daemon is down.
pub const DNS_DELEGATE_MIN_SYSTEMD: u32 = 258;

/// First line of every generated file. The CLI only overwrites or deletes a file
/// that starts with it, so files from Valet, puma-dev or the admin stay untouched.
pub const MANAGED_MARKER: &str = "# Managed by devlanes";

/// Directory of macOS per-domain resolver files.
const MACOS_DIR: &str = "/etc/resolver";

/// Directory of systemd-resolved DNS delegates.
const DELEGATE_DIR: &str = "/etc/systemd/dns-delegate.d";

/// A root-owned file to write, plus what makes the OS pick it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverFile {
    /// Absolute target path.
    pub path: PathBuf,
    /// Full file contents.
    pub contents: String,
    /// Command to run as root after writing or removing the file; `None` when the
    /// OS watches the directory itself. The program is a bare name; the caller
    /// resolves it to an absolute path.
    pub activate: Option<Vec<String>>,
}

/// `/etc/resolver/<tld>` for macOS.
///
/// mDNSResponder watches `/etc/resolver`, so no reload is needed. `dig` and
/// `nslookup` bypass this file; use `dscacheutil -q host -a name <host>` to check.
pub fn macos_resolver(zone: &Zone, server: SocketAddr) -> ResolverFile {
    ResolverFile {
        path: PathBuf::from(MACOS_DIR).join(zone.name()),
        contents: format!(
            "{MANAGED_MARKER}: sends *.{tld} lookups to the devlanes DNS server.\n\
             nameserver {ip}\n\
             port {port}\n",
            tld = zone.name(),
            ip = server.ip(),
            port = server.port(),
        ),
        activate: None,
    }
}

/// A systemd-resolved DNS delegate (systemd 258+).
pub fn systemd_dns_delegate(zone: &Zone, server: SocketAddr) -> ResolverFile {
    ResolverFile {
        path: PathBuf::from(DELEGATE_DIR).join(format!("devlanes-{}.dns-delegate", zone.name())),
        contents: format!(
            "{MANAGED_MARKER}: sends *.{tld} lookups to the devlanes DNS server.\n\
             [Delegate]\n\
             DNS={server}\n\
             Domains=~{tld}\n",
            tld = zone.name(),
        ),
        activate: Some(restart_resolved()),
    }
}

/// The systemd-resolved file for `systemd_version`: a DNS delegate, or
/// [`DnsError::SystemdTooOld`] before systemd 258.
pub fn systemd_resolver(
    zone: &Zone,
    server: SocketAddr,
    systemd_version: u32,
) -> Result<ResolverFile, DnsError> {
    if systemd_version >= DNS_DELEGATE_MIN_SYSTEMD {
        Ok(systemd_dns_delegate(zone, server))
    } else {
        Err(DnsError::SystemdTooOld {
            version: systemd_version,
        })
    }
}

/// Whether `contents` is a file this crate generated (its first line is [`MANAGED_MARKER`]).
pub fn is_managed(contents: &str) -> bool {
    contents
        .lines()
        .next()
        .is_some_and(|line| line.starts_with(MANAGED_MARKER))
}

/// The file at `path` as this crate would have generated it, minus contents, or
/// `None` when no generator writes there.
///
/// The CLI records installed paths in a user-writable file; this check keeps a
/// tampered record from naming anything but a devlanes resolver file.
pub fn generated_file(path: &Path) -> Option<ResolverFile> {
    let parent = path.parent()?.to_str()?;
    let name = path.file_name()?.to_str()?;
    let (zone, activate) = match parent {
        MACOS_DIR => (name, None),
        DELEGATE_DIR => (
            name.strip_prefix("devlanes-")?
                .strip_suffix(".dns-delegate")?,
            Some(restart_resolved()),
        ),
        _ => return None,
    };
    // Comparing names rejects aliases such as `TEST` that Zone::new would fold.
    (Zone::new(zone).ok()?.name() == zone).then(|| ResolverFile {
        path: path.to_owned(),
        contents: String::new(),
        activate,
    })
}

/// Restart re-reads delegates on every supported version.
fn restart_resolved() -> Vec<String> {
    ["systemctl", "restart", "systemd-resolved"]
        .map(str::to_owned)
        .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::default_listen;

    #[test]
    fn macos_file() {
        let zone = Zone::new("test").expect("zone");
        let file = macos_resolver(&zone, default_listen());
        assert_eq!(file.path, PathBuf::from("/etc/resolver/test"));
        assert_eq!(
            file.contents,
            "# Managed by devlanes: sends *.test lookups to the devlanes DNS server.\n\
             nameserver 127.0.0.1\nport 15353\n"
        );
        assert_eq!(file.activate, None);
        assert!(is_managed(&file.contents));
    }

    #[test]
    fn systemd_needs_dns_delegates() {
        let zone = Zone::new("dev.internal").expect("zone");
        let server = default_listen();
        let delegate = systemd_resolver(&zone, server, 258).expect("delegate");
        assert_eq!(
            delegate.path,
            PathBuf::from("/etc/systemd/dns-delegate.d/devlanes-dev.internal.dns-delegate")
        );
        assert!(
            delegate
                .contents
                .ends_with("[Delegate]\nDNS=127.0.0.1:15353\nDomains=~dev.internal\n")
        );
        assert_eq!(
            delegate.activate.as_deref(),
            Some(
                &[
                    "systemctl".to_owned(),
                    "restart".into(),
                    "systemd-resolved".into()
                ][..]
            )
        );
        assert!(is_managed(&delegate.contents));

        // A global resolved.conf.d server would answer every lookup on the machine.
        for old in [257, 255, 246, 245] {
            let error = systemd_resolver(&zone, server, old).expect_err("too old");
            assert!(matches!(error, DnsError::SystemdTooOld { version } if version == old));
            assert!(error.to_string().contains("258"), "{error}");
        }
        let v6 = systemd_dns_delegate(&zone, "[::1]:5300".parse().expect("addr"));
        assert!(v6.contents.contains("DNS=[::1]:5300\n"));
    }

    #[test]
    fn managed_marker_is_the_first_line() {
        assert!(!is_managed("nameserver 127.0.0.1\nport 20560\n"));
        assert!(!is_managed(
            "# Generated by puma-dev\n# Managed by devlanes\n"
        ));
        assert!(!is_managed(""));
    }

    #[test]
    fn generated_paths_only() {
        let macos = generated_file(Path::new("/etc/resolver/test")).expect("macos");
        assert_eq!(macos.activate, None);
        let delegate = generated_file(Path::new(
            "/etc/systemd/dns-delegate.d/devlanes-dev.internal.dns-delegate",
        ))
        .expect("delegate");
        assert!(delegate.activate.is_some());
        for other in [
            "/etc/passwd",
            "/etc/resolver/example",
            "/etc/resolver/TEST",
            "/etc/resolver/../passwd",
            "/etc/systemd/dns-delegate.d/other-test.dns-delegate",
            "/etc/systemd/resolved.conf.d/devlanes-test.conf",
            "etc/resolver/test",
        ] {
            assert!(generated_file(Path::new(other)).is_none(), "{other}");
        }
    }
}

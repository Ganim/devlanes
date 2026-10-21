//! OS resolver configuration that forwards the TLD to the embedded server.
//!
//! Only content and target paths are produced. Writing them needs root, which
//! the CLI handles; the daemon never touches these files.

use std::net::SocketAddr;
use std::path::PathBuf;

use crate::{DnsError, Zone};

/// systemd release that added `*.dns-delegate` files, which route a domain to a
/// server without making it a fallback for every other lookup.
pub const DNS_DELEGATE_MIN_SYSTEMD: u32 = 258;

/// systemd release whose `DNS=` accepts `address:port`.
pub const PORT_SYNTAX_MIN_SYSTEMD: u32 = 246;

/// A root-owned file to write, plus what makes the OS pick it up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolverFile {
    /// Absolute target path.
    pub path: PathBuf,
    /// Full file contents.
    pub contents: String,
    /// Command to run as root after writing or removing the file; `None` when the
    /// OS watches the directory itself.
    pub activate: Option<Vec<String>>,
}

/// `/etc/resolver/<tld>` for macOS.
///
/// mDNSResponder watches `/etc/resolver`, so no reload is needed. `dig` and
/// `nslookup` bypass this file; use `dscacheutil -q host -a name <host>` to check.
pub fn macos_resolver(zone: &Zone, server: SocketAddr) -> ResolverFile {
    ResolverFile {
        path: PathBuf::from("/etc/resolver").join(zone.name()),
        contents: format!(
            "# Managed by devlanes: sends *.{tld} lookups to the devlanes DNS server.\n\
             nameserver {ip}\n\
             port {port}\n",
            tld = zone.name(),
            ip = server.ip(),
            port = server.port(),
        ),
        activate: None,
    }
}

/// A systemd-resolved DNS delegate (systemd 258+), the preferred Linux form.
pub fn systemd_dns_delegate(zone: &Zone, server: SocketAddr) -> ResolverFile {
    ResolverFile {
        path: PathBuf::from("/etc/systemd/dns-delegate.d")
            .join(format!("devlanes-{}.dns-delegate", zone.name())),
        contents: format!(
            "# Managed by devlanes: sends *.{tld} lookups to the devlanes DNS server.\n\
             [Delegate]\n\
             DNS={server}\n\
             Domains=~{tld}\n",
            tld = zone.name(),
        ),
        activate: Some(restart_resolved()),
    }
}

/// A `resolved.conf.d` drop-in for systemd 246 to 257.
///
/// These versions have no delegates, so the server joins the global DNS scope. The
/// `~tld` routing domain sends the TLD only here, but resolved also queries the
/// global scope in parallel with link servers for names no domain matches. The
/// server refuses those quickly, so lookups still succeed through the link servers.
pub fn systemd_resolved_dropin(zone: &Zone, server: SocketAddr) -> ResolverFile {
    ResolverFile {
        path: PathBuf::from("/etc/systemd/resolved.conf.d")
            .join(format!("devlanes-{}.conf", zone.name())),
        contents: format!(
            "# Managed by devlanes: sends *.{tld} lookups to the devlanes DNS server.\n\
             [Resolve]\n\
             DNS={server}\n\
             Domains=~{tld}\n",
            tld = zone.name(),
        ),
        activate: Some(restart_resolved()),
    }
}

/// Picks the systemd-resolved form for `systemd_version`.
pub fn systemd_resolver(
    zone: &Zone,
    server: SocketAddr,
    systemd_version: u32,
) -> Result<ResolverFile, DnsError> {
    if systemd_version >= DNS_DELEGATE_MIN_SYSTEMD {
        Ok(systemd_dns_delegate(zone, server))
    } else if systemd_version >= PORT_SYNTAX_MIN_SYSTEMD {
        Ok(systemd_resolved_dropin(zone, server))
    } else {
        Err(DnsError::SystemdTooOld {
            version: systemd_version,
        })
    }
}

/// Restart re-reads delegates and drop-ins on every supported version; `reload`
/// only covers newer releases.
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
    }

    #[test]
    fn systemd_files_by_version() {
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

        let dropin = systemd_resolver(&zone, server, 255).expect("drop-in");
        assert_eq!(
            dropin.path,
            PathBuf::from("/etc/systemd/resolved.conf.d/devlanes-dev.internal.conf")
        );
        assert!(
            dropin
                .contents
                .ends_with("[Resolve]\nDNS=127.0.0.1:15353\nDomains=~dev.internal\n")
        );

        assert!(matches!(
            systemd_resolver(&zone, server, 245),
            Err(DnsError::SystemdTooOld { version: 245 })
        ));
        let v6 = systemd_dns_delegate(&zone, "[::1]:5300".parse().expect("addr"));
        assert!(v6.contents.contains("DNS=[::1]:5300\n"));
    }
}

//! Embedded DNS server for the `resolver` hostname strategy.
//!
//! The daemon answers A and AAAA queries for names under the configured TLD with
//! `127.0.0.1` and `::1` and refuses everything else. The OS forwards only that TLD
//! here through `/etc/resolver/<tld>` (macOS), systemd-resolved (Linux) or an NRPT
//! rule (Windows, the `nrpt` module); this crate generates those files and PowerShell
//! scripts, and the CLI applies them with sudo or through the UAC prompt.
//!
//! The responder is hand-written instead of using `hickory-server`. It needs one
//! synthesized zone with two fixed answers, which takes a few hundred lines of
//! wire handling, while hickory-server brings its protocol, authority and
//! server stacks for features (zone files, DNSSEC, forwarding) never used here.
//! The parser accepts standard queries with EDNS(0); anything else gets FORMERR,
//! NOTIMP or REFUSED, so clients fail fast instead of timing out.

mod files;
#[cfg(windows)]
pub mod nrpt;
mod server;
mod wire;
mod zone;

pub use files::{
    DNS_DELEGATE_MIN_SYSTEMD, MANAGED_MARKER, ResolverFile, generated_file, is_managed,
    macos_resolver, systemd_dns_delegate, systemd_resolver,
};
pub use server::{DEFAULT_PORT, DnsServer, default_listen};
pub use wire::answer;
pub use zone::{RESERVED_ZONES, Zone};

use std::net::SocketAddr;

/// Failure to configure or start the DNS server.
#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    /// The TLD is not a usable DNS name.
    #[error("invalid resolver TLD `{tld}`: {reason}")]
    InvalidTld {
        /// The rejected value.
        tld: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// UDP or TCP could not bind. On Windows the message adds where to look
    /// when another program holds port 53.
    #[error("cannot bind the DNS server on {addr}: {source}{}", server::bind_hint(.addr, .source))]
    Bind {
        /// Requested address.
        addr: SocketAddr,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// systemd-resolved has no DNS delegates, the only safe way to route one domain.
    #[error(
        "the resolver strategy needs systemd {DNS_DELEGATE_MIN_SYSTEMD} or newer (found {version}): \
         older systemd-resolved can only add the devlanes DNS server as a global server, which \
         would answer every lookup on this machine; use hostnames.resolver: localhost instead"
    )]
    SystemdTooOld {
        /// Detected systemd version.
        version: u32,
    },
    /// PowerShell printed NRPT rules in a shape [`nrpt::NrptRules::parse`] cannot read.
    #[cfg(windows)]
    #[error("cannot read the NRPT rules from PowerShell: {reason}")]
    NrptOutput {
        /// What was wrong with the output.
        reason: String,
    },
}

use crate::DnsError;

/// Default answer TTL. Answers never change, but a short TTL bounds how long a
/// stale entry survives after the user switches resolver strategy.
const DEFAULT_TTL: u32 = 60;

/// Special-use zones a TLD may equal or sit under, matching the hosts helper's list
/// (`devlanes_hosts_helper::RESERVED_SUFFIXES`) minus `localhost`, which only allows
/// subdomains. `test` is reserved by RFC 6761, `home.arpa` by RFC 8375 and
/// `internal` by ICANN for private use, so none can name a public site.
pub const RESERVED_ZONES: &[&str] = &["test", "internal", "home.arpa"];

/// The TLD the server is authoritative for, with the answer TTL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Zone {
    name: String,
    labels: Vec<Vec<u8>>,
    ttl: u32,
}

impl Zone {
    /// Validates `tld` (for example `test` or `dev.internal`).
    ///
    /// One trailing dot is accepted and case is folded. The zone must be `test`,
    /// `internal` or `home.arpa`, a domain under one of them, or a domain under
    /// `localhost`: the same special-use names the hosts helper accepts. Any other
    /// name could belong to a real site, and the OS resolver file would send every
    /// user's lookups for it to loopback.
    pub fn new(tld: &str) -> Result<Self, DnsError> {
        let invalid = |reason| DnsError::InvalidTld {
            tld: tld.to_owned(),
            reason,
        };
        let name = tld.strip_suffix('.').unwrap_or(tld).to_ascii_lowercase();
        if name.is_empty() {
            return Err(invalid("it is empty"));
        }
        if name.len() > 253 {
            return Err(invalid("it is longer than 253 characters"));
        }
        let mut labels = Vec::new();
        for label in name.split('.') {
            if label.is_empty() || label.len() > 63 {
                return Err(invalid("every label must be 1 to 63 characters"));
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                || label.starts_with('-')
                || label.ends_with('-')
            {
                return Err(invalid(
                    "labels may only hold letters, digits and inner hyphens",
                ));
            }
            labels.push(label.as_bytes().to_vec());
        }
        if labels
            .last()
            .is_some_and(|last| last.iter().all(u8::is_ascii_digit))
        {
            return Err(invalid("the last label cannot be numeric"));
        }
        let under = |suffix: &str| {
            name.strip_suffix(suffix)
                .is_some_and(|rest| rest.ends_with('.'))
        };
        let reserved = RESERVED_ZONES
            .iter()
            .any(|suffix| name == *suffix || under(suffix))
            // `localhost` itself already resolves to loopback without any resolver file.
            || under("localhost");
        if !reserved {
            return Err(invalid(
                "the TLD must be test, internal or home.arpa, a domain under one of them, \
                 or a domain under localhost",
            ));
        }
        Ok(Self {
            name,
            labels,
            ttl: DEFAULT_TTL,
        })
    }

    /// Replaces the answer TTL.
    #[must_use]
    pub fn with_ttl(mut self, ttl: u32) -> Self {
        self.ttl = ttl;
        self
    }

    /// Lowercase name without a trailing dot.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Answer TTL in seconds.
    pub fn ttl(&self) -> u32 {
        self.ttl
    }

    /// Whether a presentation-format name is the zone apex or below it.
    pub fn contains(&self, name: &str) -> bool {
        let name = name.strip_suffix('.').unwrap_or(name);
        let labels: Vec<Vec<u8>> = name
            .split('.')
            .map(|label| label.as_bytes().to_ascii_lowercase())
            .collect();
        self.contains_labels(&labels)
    }

    /// Label-wise suffix match; comparing labels rather than dotted strings keeps a
    /// wire label that contains a literal `.` from matching.
    pub(crate) fn contains_labels(&self, labels: &[Vec<u8>]) -> bool {
        labels.len() >= self.labels.len()
            && labels[labels.len() - self.labels.len()..]
                .iter()
                .zip(&self.labels)
                .all(|(label, zone)| label.eq_ignore_ascii_case(zone))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_tlds() {
        assert_eq!(Zone::new("Test.").expect("valid").name(), "test");
        assert_eq!(
            Zone::new("dev.internal").expect("valid").name(),
            "dev.internal"
        );
        for good in [
            "internal",
            "home.arpa",
            "lab.home.arpa",
            "a.b.test",
            "dev.localhost",
        ] {
            assert_eq!(Zone::new(good).expect("reserved").name(), good);
        }
        for public in [
            "example",
            "com",
            "dev",
            "lan",
            "contest",
            "test.com",
            "xhome.arpa",
        ] {
            assert!(
                matches!(Zone::new(public), Err(DnsError::InvalidTld { reason, .. }) if reason.contains("home.arpa")),
                "{public:?} is not a special-use name"
            );
        }
        for bad in [
            "",
            ".",
            "a..b",
            "-x",
            "x-",
            "x_y",
            "café",
            "123",
            "local",
            "localhost",
        ] {
            assert!(Zone::new(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(Zone::new(&"a".repeat(64)).is_err());
    }

    #[test]
    fn matches_apex_and_subdomains_only() {
        let zone = Zone::new("test").expect("zone");
        assert!(zone.contains("test"));
        assert!(zone.contains("api.acme-2.TEST."));
        assert!(!zone.contains("contest"));
        assert!(!zone.contains("test.com"));
        let deep = Zone::new("dev.internal").expect("zone");
        assert!(deep.contains("a.dev.internal"));
        assert!(!deep.contains("internal"));
        assert!(!deep.contains("a.xdev.internal"));
    }
}

use crate::DnsError;

/// Default answer TTL. Answers never change, but a short TTL bounds how long a
/// stale entry survives after the user switches resolver strategy.
const DEFAULT_TTL: u32 = 60;

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
    /// One trailing dot is accepted and case is folded. `local` and `localhost` are
    /// refused because routing them here would break mDNS or loopback resolution.
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
        if name == "local" || name == "localhost" {
            return Err(invalid("it is reserved for mDNS or loopback"));
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
        assert!(zone.contains("crm.clickmax-2.TEST."));
        assert!(!zone.contains("contest"));
        assert!(!zone.contains("test.com"));
        let deep = Zone::new("dev.internal").expect("zone");
        assert!(deep.contains("a.dev.internal"));
        assert!(!deep.contains("internal"));
        assert!(!deep.contains("a.xdev.internal"));
    }
}

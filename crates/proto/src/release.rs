//! Release versions: semver ordering and what the daemon's release check reports.
//! The daemon only reads the latest release's version; it never downloads or installs.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

/// A newer release, in `daemon.status` and `status` results once the daemon has
/// seen it for a while (Homebrew and npm publish a few minutes after GitHub).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateInfo {
    pub latest: String,
    /// When the daemon last read the release, in milliseconds since the epoch.
    pub checked_at: u64,
}

/// The release check's state, in `daemon.status` for `doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseCheck {
    /// `off` (see `reason`), `pending` before the first answer, or `ok`.
    pub state: String,
    /// Why checks are off: `debug_build`, `ci`, `env` (DEVLANES_NO_UPDATE_CHECK) or `config`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The latest release version the daemon read, newer or not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// When it was read, in milliseconds since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<u64>,
}

/// A parsed `MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD]`, with an optional leading `v`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    core: (u64, u64, u64),
    pre: Vec<String>,
}

impl Version {
    /// `None` for anything that is not a release version.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.strip_prefix('v').unwrap_or(text);
        let text = text
            .split_once('+')
            .map_or(text, |(version, _build)| version);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (text, None),
        };
        let mut parts = core.split('.').map(number);
        let core = (parts.next()??, parts.next()??, parts.next()??);
        if parts.next().is_some() {
            return None;
        }
        let pre = match pre {
            Some(pre) => {
                let ids: Vec<String> = pre.split('.').map(str::to_owned).collect();
                let valid = ids.iter().all(|id| {
                    !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                });
                if !valid {
                    return None;
                }
                ids
            }
            None => Vec::new(),
        };
        Some(Self { core, pre })
    }
}

/// A numeric version component without leading zeros.
fn number(text: &str) -> Option<u64> {
    if text.is_empty() || (text.len() > 1 && text.starts_with('0')) {
        return None;
    }
    if !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

impl Ord for Version {
    /// Semver precedence: a prerelease sorts before its release, numeric
    /// identifiers numerically and before alphanumeric ones.
    fn cmp(&self, other: &Self) -> Ordering {
        self.core
            .cmp(&other.core)
            .then_with(|| match (self.pre.is_empty(), other.pre.is_empty()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Greater,
                (false, true) => Ordering::Less,
                (false, false) => {
                    for (left, right) in self.pre.iter().zip(&other.pre) {
                        let order = match (left.parse::<u64>(), right.parse::<u64>()) {
                            (Ok(left), Ok(right)) => left.cmp(&right),
                            (Ok(_), Err(_)) => Ordering::Less,
                            (Err(_), Ok(_)) => Ordering::Greater,
                            (Err(_), Err(_)) => left.cmp(right),
                        };
                        if order != Ordering::Equal {
                            return order;
                        }
                    }
                    self.pre.len().cmp(&other.pre.len())
                }
            })
    }
}

impl PartialOrd for Version {
    /// Total order; see [`Ord`].
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Whether `candidate` is a strictly newer release than `current`; false when
/// either does not parse.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (Version::parse(candidate), Version::parse(current)) {
        (Some(candidate), Some(current)) => candidate > current,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_precedence() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(is_newer("v0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("1.0.0", "1.0.0-rc.1"));
        assert!(!is_newer("1.0.0-rc.1", "1.0.0"));
        assert!(is_newer("1.0.0-rc.2", "1.0.0-rc.1"));
        assert!(is_newer("1.0.0-rc.10", "1.0.0-rc.9"));
        assert!(is_newer("1.0.0-beta", "1.0.0-alpha.1"));
        assert!(is_newer("1.0.0-alpha.beta", "1.0.0-alpha.1"));
        assert!(is_newer("1.0.0-alpha.1", "1.0.0-alpha"));
        assert!(!is_newer("0.2.0+build.5", "0.2.0"));
    }

    #[test]
    fn rejects_non_versions() {
        for text in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "01.2.3",
            "1.2.x",
            "1.2.3-",
            "1.2.3-a..b",
            "latest",
        ] {
            assert_eq!(Version::parse(text), None, "{text}");
        }
        assert!(!is_newer("latest", "0.1.0"));
        assert!(!is_newer("0.2.0", "local"));
    }
}

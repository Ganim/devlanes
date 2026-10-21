//! When devlanes writes names to `/etc/hosts`, and which names.

use std::collections::BTreeSet;
use std::fmt;
use std::net::{SocketAddr, ToSocketAddrs as _};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use devlanes_hosts_helper::names::{self, MAX_NAMES, Suffixes};

/// The `hosts_file` setting, from the user config or a project's `lanes.hosts_file`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HostsFileMode {
    /// Write names only when the system resolver does not resolve `*.localhost`.
    #[default]
    Auto,
    /// Never write names.
    Never,
    /// Always write names.
    Always,
}

impl FromStr for HostsFileMode {
    type Err = String;

    /// Parses the config spelling: `auto`, `never` or `always`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "never" => Ok(Self::Never),
            "always" => Ok(Self::Always),
            other => Err(format!("unknown hosts_file mode `{other}`")),
        }
    }
}

impl fmt::Display for HostsFileMode {
    /// The config spelling.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Never => "never",
            Self::Always => "always",
        })
    }
}

/// What the system resolver returned for a random `*.localhost` name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalhostResolution {
    /// Only loopback addresses: `*.localhost` works without the hosts file.
    Loopback,
    /// Some non-loopback address, typically an upstream DNS server answering for
    /// the name; treated as not working.
    NotLoopback,
    /// No answer, an error, or no answer within the timeout.
    Fails,
}

impl LocalhostResolution {
    /// Whether the system resolver handles `*.localhost` by itself.
    pub fn works(self) -> bool {
        self == Self::Loopback
    }
}

/// Decides whether a project's names go into `/etc/hosts`.
///
/// Precedence: the user's `never` turns the feature off for every project, because
/// a repository must not be able to override the machine owner. Otherwise the
/// project's hint, when set, replaces the user's mode. `auto` writes names only when
/// the probe shows the system resolver cannot resolve `*.localhost`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HostsFilePolicy {
    user: HostsFileMode,
}

impl HostsFilePolicy {
    /// Policy for the user's `hostnames.hosts_file`, defaulting to `auto`.
    ///
    /// Map `hostnames.resolver: hosts_file` to `Some(Always)` before calling this.
    pub fn new(user: Option<HostsFileMode>) -> Self {
        Self {
            user: user.unwrap_or_default(),
        }
    }

    /// The user's mode.
    pub fn user_mode(self) -> HostsFileMode {
        self.user
    }

    /// The mode in effect for a project with `hint`.
    pub fn effective(self, hint: Option<HostsFileMode>) -> HostsFileMode {
        if self.user == HostsFileMode::Never {
            HostsFileMode::Never
        } else {
            hint.unwrap_or(self.user)
        }
    }

    /// Whether any of `hints` needs the resolver probe; skip probing otherwise.
    pub fn needs_probe(self, hints: impl IntoIterator<Item = Option<HostsFileMode>>) -> bool {
        hints
            .into_iter()
            .any(|hint| self.effective(hint) == HostsFileMode::Auto)
    }

    /// Whether a project with `hint` gets its names written, given the probe result.
    pub fn applies(self, hint: Option<HostsFileMode>, resolution: LocalhostResolution) -> bool {
        match self.effective(hint) {
            HostsFileMode::Never => false,
            HostsFileMode::Always => true,
            HostsFileMode::Auto => !resolution.works(),
        }
    }

    /// The names to write across `projects`, filtered for the helper.
    pub fn desired<'a, I, N>(
        self,
        resolution: LocalhostResolution,
        suffixes: &Suffixes,
        projects: I,
    ) -> DesiredHosts
    where
        I: IntoIterator<Item = (Option<HostsFileMode>, N)>,
        N: IntoIterator<Item = &'a str>,
    {
        let mut selected = Vec::new();
        for (hint, names) in projects {
            if self.applies(hint, resolution) {
                selected.extend(names);
            }
        }
        filter_names(selected, suffixes)
    }
}

/// Why a routed name was left out of the hosts file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A wildcard route; `/etc/hosts` only maps exact names.
    Wildcard,
    /// Not under `.localhost` or the configured TLD, or not a valid hostname.
    NotAllowed,
    /// Beyond the helper's name cap.
    OverCap,
}

/// A name left out, with the reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    /// The name as routed.
    pub name: String,
    /// Why.
    pub reason: SkipReason,
}

/// Names to send to the helper and the ones left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DesiredHosts {
    /// Lowercase names the helper accepts, at most [`MAX_NAMES`].
    pub names: BTreeSet<String>,
    /// Names left out.
    pub skipped: Vec<Skipped>,
}

/// Normalises routed names (lowercase, no trailing dot) and keeps the ones the helper
/// accepts, so one odd route cannot make the helper reject the whole batch.
pub fn filter_names<'a>(
    names: impl IntoIterator<Item = &'a str>,
    suffixes: &Suffixes,
) -> DesiredHosts {
    let mut out = DesiredHosts::default();
    let mut seen = BTreeSet::new();
    for raw in names {
        let name = raw.strip_suffix('.').unwrap_or(raw).to_ascii_lowercase();
        if !seen.insert(name.clone()) {
            continue;
        }
        let reason = if name.contains('*') {
            Some(SkipReason::Wildcard)
        } else if names::validate_name(&name, suffixes).is_err() {
            Some(SkipReason::NotAllowed)
        } else if out.names.len() >= MAX_NAMES {
            Some(SkipReason::OverCap)
        } else {
            None
        };
        match reason {
            Some(reason) => out.skipped.push(Skipped {
                name: raw.to_owned(),
                reason,
            }),
            None => {
                out.names.insert(name);
            }
        }
    }
    out
}

/// A fresh probe name; random so no cache or hosts entry can answer it.
pub fn probe_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.subsec_nanos());
    format!("devlanes-probe-{}-{nanos}.localhost", std::process::id())
}

/// Classifies a `getaddrinfo` result.
pub fn classify(result: std::io::Result<Vec<SocketAddr>>) -> LocalhostResolution {
    match result {
        Ok(addrs) if addrs.is_empty() => LocalhostResolution::Fails,
        Ok(addrs) if addrs.iter().all(|addr| addr.ip().is_loopback()) => {
            LocalhostResolution::Loopback
        }
        Ok(_) => LocalhostResolution::NotLoopback,
        Err(_) => LocalhostResolution::Fails,
    }
}

/// Resolves a random `devlanes-probe-*.localhost` through the system resolver.
///
/// `getaddrinfo` blocks and can wait on upstream DNS when the OS does not handle
/// `.localhost` itself, so it runs on the blocking pool under `timeout`. This only
/// tests the OS resolver; runtimes with their own resolver (pure-Go, musl, workerd)
/// need the project hint.
pub async fn probe_localhost(timeout: Duration) -> LocalhostResolution {
    let name = probe_name();
    let lookup = tokio::task::spawn_blocking(move || {
        (name.as_str(), 0)
            .to_socket_addrs()
            .map(Iterator::collect::<Vec<_>>)
    });
    match tokio::time::timeout(timeout, lookup).await {
        Ok(Ok(result)) => classify(result),
        Ok(Err(_)) | Err(_) => LocalhostResolution::Fails,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HostsFileMode::{Always, Auto, Never};
    use LocalhostResolution::{Fails, Loopback, NotLoopback};

    #[test]
    fn decision_table() {
        let cases = [
            // (user, project hint, resolution, expected)
            (None, None, Loopback, false),
            (None, None, Fails, true),
            (None, None, NotLoopback, true),
            (Some(Auto), None, Loopback, false),
            (Some(Auto), Some(Always), Loopback, true),
            (Some(Auto), Some(Never), Fails, false),
            (Some(Always), None, Loopback, true),
            (Some(Always), Some(Never), Fails, false),
            (Some(Always), Some(Auto), Loopback, false),
            (Some(Always), Some(Auto), Fails, true),
            (Some(Never), None, Fails, false),
            (Some(Never), Some(Always), Fails, false),
            (Some(Never), Some(Always), Loopback, false),
        ];
        for (user, hint, resolution, expected) in cases {
            assert_eq!(
                HostsFilePolicy::new(user).applies(hint, resolution),
                expected,
                "user={user:?} hint={hint:?} resolution={resolution:?}"
            );
        }
    }

    #[test]
    fn probe_is_only_needed_for_auto() {
        let auto = HostsFilePolicy::new(None);
        assert!(auto.needs_probe([None]));
        assert!(!auto.needs_probe([Some(Always), Some(Never)]));
        assert!(!HostsFilePolicy::new(Some(Never)).needs_probe([None, Some(Auto)]));
        assert!(!HostsFilePolicy::new(Some(Always)).needs_probe([None]));
        assert!(HostsFilePolicy::new(Some(Always)).needs_probe([Some(Auto)]));
    }

    #[test]
    fn modes_parse_and_display() {
        for mode in [Auto, Never, Always] {
            assert_eq!(mode.to_string().parse::<HostsFileMode>(), Ok(mode));
        }
        assert!("sometimes".parse::<HostsFileMode>().is_err());
    }

    #[test]
    fn desired_selects_projects_and_filters_names() {
        let suffixes = Suffixes::new(Some("test")).expect("suffixes");
        let policy = HostsFilePolicy::new(Some(Auto));
        let desired = policy.desired(
            Loopback,
            &suffixes,
            [
                (None, vec!["web.acme.localhost"]),
                (
                    Some(Always),
                    vec![
                        "Web.Workerd.localhost.",
                        "*.pages-2.workerd.localhost",
                        "api.workerd.test",
                        "web.workerd.localhost",
                        "example.com",
                    ],
                ),
            ],
        );
        assert_eq!(
            desired.names.into_iter().collect::<Vec<_>>(),
            ["api.workerd.test", "web.workerd.localhost"]
        );
        assert_eq!(
            desired.skipped,
            [
                Skipped {
                    name: "*.pages-2.workerd.localhost".into(),
                    reason: SkipReason::Wildcard
                },
                Skipped {
                    name: "example.com".into(),
                    reason: SkipReason::NotAllowed
                },
            ]
        );
    }

    #[test]
    fn filter_caps_the_count() {
        let suffixes = Suffixes::new(None).expect("suffixes");
        let names: Vec<String> = (0..=MAX_NAMES).map(|i| format!("n{i}.localhost")).collect();
        let desired = filter_names(names.iter().map(String::as_str), &suffixes);
        assert_eq!(desired.names.len(), MAX_NAMES);
        assert_eq!(desired.skipped.len(), 1);
        assert_eq!(desired.skipped[0].reason, SkipReason::OverCap);
    }

    #[test]
    fn classifies_lookups() {
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("addr");
        let v6: SocketAddr = "[::1]:0".parse().expect("addr");
        let public: SocketAddr = "203.0.113.7:0".parse().expect("addr");
        assert_eq!(classify(Ok(vec![v6, loopback])), Loopback);
        assert_eq!(classify(Ok(vec![loopback, public])), NotLoopback);
        assert_eq!(classify(Ok(vec![])), Fails);
        assert_eq!(classify(Err(std::io::Error::other("nxdomain"))), Fails);
    }

    #[test]
    fn probe_names_are_valid() {
        let suffixes = Suffixes::new(None).expect("suffixes");
        let name = probe_name();
        assert!(names::validate_name(&name, &suffixes).is_ok(), "{name}");
    }

    #[tokio::test]
    async fn probe_returns_within_the_timeout() {
        // The answer depends on the machine; only the bound is asserted.
        let started = std::time::Instant::now();
        let _ = probe_localhost(Duration::from_secs(3)).await;
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}

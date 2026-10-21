use std::{collections::BTreeMap, net::SocketAddr};

/// A dial target; local ports try IPv4 before IPv6 without replaying requests.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Upstream {
    Local(u16),
    Address(SocketAddr),
    Host { hostname: String, port: u16 },
}

impl From<SocketAddr> for Upstream {
    /// Explicit addresses retain their address family.
    fn from(value: SocketAddr) -> Self {
        Self::Address(value)
    }
}

impl Upstream {
    /// Supplies an authority for Host rewriting without changing the dial target.
    pub fn authority(&self) -> String {
        match self {
            Self::Local(port) => format!("127.0.0.1:{port}"),
            Self::Address(addr) => addr.to_string(),
            Self::Host { hostname, port } => format!("{hostname}:{port}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum HostHeader {
    #[default]
    Preserve,
    Upstream,
}

/// The daemon supplies status and a bounded, already-redacted log tail.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RouteState {
    #[default]
    Ready,
    Idle,
    Starting,
    Failed {
        log_tail: String,
    },
    /// The route belongs to a remote box whose connection is down and retrying.
    Reconnecting,
}

/// A hostname mapping independent of daemon/configuration types.
#[derive(Clone, Debug)]
pub struct Route {
    pub host: String,
    pub upstream: Upstream,
    pub fallback: bool,
    pub host_header: HostHeader,
    pub state: RouteState,
    pub start_hint: String,
    pub named_ports: BTreeMap<String, Upstream>,
    pub port: Option<String>,
    /// Extra SANs, usually the lane apex. They do not authorize new routes.
    pub certificate_names: Vec<String>,
}

impl Route {
    /// Creates a ready, Host-preserving route; table construction validates it.
    pub fn new(host: impl Into<String>, upstream: impl Into<Upstream>) -> Self {
        Self {
            host: host.into(),
            upstream: upstream.into(),
            fallback: false,
            host_header: HostHeader::Preserve,
            state: RouteState::Ready,
            start_hint: "Start this service with devlanes start <service>.".into(),
            named_ports: BTreeMap::new(),
            port: None,
            certificate_names: Vec::new(),
        }
    }

    /// Returns the selected named endpoint after `RouteTable::new` validates it.
    pub fn target(&self) -> &Upstream {
        self.port
            .as_ref()
            .and_then(|name| self.named_ports.get(name))
            .unwrap_or(&self.upstream)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RouteError {
    #[error("invalid DNS route name: {0}")]
    InvalidHost(String),
    #[error("duplicate route: {0}")]
    Duplicate(String),
    #[error("route {host} refers to missing named port {port}")]
    MissingPort { host: String, port: String },
    #[error("invalid upstream authority: {0}")]
    InvalidUpstream(String),
}

/// Immutable, validated table. Fallback routes, exact ones included, apply only when no
/// other route matches; within each group an exact route beats the longest wildcard.
#[derive(Clone, Debug, Default)]
pub struct RouteTable {
    routes: Vec<Route>,
}

/// A route [`RouteTable::accept`] left out of its table, and why.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejected {
    /// Position of the route in the snapshot passed to `accept`.
    pub index: usize,
    /// The route's host as given, before normalisation.
    pub host: String,
    pub error: RouteError,
}

impl RouteTable {
    /// Validates a complete snapshot before it can replace live routes.
    pub fn new(mut routes: Vec<Route>) -> Result<Self, RouteError> {
        let mut seen = std::collections::HashSet::new();
        for route in &mut routes {
            normalize(route)?;
            if !seen.insert((route.host.clone(), route.fallback)) {
                return Err(RouteError::Duplicate(route.host.clone()));
            }
        }
        Ok(Self::sorted(routes))
    }

    /// Builds a table from the valid routes of a snapshot and lists the others:
    /// invalid ones, and every later route whose normalised host (and fallback
    /// flag) an earlier route already claimed. One bad entry never blocks the rest.
    pub fn accept(routes: Vec<Route>) -> (Self, Vec<Rejected>) {
        let mut seen = std::collections::HashSet::new();
        let mut kept = Vec::with_capacity(routes.len());
        let mut rejected = Vec::new();
        for (index, mut route) in routes.into_iter().enumerate() {
            let host = route.host.clone();
            let checked = normalize(&mut route).and_then(|()| {
                if seen.insert((route.host.clone(), route.fallback)) {
                    Ok(())
                } else {
                    Err(RouteError::Duplicate(route.host.clone()))
                }
            });
            match checked {
                Ok(()) => kept.push(route),
                Err(error) => rejected.push(Rejected { index, host, error }),
            }
        }
        (Self::sorted(kept), rejected)
    }

    /// Orders validated routes for lookup.
    fn sorted(mut routes: Vec<Route>) -> Self {
        // Matches the config contract that a fallback host is used only when nothing else matches.
        routes.sort_by_key(|route| {
            (
                route.fallback,
                route.host.starts_with("*."),
                std::cmp::Reverse(route.host.len()),
            )
        });
        Self { routes }
    }

    /// Looks up a normalized hostname, never a URL or a host:port authority.
    pub fn lookup(&self, host: &str) -> Option<&Route> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        if host.starts_with("*.") || !valid_pattern(&host) {
            return None;
        }
        self.routes
            .iter()
            .find(|route| matches_host(&route.host, &host))
    }

    /// Exposes the snapshot for hosts listings and certificate policies.
    pub fn routes(&self) -> &[Route] {
        &self.routes
    }
}

/// Lowercases and trims one route's names in place and checks everything except
/// duplicates, which need the whole snapshot.
fn normalize(route: &mut Route) -> Result<(), RouteError> {
    route.host = route.host.trim_end_matches('.').to_ascii_lowercase();
    if !valid_pattern(&route.host) {
        return Err(RouteError::InvalidHost(route.host.clone()));
    }
    if let Some(port) = &route.port
        && !route.named_ports.contains_key(port)
    {
        return Err(RouteError::MissingPort {
            host: route.host.clone(),
            port: port.clone(),
        });
    }
    let authority = route.target().authority();
    let invalid_hostname = matches!(route.target(), Upstream::Host { hostname, .. }
        if hostname.starts_with("*.") || !valid_pattern(hostname.trim_end_matches('.')));
    if invalid_hostname || authority.parse::<hyper::http::uri::Authority>().is_err() {
        return Err(RouteError::InvalidUpstream(authority));
    }
    for name in &mut route.certificate_names {
        *name = name.trim_end_matches('.').to_ascii_lowercase();
        if !valid_pattern(name)
            || name
                .strip_prefix("*.")
                .is_some_and(|suffix| !suffix.contains('.'))
        {
            return Err(RouteError::InvalidHost(name.clone()));
        }
    }
    Ok(())
}

/// Wildcards route subdomains at any depth; TLS adds an exact SAN for deeper names.
pub(crate) fn matches_host(pattern: &str, host: &str) -> bool {
    pattern
        .strip_prefix("*.")
        .map_or(pattern == host, |suffix| {
            host.strip_suffix(suffix)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1)
        })
}

/// DNS syntax is deliberately narrower than arbitrary HTTP authorities.
fn valid_pattern(pattern: &str) -> bool {
    let host = pattern.strip_prefix("*.").unwrap_or(pattern);
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

//! Routed hostnames the active CA cannot certify. The devlanes CA is name
//! constrained (localhost, test, internal, home.arpa and the configured TLD), so
//! clients reject its certificates for other names. Such routes stay: plain
//! HTTP keeps working. `config check`, `doctor` and the start result warn and
//! suggest a name under the lane's domain. mkcert roots have no constraints.

use devlanes_proto::LaneWarning;
use serde_json::json;

use crate::{engine::Inner, model::LaneSt};

/// True when `host` (a `*.` wildcard included) is one of `permitted` or below it.
pub(crate) fn permitted(host: &str, permitted: &[String]) -> bool {
    let host = host
        .trim_start_matches("*.")
        .trim_end_matches('.')
        .to_ascii_lowercase();
    permitted.iter().any(|name| {
        host == *name
            || host
                .strip_suffix(name.as_str())
                .is_some_and(|rest| rest.ends_with('.'))
    })
}

/// A name for `host` under `domain`, keeping its first label (and a wildcard).
fn suggestion(host: &str, domain: &str) -> String {
    let (wildcard, rest) = match host.strip_prefix("*.") {
        Some(rest) => ("*.", rest),
        None => ("", host),
    };
    let label = rest.split('.').next().unwrap_or(rest);
    if wildcard.is_empty() {
        format!("{label}.{domain}")
    } else {
        format!("*.{domain}")
    }
}

impl Inner {
    /// `host_outside_ca` warnings for the lane's routes (services and infra) the
    /// CA's name constraints exclude; none without constraints.
    pub(crate) fn ca_warnings(&self, reference: &str, lane: &LaneSt) -> Vec<LaneWarning> {
        let (Some(allowed), Some(view)) = (&self.integ.ca_permitted, &lane.view) else {
            return Vec::new();
        };
        let domain = if permitted(&view.domain, allowed) {
            view.domain.clone()
        } else {
            // The lane's own domain is outside too (an explicit `lanes.vars.domain`).
            let tld = self.resolver_tld().unwrap_or_else(|| "localhost".into());
            let project = reference.split('@').next().unwrap_or(reference);
            format!("{project}.{tld}")
        };
        let hosts = view
            .routes
            .iter()
            .map(|route| (route.host.as_str(), route.unit.as_str()))
            .chain(
                view.infra_routes
                    .iter()
                    .map(|(host, container, _)| (host.as_str(), container.as_str())),
            );
        let mut seen = std::collections::BTreeSet::new();
        hosts
            .filter(|(host, _)| !permitted(host, allowed) && seen.insert(host.to_ascii_lowercase()))
            .map(|(host, unit)| LaneWarning {
                code: "host_outside_ca".into(),
                data: json!({
                    "lane": reference,
                    "unit": unit,
                    "host": host,
                    "permitted": allowed,
                    "suggestion": suggestion(host, &domain),
                }),
            })
            .collect()
    }

    /// Every lane's [`Self::ca_warnings`], for `doctor`.
    pub(crate) fn all_ca_warnings(&self) -> Vec<LaneWarning> {
        self.all_lanes()
            .iter()
            .flat_map(|(project, lane)| {
                let st = lane.st();
                self.ca_warnings(&st.reference(&project.name()), &st)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Subtree matching is per label and ignores case, dots and wildcards.
    fn hosts_under_permitted_subtrees() {
        let allowed: Vec<String> = ["localhost", "test", "home.arpa", "acme"]
            .map(str::to_owned)
            .to_vec();
        for host in [
            "web.demo.localhost",
            "localhost",
            "*.pages.test",
            "API.Acme.",
            "x.home.arpa",
        ] {
            assert!(permitted(host, &allowed), "{host}");
        }
        for host in [
            "web.example.com",
            "localhost.example",
            "notlocalhost",
            "arpa",
            "*.acme.io",
        ] {
            assert!(!permitted(host, &allowed), "{host}");
        }
        assert_eq!(
            suggestion("api.example.com", "demo.localhost"),
            "api.demo.localhost"
        );
        assert_eq!(
            suggestion("*.pages.example.com", "demo.test"),
            "*.demo.test"
        );
    }
}

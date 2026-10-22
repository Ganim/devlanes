//! Doctor checks and daemon facts for the daemon page. The daemon sends raw
//! facts; the checks, their words and fixes come from `devlanes doctor`'s
//! renderer (`devlanes_cli::doctor_checks_of`), so both clients say the same.

use serde_json::Value;

use crate::text::clean;

/// How a check came out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    Ok,
    Warn,
    Fail,
}

impl CheckState {
    /// The word shown in the list.
    pub fn word(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// One doctor check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub group: &'static str,
    pub state: CheckState,
    pub text: String,
    pub fix: Option<String>,
}

/// A fact as cleaned text: strings bare, everything else as JSON, `unknown` when absent.
pub fn fact(data: &Value, key: &str) -> String {
    match data.get(key) {
        Some(Value::String(value)) => clean(value),
        Some(Value::Null) | None => "unknown".into(),
        Some(other) => clean(&other.to_string()),
    }
}

/// `2h 5m`, `3m 12s` or `40s`.
pub fn uptime(ms: u64) -> String {
    let seconds = ms / 1000;
    let (days, hours, minutes) = (seconds / 86_400, seconds / 3600 % 24, seconds / 60 % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{seconds}s"),
        (0, 0, m) => format!("{m}m {}s", seconds % 60),
        (0, h, m) => format!("{h}h {m}m"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Every check from the doctor facts, as `devlanes doctor` words them:
/// state database, sandbox, Docker, proxy listeners (serving or not), CA
/// and its route warnings, /etc/hosts and its helper, DNS, the user config,
/// the service manager's stop window against the services' stop timeouts.
pub fn checks(data: &Value) -> Vec<Check> {
    devlanes_cli::doctor_checks_of(data)
        .into_iter()
        .map(|check| Check {
            group: check.group,
            state: match check.state {
                devlanes_cli::DoctorState::Ok => CheckState::Ok,
                devlanes_cli::DoctorState::Warn => CheckState::Warn,
                devlanes_cli::DoctorState::Fail => CheckState::Fail,
            },
            text: clean(&check.text),
            fix: check.fix.map(|fix| clean(&fix)),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn find<'a>(checks: &'a [Check], needle: &str) -> Option<&'a Check> {
        checks.iter().find(|c| c.text.contains(needle))
    }

    #[test]
    fn checks_word_proxy_fallback_and_missing_ca() {
        let data = json!({
            "schema_version": 7, "supported_schema_version": 7, "db_path": "/h/state.db",
            "infra": {"docker": {"reachable": true, "version": "27.1"}, "compose": {"version": "2.29.1"}},
            "proxy": {
                "http": {"bound": true, "port": 80},
                "https": {"bound": true, "port": 8443, "fallback_reason": "port 443 needs root"}
            },
            "ca": {"present": false, "error": "not created"},
        });
        let checks = checks(&data);
        let https = find(&checks, "https on").map(|c| (c.state, c.text.as_str()));
        assert_eq!(
            https,
            Some((
                CheckState::Warn,
                "https on fallback port 8443: port 443 needs root"
            ))
        );
        let tls = checks.iter().find(|c| c.group == "tls");
        assert_eq!(
            tls.and_then(|c| c.fix.as_deref()),
            Some("devlanes certs trust")
        );
        assert!(checks.iter().any(|c| c.text == "Compose 2.29.1"));
    }

    /// The checks the CLI has and the TUI used to miss, in the CLI's words.
    #[test]
    fn the_page_has_every_check_the_cli_has() {
        let data = json!({
            "proxy": {
                "http": {"bound": true, "port": 80, "serving": false,
                    "listeners": [{"address": "127.0.0.1:80", "running": false}]},
            },
            "ca": {"present": true, "path": "/h/ca.pem"},
            "ca_warnings": [{"code": "host_outside_ca",
                "data": {"lane": "acme@0", "unit": "api", "host": "api.example.com"}}],
            "route_warnings": [{"code": "host_conflict",
                "data": {"host": "api.acme.localhost", "lane": "acme@0", "other": "acme@1"}}],
            "warnings": [{"code": "ports_pending", "data": {"lane": "acme@2"}}],
            "hosts": {"mode": "auto", "state": "applied", "names": 3, "last_error": null,
                "helper": {"state": "mismatch", "level": "warning", "installed": "0.1.0", "expected": "0.2.0"}},
            "dns": {"tld": "test", "listen": "127.0.0.1:15353", "running": true,
                "resolver_file": {"path": "/etc/resolver/test", "installed": false, "matches": false}},
            "user_config": {"path": "/h/config.yaml", "valid": false,
                "error": {"message": "unknown key `prxy`", "line": 3, "column": 1}, "warnings": []},
            "service": {"manager": "launchd agent", "path": "/h/agent.plist", "installed": true,
                "stop_window_s": 20, "window_source": "installed", "recommended_window_s": 45},
            "stop_timeouts": [{"project": "acme", "unit": "worker", "timeout_ms": 60000}],
        });
        let checks = checks(&data);
        let state = |needle: &str| find(&checks, needle).map(|c| c.state);
        assert_eq!(
            state("http on port 80 is not serving"),
            Some(CheckState::Fail)
        );
        assert_eq!(
            state("api.example.com is outside the CA's names"),
            Some(CheckState::Warn)
        );
        assert_eq!(
            state("api.acme.localhost is routed by acme@0 and acme@1"),
            Some(CheckState::Warn)
        );
        assert_eq!(state("config changed ports"), Some(CheckState::Warn));
        assert_eq!(
            state("the installed /etc/hosts helper is version 0.1.0, devlanes is 0.2.0"),
            Some(CheckState::Warn)
        );
        assert_eq!(
            state(".test lookups do not reach the devlanes DNS server"),
            Some(CheckState::Warn)
        );
        assert_eq!(
            state("user config /h/config.yaml:3:1: unknown key `prxy`"),
            Some(CheckState::Fail)
        );
        assert_eq!(
            state("gives the daemon 20s to stop"),
            Some(CheckState::Warn)
        );
        assert_eq!(
            state("1 unit with a stop timeout longer than"),
            Some(CheckState::Warn)
        );
        assert!(!checks.iter().any(|c| c.group == "output"));
    }

    #[test]
    fn uptime_reads_naturally() {
        assert_eq!(uptime(40_000), "40s");
        assert_eq!(uptime(192_000), "3m 12s");
        assert_eq!(uptime(7_500_000), "2h 5m");
        assert_eq!(uptime(90_000_000), "1d 1h");
    }
}

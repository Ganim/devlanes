//! Doctor checks and daemon facts for the daemon tab. The daemon sends raw
//! facts; the checks and fixes follow the CLI's `doctor` renderer
//! (`devlanes-cli` `render/other.rs`) so both clients agree.

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

impl Check {
    /// A check without a fix.
    fn new(group: &'static str, state: CheckState, text: impl Into<String>) -> Self {
        Self {
            group,
            state,
            text: text.into(),
            fix: None,
        }
    }

    /// Adds the fix.
    fn fix(mut self, fix: impl Into<String>) -> Self {
        self.fix = Some(fix.into());
        self
    }
}

/// A fact as cleaned text: strings bare, everything else as JSON, `unknown` when absent.
pub fn fact(data: &Value, key: &str) -> String {
    match data.get(key) {
        Some(Value::String(value)) => clean(value),
        Some(Value::Null) | None => "unknown".into(),
        Some(other) => clean(&other.to_string()),
    }
}

/// An integer fact.
fn int(data: &Value, key: &str) -> Option<i64> {
    data.get(key).and_then(Value::as_i64)
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

/// The `http` or `https` listener check.
fn listener(proxy: &Value, name: &'static str) -> Option<Check> {
    let entry = proxy.get(name)?;
    if entry.get("bound").and_then(Value::as_bool) == Some(true) {
        let port = fact(entry, "port");
        return Some(match entry.get("fallback_reason").and_then(Value::as_str) {
            Some(reason) => Check::new(
                "proxy",
                CheckState::Warn,
                format!("{name} on fallback port {port}: {}", clean(reason)),
            )
            .fix(format!(
                "free port {}, or set proxy ports in ~/.devlanes/config.yaml",
                if name == "https" { 443 } else { 80 }
            )),
            None => Check::new("proxy", CheckState::Ok, format!("{name} on port {port}")),
        });
    }
    if entry.get("reason").and_then(Value::as_str) == Some("no_ca") {
        return Some(
            Check::new(
                "proxy",
                CheckState::Warn,
                format!("{name} off: no local CA yet"),
            )
            .fix("devlanes certs trust"),
        );
    }
    Some(
        Check::new(
            "proxy",
            CheckState::Fail,
            format!(
                "{name} can't bind port {}: {}",
                fact(entry, "port"),
                fact(entry, "error")
            ),
        )
        .fix("stop whatever holds the port, then run devlanes daemon restart"),
    )
}

/// Every check from the doctor facts.
pub fn checks(data: &Value) -> Vec<Check> {
    let mut checks = Vec::new();
    match (
        int(data, "schema_version"),
        int(data, "supported_schema_version"),
    ) {
        (Some(found), Some(supported)) if found > supported => checks.push(
            Check::new(
                "state",
                CheckState::Fail,
                format!("state database is schema {found}; this devlanes reads up to {supported}"),
            )
            .fix("upgrade devlanes"),
        ),
        (Some(found), _) => checks.push(Check::new(
            "state",
            CheckState::Ok,
            format!("schema {found}, {}", fact(data, "db_path")),
        )),
        (None, _) => checks.push(
            Check::new(
                "state",
                CheckState::Fail,
                "can't read the state database schema",
            )
            .fix("devlanes daemon reset-state --yes"),
        ),
    }
    let sandbox = &data["sandbox"];
    let sandboxed: Vec<&str> = ["codex_sandbox", "app_sandbox"]
        .into_iter()
        .filter(|key| sandbox.get(*key).and_then(Value::as_bool) == Some(true))
        .collect();
    if !sandboxed.is_empty() {
        checks.push(
            Check::new(
                "sandbox",
                CheckState::Warn,
                format!("daemon runs inside a sandbox ({})", sandboxed.join(", ")),
            )
            .fix("run devlanes daemon install so the daemon starts outside the sandbox"),
        );
    }
    let docker = &data["infra"]["docker"];
    let reachable = docker.get("reachable").and_then(Value::as_bool) == Some(true);
    checks.push(if reachable {
        Check::new(
            "docker",
            CheckState::Ok,
            format!("Docker {}", fact(docker, "version")),
        )
    } else {
        Check::new("docker", CheckState::Warn, "Docker not reachable")
            .fix("start Docker (only projects with infra need it)")
    });
    let compose = &data["infra"]["compose"];
    if compose.get("version").is_some() {
        let unsupported = compose
            .get("supports_lane_overrides")
            .and_then(Value::as_bool)
            == Some(false);
        checks.push(if unsupported {
            Check::new(
                "docker",
                CheckState::Warn,
                format!(
                    "Compose {} can't apply per-lane overrides",
                    fact(compose, "version")
                ),
            )
            .fix("upgrade Docker Compose")
        } else {
            Check::new(
                "docker",
                CheckState::Ok,
                format!("Compose {}", fact(compose, "version")),
            )
        });
    } else if compose.get("error").is_some() && reachable {
        // Without Docker, Compose fails for the same reason; one check says it.
        checks.push(
            Check::new(
                "docker",
                CheckState::Warn,
                format!("Compose unavailable ({})", fact(compose, "error")),
            )
            .fix("install the Docker Compose plugin"),
        );
    }
    let proxy = &data["proxy"];
    checks.extend(
        ["http", "https"]
            .into_iter()
            .filter_map(|name| listener(proxy, name)),
    );
    let ca = &data["ca"];
    if ca.get("present").and_then(Value::as_bool) == Some(true) {
        checks.push(Check::new(
            "tls",
            CheckState::Ok,
            format!("CA at {}", fact(ca, "path")),
        ));
        // A CA that exists but is not trusted, or about to expire.
        if let Some(hint) = ca.get("hint").and_then(Value::as_str) {
            let mut check = Check::new("tls", CheckState::Warn, clean(hint));
            if let Some(fix) = ca.get("fix").and_then(Value::as_str) {
                check = check.fix(clean(fix));
            }
            checks.push(check);
        }
    } else if ca.is_object() {
        let error = fact(ca, "error");
        let text = if error.contains("no CA") || error.contains("not created") {
            "no local CA yet".to_owned()
        } else {
            format!("CA error: {error}")
        };
        checks.push(Check::new("tls", CheckState::Fail, text).fix("devlanes certs trust"));
    }
    let hosts = &data["hosts"];
    if hosts.is_object() {
        let mode = fact(hosts, "mode");
        let probe = hosts.get("probe").and_then(Value::as_str).map_or_else(
            || "resolver not checked".to_owned(),
            |probe| match probe {
                "localhost_resolves" => ".localhost resolves".to_owned(),
                other => format!("resolver {}", clean(other).replace('_', " ")),
            },
        );
        if let Some(error) = hosts.get("last_error").filter(|e| e.is_object()) {
            let check = Check::new(
                "hosts",
                CheckState::Fail,
                format!("/etc/hosts fallback {mode}: {}", fact(error, "message")),
            );
            checks.push(match error.get("code").and_then(Value::as_str) {
                Some("hosts_file_refused") => {
                    check.fix("fix the devlanes markers in /etc/hosts by hand")
                }
                _ => check.fix("devlanes certs trust --hosts"),
            });
        } else {
            let state = match (
                hosts.get("state").and_then(Value::as_str),
                int(hosts, "names"),
            ) {
                (Some("applied"), Some(names)) => format!("{names} names"),
                (Some("idle"), _) => "no names written".into(),
                (Some(other), _) => clean(other),
                (None, _) => "unknown".into(),
            };
            checks.push(Check::new(
                "hosts",
                CheckState::Ok,
                format!("{probe}; /etc/hosts fallback {mode}, {state}"),
            ));
        }
        if let Some(error) = hosts.get("tld_error").filter(|e| e.is_object()) {
            checks.push(Check::new(
                "hosts",
                CheckState::Warn,
                format!(
                    "{}; only .localhost names are written",
                    fact(error, "message")
                ),
            ));
        }
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        let https = checks
            .iter()
            .find(|c| c.text.starts_with("https"))
            .map(|c| (c.state, c.text.as_str()));
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

    #[test]
    fn uptime_reads_naturally() {
        assert_eq!(uptime(40_000), "40s");
        assert_eq!(uptime(192_000), "3m 12s");
        assert_eq!(uptime(7_500_000), "2h 5m");
        assert_eq!(uptime(90_000_000), "1d 1h");
    }
}

//! `config check` warnings for `platform:` values no registered contributor provides.
//!
//! An unknown platform is a warning, not an error: the service still runs, it just
//! gets no platform env (Node's CA variables, Python's venv and the like).

use std::sync::Arc;

use devlanes_config::ProjectConfig;
use devlanes_core::contrib::Contributor;
use serde_json::{Value, json};

/// Platform names the registered contributors provide, sorted and unique.
pub(crate) fn known(contributors: &[Arc<dyn Contributor>]) -> Vec<String> {
    let mut names: Vec<String> = contributors
        .iter()
        .filter_map(|contributor| contributor.platform())
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

/// One `unknown_platform` warning per service whose explicit `platform:` is not in
/// `known`: `{code, path, message, data: {service, platform, known}}`. Nothing is
/// reported without any platform contributor, since then every name would be unknown.
pub(crate) fn unknown(config: &ProjectConfig, known: &[String]) -> Vec<Value> {
    if known.is_empty() {
        return Vec::new();
    }
    config
        .services
        .iter()
        .filter_map(|(name, service)| {
            let platform = service.platform.as_ref()?;
            if known.iter().any(|known| known == platform) {
                return None;
            }
            Some(json!({
                "code": "unknown_platform",
                "path": format!("services.{name}.platform"),
                "message": format!("no contributor provides platform `{platform}`"),
                "data": {"service": name, "platform": platform, "known": known},
            }))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A parsed project config.
    fn config(yaml: &str) -> ProjectConfig {
        devlanes_config::parse_project(yaml, None).expect("config")
    }

    #[test]
    fn explicit_platforms_without_a_contributor_are_reported() {
        let config = config(
            "project: acme\nservices:\n  web:\n    run: serve\n    platform: node\n  api:\n    run: serve\n    platform: elixir\n  worker:\n    run: work\n",
        );
        let known = vec!["node".to_owned(), "python".to_owned()];
        let warnings = unknown(&config, &known);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0]["code"], "unknown_platform");
        assert_eq!(warnings[0]["path"], "services.api.platform");
        assert_eq!(
            warnings[0]["data"],
            json!({"service": "api", "platform": "elixir", "known": ["node", "python"]})
        );
        assert!(
            unknown(&config, &[]).is_empty(),
            "no contributors, no warnings"
        );
    }
}

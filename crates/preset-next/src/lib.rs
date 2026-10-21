//! `preset: next`: binds `next dev` to the service's allocated port.
//!
//! Flags follow <https://nextjs.org/docs/app/api-reference/cli/next>:
//! `--port` and `--hostname`. Next binds `0.0.0.0` by default, so the
//! hostname is pinned to loopback like every other service.

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ContribCx, Contributor, LaunchDraft},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Preset name, also the key that holds [`NextOptions`] in a service.
pub const NAME: &str = "next";

/// Address the dev server binds.
pub const HOST: &str = "127.0.0.1";

/// Options under the service's `next:` key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NextOptions {
    /// Extra arguments appended after the preset's flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
}

impl NextOptions {
    /// Applies a local override or `extends` overlay; set fields replace.
    pub fn merge(&mut self, overlay: Self) {
        if overlay.args.is_some() {
            self.args = overlay.args;
        }
    }
}

/// Arguments and environment a preset adds to a service's launch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Contribution {
    /// Appended to the command; runners decide whether a `--` precedes them.
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}

/// Returns the named ports to allocate before expressions are evaluated. Next
/// needs only `self.port`.
pub fn required_ports() -> &'static [&'static str] {
    &[]
}

/// Renders the flags for a dev server on `port`.
pub fn contribution(port: u16, options: &NextOptions) -> Contribution {
    let mut args = vec![
        "--port".to_owned(),
        port.to_string(),
        "--hostname".to_owned(),
        HOST.to_owned(),
    ];
    args.extend(options.args.iter().flatten().cloned());
    Contribution {
        args,
        env: BTreeMap::new(),
    }
}

/// JSON Schema of [`NextOptions`], for the config's `Extensions` registration.
pub fn options_schema() -> schemars::Schema {
    schemars::schema_for!(NextOptions)
}

/// Parses evaluated options; a service without a `next:` key gets defaults.
pub fn parse_options(value: Option<&serde_json::Value>) -> Result<NextOptions, Error> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(NextOptions::default()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::InvalidInput(format!("{NAME} options: {e}"))),
    }
}

/// Core contributor for services with `preset: next`.
#[derive(Debug, Default, Clone, Copy)]
pub struct NextPreset;

#[async_trait]
impl Contributor for NextPreset {
    fn id(&self) -> &'static str {
        "preset-next"
    }

    fn priority(&self) -> i32 {
        100
    }

    /// Appends the dev server flags; other services are left untouched.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.preset != Some(NAME) {
            return Ok(());
        }
        let options = parse_options(cx.preset_options)?;
        let port = cx
            .port
            .ok_or_else(|| Error::InvalidInput(format!("preset {NAME} needs a service port")))?;
        let contribution = contribution(port, &options);
        devlanes_run_script_npm::forward_args(draft, contribution.args);
        draft.env.extend(contribution.env);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_core::contrib::{ContribMemo, DraftCommand, UnitKind};
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    /// Runs the contributor on a draft for `command` with a fake context.
    async fn apply(
        preset: Option<&str>,
        options: Option<serde_json::Value>,
        command: DraftCommand,
    ) -> Result<LaunchDraft, Error> {
        let ports = BTreeMap::new();
        let empty = BTreeMap::new();
        let memo = ContribMemo::default();
        let cx = ContribCx {
            kind: UnitKind::Service,
            project: "acme",
            name: "site",
            replica: Some(1),
            lane_index: 0,
            lane_name: "main",
            lane_path: Path::new("/lanes/acme"),
            primary_path: Path::new("/lanes/acme"),
            platform: Some("node"),
            preset,
            preset_options: options.as_ref(),
            port: Some(20013),
            ports: &ports,
            public_url: None,
            pty: true,
            script: None,
            client_env: &empty,
            ca_env: &empty,
            memo: &memo,
        };
        let mut draft = LaunchDraft {
            command,
            args: Vec::new(),
            cwd: PathBuf::from("/lanes/acme"),
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        NextPreset.apply(&mut draft, &cx).await?;
        Ok(draft)
    }

    /// Only `preset: next` services get flags; argv commands get them appended.
    #[tokio::test]
    async fn adapter_applies_only_to_next_services() {
        let argv =
            |items: &[&str]| DraftCommand::Argv(items.iter().map(|s| (*s).to_owned()).collect());
        let other = apply(None, None, argv(&["next", "dev"]))
            .await
            .expect("apply");
        assert!(other.args.is_empty());
        let plain = apply(
            Some(NAME),
            Some(serde_json::json!({"args": null})),
            argv(&["next", "dev"]),
        )
        .await
        .expect("apply");
        assert_eq!(plain.args.join(" "), "--port 20013 --hostname 127.0.0.1");
        let npm = apply(Some(NAME), None, argv(&["npm", "run", "dev"]))
            .await
            .expect("apply");
        assert_eq!(npm.args.first().map(String::as_str), Some("--"));
        let script = apply(
            Some(NAME),
            None,
            DraftCommand::Script {
                source: "package.json".into(),
                name: "dev".into(),
            },
        )
        .await
        .expect("apply");
        assert_eq!(script.args.first().map(String::as_str), Some("--port"));
    }

    /// The exported schema is the options struct's schema.
    #[test]
    fn schema_lists_args() {
        let schema = options_schema().to_value();
        assert!(schema["properties"]["args"].is_object(), "{schema}");
    }

    /// User args come after the preset's own flags.
    #[test]
    fn renders_flags_then_user_args() {
        let options = NextOptions {
            args: Some(vec!["--turbopack".into()]),
        };
        insta::assert_snapshot!(contribution(3000, &options).args.join(" "), @"--port 3000 --hostname 127.0.0.1 --turbopack");
    }

    /// The options reject unknown keys so typos surface in `config check`.
    #[test]
    fn options_reject_unknown_keys() {
        let error =
            serde_json::from_value::<NextOptions>(serde_json::json!({"hostname": "0.0.0.0"}));
        assert!(error.is_err());
    }
}

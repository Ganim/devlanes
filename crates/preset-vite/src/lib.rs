//! `preset: vite`: binds the Vite dev server to the service's allocated port.
//!
//! Flags follow <https://vite.dev/guide/cli>: `--port`, `--strictPort` (exit
//! instead of silently moving to the next free port, which would break the
//! proxy route) and `--host 127.0.0.1`.

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ContribCx, Contributor, LaunchDraft},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Preset name, also the key that holds [`ViteOptions`] in a service.
pub const NAME: &str = "vite";

/// Address the dev server binds; the proxy and `self.url` use loopback.
pub const HOST: &str = "127.0.0.1";

/// Options under the service's `vite:` key.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"args":["--open"]}]))]
pub struct ViteOptions {
    /// Extra arguments appended after the preset's flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = [["--open"]]))]
    pub args: Option<Vec<String>>,
}

impl ViteOptions {
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

/// Returns the named ports to allocate before expressions are evaluated. Vite
/// needs only `self.port`.
pub fn required_ports() -> &'static [&'static str] {
    &[]
}

/// Renders the flags for a dev server on `port`.
pub fn contribution(port: u16, options: &ViteOptions) -> Contribution {
    let mut args = vec![
        "--port".to_owned(),
        port.to_string(),
        "--strictPort".to_owned(),
        "--host".to_owned(),
        HOST.to_owned(),
    ];
    args.extend(options.args.iter().flatten().cloned());
    Contribution {
        args,
        env: BTreeMap::new(),
    }
}

/// JSON Schema of [`ViteOptions`], for the config's `Extensions` registration.
pub fn options_schema() -> schemars::Schema {
    schemars::schema_for!(ViteOptions)
}

/// Parses evaluated options; a service without a `vite:` key gets defaults.
pub fn parse_options(value: Option<&serde_json::Value>) -> Result<ViteOptions, Error> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(ViteOptions::default()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::InvalidInput(format!("{NAME} options: {e}"))),
    }
}

/// Core contributor for services with `preset: vite`.
#[derive(Debug, Default, Clone, Copy)]
pub struct VitePreset;

#[async_trait]
impl Contributor for VitePreset {
    fn id(&self) -> &'static str {
        "preset-vite"
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
            name: "web",
            replica: Some(1),
            lane_index: 1,
            lane_name: "lane1",
            lane_path: Path::new("/lanes/acme-1"),
            primary_path: Path::new("/lanes/acme"),
            platform: Some("node"),
            preset,
            preset_options: options.as_ref(),
            port: Some(20012),
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
            cwd: PathBuf::from("/lanes/acme-1"),
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        VitePreset.apply(&mut draft, &cx).await?;
        Ok(draft)
    }

    /// Only `preset: vite` services get flags; a plain `npm run` gets `--` first.
    #[tokio::test]
    async fn adapter_applies_only_to_vite_services() {
        let shell = |text: &str| DraftCommand::Shell(text.to_owned());
        let other = apply(Some("next"), None, shell("vite"))
            .await
            .expect("apply");
        assert!(other.args.is_empty());
        let plain = apply(Some(NAME), None, shell("pnpm --silent -F @acme/web dev"))
            .await
            .expect("apply");
        assert_eq!(
            plain.args.join(" "),
            "--port 20012 --strictPort --host 127.0.0.1"
        );
        let npm = apply(
            Some(NAME),
            Some(serde_json::json!({"args": ["--open"]})),
            shell("npm run dev"),
        )
        .await
        .expect("apply");
        assert_eq!(
            npm.args.join(" "),
            "-- --port 20012 --strictPort --host 127.0.0.1 --open"
        );
        let error = apply(
            Some(NAME),
            Some(serde_json::json!({"port": 1})),
            shell("vite"),
        )
        .await;
        assert!(matches!(error, Err(Error::InvalidInput(_))));
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
        let options = ViteOptions {
            args: Some(vec!["--mode".into(), "staging".into()]),
        };
        insta::assert_snapshot!(contribution(5173, &options).args.join(" "), @"--port 5173 --strictPort --host 127.0.0.1 --mode staging");
    }

    /// The options reject unknown keys so typos surface in `config check`.
    #[test]
    fn options_reject_unknown_keys() {
        let error = serde_json::from_value::<ViteOptions>(serde_json::json!({"arg": []}));
        assert!(error.is_err());
    }

    /// Overlays replace args only when they set them.
    #[test]
    fn merge_keeps_unset_fields() {
        let mut base = ViteOptions {
            args: Some(vec!["--open".into()]),
        };
        base.merge(ViteOptions::default());
        assert_eq!(base.args.as_deref(), Some(&["--open".to_owned()][..]));
    }
}

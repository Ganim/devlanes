//! `preset: tauri`: runs several Tauri dev apps side by side on one machine.
//!
//! Each lane and replica gets its own bundle `identifier`, which keys the app
//! data, webview data and keychain entries, plus its own `build.devUrl`. Both
//! go into a JSON overlay passed with `tauri dev --config <file>`, which Tauri
//! v2 merges over `tauri.conf.json` (<https://v2.tauri.app/reference/cli/#dev>).
//!
//! Local only: the app runs on the machine that runs the lane. Remote boxes
//! need GUI streaming, which is out of scope for v1.

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ContribCx, Contributor, LaunchDraft, PreparedFile},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Preset name, also the key that holds [`TauriOptions`] in a service.
pub const NAME: &str = "tauri";

/// Also set to the overlay path, for wrapper scripts that start `tauri dev`
/// themselves and do not forward extra arguments.
pub const CONFIG_ENV: &str = "DEVLANES_TAURI_CONFIG";

/// Options under the service's `tauri:` key, after expression evaluation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"identifier":"io.acme.app.lane${{ lane.index }}.client${{ replica.index }}","dev_url":"${{ svc.desktop_web.url }}","before_dev_command":""}]))]
pub struct TauriOptions {
    /// Bundle identifier for this lane and replica. Required; it must differ per
    /// lane and replica, for example
    /// `com.acme.app.dev${{ '' if lane.index == 0 else '.lane' ~ lane.index }}.client${{ replica.index }}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = ["io.acme.app.lane${{ lane.index }}.client${{ replica.index }}"]))]
    pub identifier: Option<String>,
    /// `productName` override, shown in the dock and window menus.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = ["Acme Dev"]))]
    pub product_name: Option<String>,
    /// `build.devUrl`, usually another service's `url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = ["${{ svc.desktop_web.url }}"]))]
    pub dev_url: Option<String>,
    /// `build.beforeDevCommand` override. Set it to an empty string when the
    /// frontend runs as its own devlanes service; otherwise every replica
    /// starts its own frontend on the port from `tauri.conf.json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(extend("examples" = ["","pnpm dev"]))]
    pub before_dev_command: Option<String>,
}

impl TauriOptions {
    /// Applies a local override or `extends` overlay; set fields replace.
    pub fn merge(&mut self, overlay: Self) {
        let Self {
            identifier,
            product_name,
            dev_url,
            before_dev_command,
        } = overlay;
        if identifier.is_some() {
            self.identifier = identifier;
        }
        if product_name.is_some() {
            self.product_name = product_name;
        }
        if dev_url.is_some() {
            self.dev_url = dev_url;
        }
        if before_dev_command.is_some() {
            self.before_dev_command = before_dev_command;
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

/// Everything [`contribution`] needs for one replica.
#[derive(Debug, Clone, Copy)]
pub struct TauriInput<'a> {
    pub options: &'a TauriOptions,
    /// Absolute path the overlay is written to; one per lane and replica.
    pub overlay_path: &'a Path,
}

/// The launch additions plus the overlay the caller writes with
/// [`write_overlay`] before spawning.
#[derive(Debug, Clone, PartialEq)]
pub struct TauriLaunch {
    pub contribution: Contribution,
    pub overlay: Value,
}

/// Errors that stop a Tauri replica from starting.
#[derive(Debug, thiserror::Error)]
pub enum TauriError {
    #[error(
        "preset tauri needs `tauri.identifier`, distinct per lane and replica, e.g. `com.acme.app.dev.lane${{{{ lane.index }}}}.client${{{{ replica.index }}}}`"
    )]
    MissingIdentifier,
    #[error("tauri identifier `{identifier}` is invalid: {reason}")]
    InvalidIdentifier {
        identifier: String,
        reason: &'static str,
    },
    #[error("tauri dev_url `{url}` must start with http:// or https://")]
    InvalidDevUrl { url: String },
    #[error(
        "`{first}` and `{second}` both use tauri identifier `{identifier}`; include lane.index and replica.index in it so their app data stays apart"
    )]
    DuplicateIdentifier {
        identifier: String,
        first: String,
        second: String,
    },
    #[error("cannot write the tauri config overlay `{}`", .path.display())]
    Write {
        path: PathBuf,
        #[source]
        error: std::io::Error,
    },
}

/// Returns the named ports to allocate before expressions are evaluated. The
/// app itself listens on nothing devlanes routes.
pub fn required_ports() -> &'static [&'static str] {
    &[]
}

/// Returns the conventional overlay path for `service` replica `replica` in a lane.
pub fn overlay_path(lane_worktree: &Path, service: &str, replica: u32) -> PathBuf {
    lane_worktree
        .join(".devlanes")
        .join("tauri")
        .join(format!("{service}.{replica}.conf.json"))
}

/// Builds the Tauri v2 config overlay: top-level `identifier` and
/// `productName`, `build.devUrl` and `build.beforeDevCommand`.
pub fn overlay(options: &TauriOptions) -> Result<Value, TauriError> {
    let identifier = options
        .identifier
        .as_deref()
        .ok_or(TauriError::MissingIdentifier)?;
    validate_identifier(identifier)?;
    let mut root = Map::new();
    root.insert("identifier".into(), identifier.into());
    if let Some(name) = &options.product_name {
        root.insert("productName".into(), name.as_str().into());
    }
    let mut build = Map::new();
    if let Some(url) = &options.dev_url {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            return Err(TauriError::InvalidDevUrl { url: url.clone() });
        }
        build.insert("devUrl".into(), url.as_str().into());
    }
    if let Some(command) = &options.before_dev_command {
        build.insert("beforeDevCommand".into(), command.as_str().into());
    }
    if !build.is_empty() {
        root.insert("build".into(), Value::Object(build));
    }
    Ok(Value::Object(root))
}

/// Renders `--config <overlay_path>` and the overlay to write there.
pub fn contribution(input: &TauriInput<'_>) -> Result<TauriLaunch, TauriError> {
    let overlay = overlay(input.options)?;
    let path = input.overlay_path.to_string_lossy().into_owned();
    Ok(TauriLaunch {
        contribution: Contribution {
            args: vec!["--config".to_owned(), path.clone()],
            env: BTreeMap::from([(CONFIG_ENV.to_owned(), path)]),
        },
        overlay,
    })
}

/// Writes `overlay` to `path` through a temporary file and a rename, so a
/// starting replica never reads a half-written file. Creates parent
/// directories. Blocking.
pub fn write_overlay(path: &Path, overlay: &Value) -> Result<(), TauriError> {
    let write_error = |error| TauriError::Write {
        path: path.to_owned(),
        error,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(write_error)?;
    }
    let mut bytes =
        serde_json::to_vec_pretty(overlay).map_err(|error| write_error(error.into()))?;
    bytes.push(b'\n');
    let file_name = path
        .file_name()
        .map_or_else(Default::default, |name| name.to_string_lossy());
    let temp = path.with_file_name(format!(".{file_name}.tmp"));
    std::fs::write(&temp, bytes).map_err(write_error)?;
    std::fs::rename(&temp, path).map_err(write_error)
}

/// Fails when two instances share an identifier. `instances` pairs a label
/// for messages (such as `desktop#2 in lane 1`) with the evaluated identifier.
/// Call it across every lane and replica of a project before starting any.
pub fn ensure_distinct<'a>(
    instances: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<(), TauriError> {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for (label, identifier) in instances {
        if let Some(first) = seen.insert(identifier, label) {
            return Err(TauriError::DuplicateIdentifier {
                identifier: identifier.to_owned(),
                first: first.to_owned(),
                second: label.to_owned(),
            });
        }
    }
    Ok(())
}

/// Applies Tauri's rule ("only alphanumeric characters, hyphens and periods",
/// <https://v2.tauri.app/reference/config/#identifier>) plus non-empty dot
/// segments, which macOS bundle identifiers require.
fn validate_identifier(identifier: &str) -> Result<(), TauriError> {
    let reason = if identifier.is_empty() {
        "it is empty"
    } else if !identifier
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
    {
        "only A-Z, a-z, 0-9, `-` and `.` are allowed"
    } else if identifier.split('.').any(str::is_empty) {
        "it has an empty segment between dots"
    } else {
        return Ok(());
    };
    Err(TauriError::InvalidIdentifier {
        identifier: identifier.to_owned(),
        reason,
    })
}

/// JSON Schema of [`TauriOptions`], for the config's `Extensions` registration.
pub fn options_schema() -> schemars::Schema {
    schemars::schema_for!(TauriOptions)
}

/// Parses evaluated options. A service without a `tauri:` key gets defaults,
/// which then fail on the missing identifier with an example.
pub fn parse_options(value: Option<&Value>) -> Result<TauriOptions, Error> {
    match value {
        None | Some(Value::Null) => Ok(TauriOptions::default()),
        Some(value) => serde_json::from_value(value.clone())
            .map_err(|e| Error::InvalidInput(format!("{NAME} options: {e}"))),
    }
}

/// Identifiers claimed by replicas resolved so far in one lane, keyed by
/// identifier with the claiming replica's label.
type Claims = Mutex<BTreeMap<String, String>>;

/// Core contributor for services with `preset: tauri`.
///
/// The overlay goes into `draft.files` as a [`PreparedFile`] rather than being
/// written here: contributors only plan, and the daemon writes prepared files
/// right before it spawns, so a failed resolution leaves no stale overlay.
#[derive(Debug, Default, Clone, Copy)]
pub struct TauriPreset;

#[async_trait]
impl Contributor for TauriPreset {
    fn id(&self) -> &'static str {
        "preset-tauri"
    }

    fn priority(&self) -> i32 {
        100
    }

    /// Adds `--config <overlay>` and the overlay file for this replica, after
    /// checking that no other replica in the lane uses the same identifier.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.preset != Some(NAME) {
            return Ok(());
        }
        let options = parse_options(cx.preset_options)?;
        let replica = cx.replica.unwrap_or(1);
        let path = overlay_path(cx.lane_path, cx.name, u32::from(replica));
        let launch = contribution(&TauriInput {
            options: &options,
            overlay_path: &path,
        })
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
        if let Some(identifier) = options.identifier.as_deref() {
            claim(cx, identifier, &format!("{}#{replica}", cx.name))?;
        }
        let mut contents = serde_json::to_vec_pretty(&launch.overlay)
            .map_err(|e| Error::Integration(format!("cannot render the tauri overlay: {e}")))?;
        contents.push(b'\n');
        draft.files.push(PreparedFile { path, contents });
        devlanes_run_script_npm::forward_args(draft, launch.contribution.args);
        draft.env.extend(launch.contribution.env);
        Ok(())
    }
}

/// Records `identifier` for `label` in the lane's memo and fails when another
/// replica already holds it. Lanes are resolved separately, so this catches
/// expressions that ignore `replica.index`; [`ensure_distinct`] covers lanes.
fn claim(cx: &ContribCx<'_>, identifier: &str, label: &str) -> Result<(), Error> {
    let key = format!("{NAME}:identifiers:{}", cx.lane_path.display());
    let claims = cx.memo.get::<Claims>(&key).unwrap_or_else(|| {
        let claims = Arc::new(Claims::default());
        cx.memo.insert(key, claims.clone());
        claims
    });
    let mut claims = claims.lock().unwrap_or_else(PoisonError::into_inner);
    match claims.get(identifier) {
        Some(first) if first != label => Err(Error::InvalidInput(
            TauriError::DuplicateIdentifier {
                identifier: identifier.to_owned(),
                first: first.clone(),
                second: label.to_owned(),
            }
            .to_string(),
        )),
        _ => {
            claims.insert(identifier.to_owned(), label.to_owned());
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_core::contrib::{ContribMemo, DraftCommand, UnitKind};
    use std::collections::BTreeSet;

    /// Runs [`TauriPreset`] for replica `replica` of `desktop` in lane 1.
    async fn apply_replica(
        memo: &ContribMemo,
        options: Option<Value>,
        replica: u16,
    ) -> Result<LaunchDraft, Error> {
        let ports = BTreeMap::new();
        let empty = BTreeMap::new();
        let cx = ContribCx {
            kind: UnitKind::Service,
            project: "acme",
            name: "desktop",
            replica: Some(replica),
            lane_index: 1,
            lane_name: "lane1",
            lane_path: Path::new("/lanes/acme-1"),
            primary_path: Path::new("/lanes/acme"),
            platform: Some("node"),
            preset: Some(NAME),
            preset_options: options.as_ref(),
            port: Some(20015),
            ports: &ports,
            public_url: None,
            pty: true,
            script: None,
            client_env: &empty,
            ca_env: &empty,
            memo,
        };
        let mut draft = LaunchDraft {
            command: DraftCommand::Shell("pnpm --filter @acme/desktop dev".to_owned()),
            args: Vec::new(),
            cwd: PathBuf::from("/lanes/acme-1"),
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        TauriPreset.apply(&mut draft, &cx).await?;
        Ok(draft)
    }

    /// Each replica gets its own overlay file, `--config` and env pointer.
    #[tokio::test]
    async fn adapter_prepares_overlay_per_replica() {
        let memo = ContribMemo::default();
        for replica in [1_u16, 2] {
            let options = serde_json::json!({
                "identifier": format!("com.acme.app.dev.lane1.client{replica}"),
                "dev_url": "http://127.0.0.1:20014",
                "before_dev_command": "",
                "product_name": null,
            });
            let draft = apply_replica(&memo, Some(options), replica)
                .await
                .expect("apply");
            let expected = format!("/lanes/acme-1/.devlanes/tauri/desktop.{replica}.conf.json");
            assert_eq!(draft.args, ["--config", expected.as_str()]);
            assert_eq!(draft.env.get(CONFIG_ENV), Some(&expected));
            let [file] = draft.files.as_slice() else {
                panic!("expected one prepared file, got {}", draft.files.len());
            };
            assert_eq!(file.path, Path::new(&expected));
            let written: Value = serde_json::from_slice(&file.contents).expect("overlay json");
            assert_eq!(
                written["identifier"],
                format!("com.acme.app.dev.lane1.client{replica}")
            );
            assert_eq!(written["build"]["devUrl"], "http://127.0.0.1:20014");
        }
    }

    /// Replicas sharing an identifier, a missing identifier and unknown keys fail.
    #[tokio::test]
    async fn adapter_rejects_bad_options() {
        let memo = ContribMemo::default();
        let same = || Some(serde_json::json!({ "identifier": "com.acme.app.dev" }));
        apply_replica(&memo, same(), 1)
            .await
            .expect("first replica");
        // `LaunchDraft` has no `Debug` (it may hold secrets), so no `expect_err`.
        let duplicate = apply_replica(&memo, same(), 2)
            .await
            .err()
            .expect("duplicate");
        assert!(
            duplicate
                .to_string()
                .contains("`desktop#1` and `desktop#2`"),
            "{duplicate}"
        );
        let missing = apply_replica(&ContribMemo::default(), None, 1)
            .await
            .err()
            .expect("missing");
        assert!(
            missing.to_string().contains("tauri.identifier"),
            "{missing}"
        );
        let unknown = apply_replica(
            &ContribMemo::default(),
            Some(serde_json::json!({ "bundle_id": "x" })),
            1,
        )
        .await;
        assert!(matches!(unknown, Err(Error::InvalidInput(_))));
    }

    /// The exported schema is the options struct's schema.
    #[test]
    fn schema_lists_identifier() {
        let schema = options_schema().to_value();
        assert!(schema["properties"]["identifier"].is_object(), "{schema}");
    }

    fn options(identifier: &str) -> TauriOptions {
        TauriOptions {
            identifier: Some(identifier.to_owned()),
            dev_url: Some("http://127.0.0.1:20014".to_owned()),
            before_dev_command: Some(String::new()),
            product_name: None,
        }
    }

    /// The overlay uses Tauri v2 key names and nesting. Compared as values:
    /// key order depends on serde_json's workspace-unified `preserve_order`.
    #[test]
    fn overlay_shape() {
        let mut with_name = options("com.acme.app.dev.lane1.client2");
        with_name.product_name = Some("Acme Dev 2".to_owned());
        let value = overlay(&with_name).expect("valid options");
        let expected = serde_json::json!({
            "identifier": "com.acme.app.dev.lane1.client2",
            "productName": "Acme Dev 2",
            "build": { "devUrl": "http://127.0.0.1:20014", "beforeDevCommand": "" }
        });
        assert_eq!(value, expected);
        let minimal = TauriOptions {
            identifier: Some("com.acme".to_owned()),
            ..TauriOptions::default()
        };
        assert_eq!(
            overlay(&minimal).expect("valid options"),
            serde_json::json!({"identifier": "com.acme"})
        );
    }

    /// `--config` and the env variable point at the same file.
    #[test]
    fn contribution_points_at_overlay() {
        let options = options("com.acme.app.dev.client1");
        let path = overlay_path(Path::new("/lanes/acme"), "desktop", 1);
        let launch = contribution(&TauriInput {
            options: &options,
            overlay_path: &path,
        })
        .expect("valid options");
        insta::assert_snapshot!(launch.contribution.args.join(" "), @"--config /lanes/acme/.devlanes/tauri/desktop.1.conf.json");
        assert_eq!(
            launch.contribution.env.get(CONFIG_ENV).map(String::as_str),
            Some("/lanes/acme/.devlanes/tauri/desktop.1.conf.json")
        );
    }

    /// Identifier problems are reported before anything starts.
    #[test]
    fn identifier_validation() {
        assert!(matches!(
            overlay(&TauriOptions::default()),
            Err(TauriError::MissingIdentifier)
        ));
        for bad in ["", "com.acme app", "com..acme", "com.acme.", "com_acme"] {
            assert!(
                matches!(
                    overlay(&options(bad)),
                    Err(TauriError::InvalidIdentifier { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
        let mut bad_url = options("com.acme");
        bad_url.dev_url = Some("127.0.0.1:1".to_owned());
        assert!(matches!(
            overlay(&bad_url),
            Err(TauriError::InvalidDevUrl { .. })
        ));
    }

    /// Two lanes or replicas sharing an identifier would share app data.
    #[test]
    fn duplicate_identifiers_are_rejected() {
        let ok = ensure_distinct([("desktop#1", "a.client1"), ("desktop#2", "a.client2")]);
        assert!(ok.is_ok());
        let error = ensure_distinct([
            ("lane 0 desktop#1", "a.client1"),
            ("lane 1 desktop#1", "a.client1"),
        ])
        .expect_err("duplicate identifier");
        insta::assert_snapshot!(error.to_string(), @"`lane 0 desktop#1` and `lane 1 desktop#1` both use tauri identifier `a.client1`; include lane.index and replica.index in it so their app data stays apart");
    }

    /// The written overlay round-trips and leaves no temporary file behind.
    #[test]
    fn write_overlay_is_atomic() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = overlay_path(dir.path(), "desktop", 2);
        let value = overlay(&options("com.acme.client2")).expect("valid options");
        write_overlay(&path, &value).expect("write overlay");
        let read: Value =
            serde_json::from_slice(&std::fs::read(&path).expect("read overlay")).expect("json");
        assert_eq!(read, value);
        let entries = std::fs::read_dir(path.parent().expect("parent"))
            .expect("list")
            .count();
        assert_eq!(entries, 1);
    }

    /// The missing-identifier message shows a working expression.
    #[test]
    fn missing_identifier_message() {
        insta::assert_snapshot!(TauriError::MissingIdentifier.to_string(), @"preset tauri needs `tauri.identifier`, distinct per lane and replica, e.g. `com.acme.app.dev.lane${{ lane.index }}.client${{ replica.index }}`");
    }
}

use devlanes_core::Error;
use devlanes_core::contrib::{
    ContribCx, ContribMemo, Contributor, DraftCommand, LaunchDraft, ScriptInfo, ServiceView,
    UnitKind,
};
use devlanes_preset_next as next;
use devlanes_preset_tauri as tauri;
use devlanes_preset_vite as vite;
use devlanes_preset_wrangler::{
    self as wrangler, INSPECTOR_PORT, REGISTRY_ENV, WranglerError, WranglerPreset,
};
use devlanes_run_script_npm::{PackageManager, ScriptRequest, plan};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

const LANE: &str = "/lanes/acme-1";

/// Inputs that vary between adapter tests.
struct Fake {
    preset: Option<&'static str>,
    options: Option<serde_json::Value>,
    command: DraftCommand,
    script_body: Option<&'static str>,
    preset_env: BTreeMap<String, String>,
}

impl Fake {
    fn new(command: DraftCommand) -> Self {
        Self {
            preset: Some(wrangler::NAME),
            options: None,
            command,
            script_body: None,
            preset_env: BTreeMap::new(),
        }
    }

    /// Runs [`WranglerPreset`] with a fake context for service `api` in lane 1.
    async fn apply(self) -> Result<LaunchDraft, Error> {
        let ports = BTreeMap::from([(INSPECTOR_PORT.to_owned(), 20011)]);
        let empty = BTreeMap::new();
        let memo = ContribMemo::default();
        let script = self.script_body.map(|body| ScriptInfo {
            source: PathBuf::from(LANE).join("apps/api/package.json"),
            name: "dev".to_owned(),
            body: Some(body.to_owned()),
            cwd: PathBuf::from(LANE).join("apps/api"),
            runner: "npm",
        });
        let cx = ContribCx {
            kind: UnitKind::Service,
            project: "acme",
            name: "api",
            replica: Some(1),
            lane_index: 1,
            lane_name: "lane1",
            lane_path: Path::new(LANE),
            primary_path: Path::new("/lanes/acme"),
            platform: Some("node"),
            preset: self.preset,
            preset_options: self.options.as_ref(),
            port: Some(20010),
            ports: &ports,
            public_url: Some("https://api.acme-1.localhost"),
            pty: true,
            script: script.as_ref(),
            client_env: &empty,
            ca_env: &empty,
            memo: &memo,
        };
        let mut draft = LaunchDraft {
            command: self.command,
            args: Vec::new(),
            cwd: PathBuf::from(LANE),
            env: self.preset_env,
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        WranglerPreset.apply(&mut draft, &cx).await?;
        Ok(draft)
    }
}

fn script_command() -> DraftCommand {
    DraftCommand::Script {
        source: PathBuf::from(LANE).join("apps/api/package.json"),
        name: "dev".to_owned(),
    }
}

/// `inspector` is requested for wrangler services only.
#[test]
fn adapter_required_ports() {
    let view = |preset| ServiceView {
        name: "api",
        platform: Some("node"),
        preset,
        tags: &[],
    };
    assert_eq!(
        WranglerPreset.required_ports(&view(Some("wrangler"))),
        ["inspector"]
    );
    assert!(
        WranglerPreset
            .required_ports(&view(Some("vite")))
            .is_empty()
    );
    assert!(WranglerPreset.required_ports(&view(None)).is_empty());
}

/// Ports, scalar vars and args land in the draft; the registry is per lane.
#[tokio::test]
#[cfg_attr(
    windows,
    ignore = "Unix path fixtures render with `\\` on Windows, ported in the Windows path step"
)]
async fn adapter_renders_flags_and_env() {
    let mut fake = Fake::new(script_command());
    fake.options = Some(serde_json::json!({
        "vars": { "PUBLIC_BASE_URL": "https://api.acme-1.localhost", "RETRIES": 3, "DEBUG": true },
        "args": ["--env-file", ".dev.vars"]
    }));
    fake.script_body = Some("wrangler dev --persist-to .state");
    let draft = fake.apply().await.expect("apply");
    insta::assert_snapshot!(draft.args.join(" "), @"--port 20010 --inspector-port 20011 --var DEBUG:true --var PUBLIC_BASE_URL:https://api.acme-1.localhost --var RETRIES:3 --env-file .dev.vars");
    assert_eq!(
        draft.env.get(REGISTRY_ENV).map(String::as_str),
        Some("/lanes/acme-1/.devlanes/wrangler-registry")
    );
}

/// A `run_script` body with `--port` fails the launch with the fix.
#[tokio::test]
async fn adapter_rejects_hardcoded_flags_in_script() {
    let mut fake = Fake::new(script_command());
    fake.script_body = Some("wrangler dev --port 8787");
    // `LaunchDraft` has no `Debug` (it may hold secrets), so no `expect_err`.
    let error = fake.apply().await.err().expect("hardcoded port");
    assert!(
        matches!(&error, Error::InvalidInput(message) if message.contains("`--port 8787`")),
        "{error}"
    );
}

/// Other presets, unknown keys and registry opt-out.
#[tokio::test]
async fn adapter_scope_and_options() {
    let shell = || DraftCommand::Shell("pnpm --silent -F @acme/api dev".to_owned());
    let mut other = Fake::new(shell());
    other.preset = Some("vite");
    assert!(other.apply().await.expect("apply").args.is_empty());

    let mut unknown = Fake::new(shell());
    unknown.options = Some(serde_json::json!({ "var": {} }));
    assert!(matches!(unknown.apply().await, Err(Error::InvalidInput(_))));

    let mut opt_out = Fake::new(shell());
    opt_out.options = Some(serde_json::json!({ "isolate_registry": false }));
    opt_out.preset_env = BTreeMap::from([(REGISTRY_ENV.to_owned(), "/set/by/platform".to_owned())]);
    let draft = opt_out.apply().await.expect("apply");
    assert!(!draft.env.contains_key(REGISTRY_ENV));

    let npm = Fake::new(DraftCommand::Shell("npm run dev".to_owned()));
    let draft = npm.apply().await.expect("apply");
    assert_eq!(draft.args.first().map(String::as_str), Some("--"));
}

/// The exported schema accepts scalar var values.
#[test]
fn schema_allows_scalar_vars() {
    let schema = wrangler::options_schema().to_value();
    assert_eq!(
        schema["properties"]["vars"]["additionalProperties"]["type"],
        serde_json::json!(["string", "number", "boolean"])
    );
}

/// The hardcoded-flag check works on the body the npm runner exposes.
#[test]
fn rejects_hardcoded_port_from_package_json() {
    let temp = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        temp.path().join("package.json"),
        r#"{"scripts":{"dev":"wrangler dev --port 8787"}}"#,
    )
    .expect("write package.json");
    let plan = plan(&ScriptRequest {
        worktree: temp.path(),
        source: Path::new("package.json"),
        name: "dev",
        extra_args: &[],
    })
    .expect("plan");
    let ports = BTreeMap::from([(INSPECTOR_PORT.to_owned(), 20011)]);
    let error = wrangler::contribution(&wrangler::WranglerInput {
        port: 20010,
        ports: &ports,
        lane_worktree: temp.path(),
        options: &wrangler::WranglerOptions::default(),
        script_body: Some(&plan.script_body),
    })
    .expect_err("hardcoded port");
    assert_eq!(
        error,
        WranglerError::HardcodedFlags {
            flags: vec!["`--port 8787`".to_owned()]
        }
    );
}

/// Renders `<pm> run dev <preset args>` for every package manager and preset.
#[test]
#[cfg_attr(
    windows,
    ignore = "Unix path fixtures render with `\\` on Windows, ported in the Windows path step"
)]
fn preset_arg_rendering_matrix() {
    let lane = Path::new(LANE);
    let ports = BTreeMap::from([(INSPECTOR_PORT.to_owned(), 20011)]);
    let wrangler_options = wrangler::WranglerOptions {
        vars: BTreeMap::from([(
            "PUBLIC_BASE_URL".to_owned(),
            "https://api.acme-1.localhost".to_owned(),
        )]),
        args: Some(vec!["--env-file".into(), ".dev.vars".into()]),
        isolate_registry: None,
    };
    let tauri_options = tauri::TauriOptions {
        identifier: Some("com.acme.app.dev.lane1.client1".to_owned()),
        dev_url: Some("http://127.0.0.1:20014".to_owned()),
        ..tauri::TauriOptions::default()
    };
    let overlay = tauri::overlay_path(lane, "desktop", 1);
    let presets: Vec<(&str, Vec<String>)> = vec![
        (
            "vite",
            vite::contribution(20012, &vite::ViteOptions::default()).args,
        ),
        (
            "next",
            next::contribution(20013, &next::NextOptions::default()).args,
        ),
        (
            "wrangler",
            wrangler::contribution(&wrangler::WranglerInput {
                port: 20010,
                ports: &ports,
                lane_worktree: lane,
                options: &wrangler_options,
                script_body: Some("wrangler dev"),
            })
            .expect("wrangler contribution")
            .args,
        ),
        (
            "tauri",
            tauri::contribution(&tauri::TauriInput {
                options: &tauri_options,
                overlay_path: &overlay,
            })
            .expect("tauri contribution")
            .contribution
            .args,
        ),
    ];
    let managers = [
        PackageManager::Npm,
        PackageManager::Pnpm,
        PackageManager::YarnClassic,
        PackageManager::YarnBerry,
        PackageManager::Bun,
    ];
    let mut rendered = String::new();
    for manager in managers {
        for (preset, args) in &presets {
            rendered.push_str(&format!(
                "{manager:?} x {preset}: {}\n",
                manager.run_argv("dev", args).join(" ")
            ));
        }
    }
    insta::assert_snapshot!(rendered);
}

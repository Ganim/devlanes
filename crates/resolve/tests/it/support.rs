//! Fakes and fixtures shared by the resolution tests.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use devlanes_config::ProjectConfig;
use devlanes_core::{
    Error, StopSignal,
    contrib::{
        ContribCx, Contributor, DETECT_MANIFEST, DETECT_SCRIPT_SOURCE, DetectCx, LaunchDraft,
        ScriptPlan, ScriptRequest, ScriptRunner, SecretEnv, SecretsError, SecretsProvider,
        SecretsRequest, ServiceView,
    },
    ports::{PortBlock, PortName, allocate_offsets, block_size},
};
use devlanes_resolve::{LaneFacts, Launch, ResolvedLane, port_needs};
use serde_json::{Value, json};

pub const FULL: &str = include_str!("../../../../docs/examples/devlanes.yaml");
pub const MINIMAL: &str = include_str!("../../../../docs/examples/minimal.yaml");

/// Parses and validates a project config.
pub fn parse(yaml: &str) -> ProjectConfig {
    devlanes_config::parse_project(yaml, None).expect("valid config")
}

/// Lane facts under a fixed fake checkout.
pub fn facts(project: &str, index: u32, branch: Option<&str>) -> LaneFacts {
    let path = if index == 0 {
        format!("/work/{project}")
    } else {
        format!("/work/{project}-lanes/lane-{index}")
    };
    LaneFacts::new(
        project,
        format!("/work/{project}"),
        index,
        path,
        format!("/home/dev/.devlanes/data/{project}/{}", index + 10),
        branch.map(str::to_owned),
    )
}

/// Allocates a block and offsets the way the daemon would, from a fixed start.
pub fn allocation(
    config: &ProjectConfig,
    start: u16,
    contributors: &[Arc<dyn Contributor>],
) -> (PortBlock, BTreeMap<PortName, u16>) {
    let needs = port_needs(config, contributors).expect("port needs");
    let size = block_size(needs.min_block_size(), None).expect("block size");
    let block = PortBlock::new(start, size).expect("block");
    let offsets = allocate_offsets(&BTreeMap::new(), &needs.names, &needs.reserved, &[block])
        .expect("offsets");
    (block, offsets)
}

/// Client environment used by most tests.
pub fn client_env() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("HOME".to_owned(), "/home/dev".to_owned()),
        ("PATH".to_owned(), "/usr/bin:/bin".to_owned()),
    ])
}

/// Infra endpoints for the reference example.
pub fn infra() -> Value {
    json!({
        "postgres": {"port": 15432, "ports": {"5432": 15432}},
        "redis": {"port": 16379, "ports": {"6379": 16379}},
        "rabbitmq": {"port": 15672, "ports": {"5672": 15672}},
        "s3mock": {"port": 19090, "ports": {"9090": 19090}},
        "cerbos": {"port": 21050, "ports": {"http": 21050, "grpc": 21051}},
    })
}

/// Contributor that sets fixed variables and, optionally, preset-style flags.
pub struct FakeContributor {
    pub id: &'static str,
    pub priority: i32,
    pub env: Vec<(&'static str, &'static str)>,
    /// Port names required for services using this preset.
    pub preset_ports: Option<(&'static str, &'static str)>,
    /// Append `--port <self.port>` for services with this preset.
    pub port_flag_for: Option<&'static str>,
    /// Platform claimed through detection.
    pub platform: Option<&'static str>,
    /// Detection rules, checked in order; the first match gives the confidence.
    pub probes: Vec<(Probe, u8)>,
}

/// One detection rule of a [`FakeContributor`].
#[derive(Debug, Clone, Copy)]
pub enum Probe {
    /// The `run_script` source has this file name.
    Script(&'static str),
    /// The working directory contains this file.
    Cwd(&'static str),
}

impl FakeContributor {
    /// Contributor that only sets environment variables.
    pub fn env(id: &'static str, priority: i32, env: Vec<(&'static str, &'static str)>) -> Self {
        Self {
            id,
            priority,
            env,
            preset_ports: None,
            port_flag_for: None,
            platform: None,
            probes: Vec::new(),
        }
    }

    /// Contributor that also claims `platform` with `probes`.
    pub fn platform(
        id: &'static str,
        priority: i32,
        platform: &'static str,
        probes: Vec<(Probe, u8)>,
    ) -> Self {
        Self {
            platform: Some(platform),
            probes,
            ..Self::env(id, priority, Vec::new())
        }
    }

    /// Detection the real node contributor uses: `package.json` as script source
    /// or in the working directory.
    pub fn node_detection() -> Vec<(Probe, u8)> {
        vec![
            (Probe::Script("package.json"), DETECT_SCRIPT_SOURCE),
            (Probe::Cwd("package.json"), DETECT_MANIFEST),
        ]
    }
}

#[async_trait]
impl Contributor for FakeContributor {
    fn id(&self) -> &'static str {
        self.id
    }

    fn priority(&self) -> i32 {
        self.priority
    }

    fn required_ports(&self, service: &ServiceView<'_>) -> Vec<String> {
        match self.preset_ports {
            Some((preset, port)) if service.preset == Some(preset) => vec![port.to_owned()],
            _ => Vec::new(),
        }
    }

    fn platform(&self) -> Option<&'static str> {
        self.platform
    }

    async fn detect(&self, cx: &DetectCx<'_>) -> Option<u8> {
        for (probe, confidence) in &self.probes {
            let hit = match probe {
                Probe::Script(file) => cx
                    .script
                    .and_then(|s| s.source.file_name())
                    .is_some_and(|name| name == *file),
                Probe::Cwd(file) => tokio::fs::try_exists(cx.cwd.join(file))
                    .await
                    .unwrap_or(false),
            };
            if hit {
                return Some(*confidence);
            }
        }
        None
    }

    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        for (key, value) in &self.env {
            draft.env.insert((*key).to_owned(), (*value).to_owned());
        }
        if let (Some(preset), Some(port)) = (self.port_flag_for, cx.port)
            && cx.preset == Some(preset)
        {
            draft.args.extend(["--port".to_owned(), port.to_string()]);
        }
        Ok(())
    }
}

/// Script runner for package.json, Makefile and justfile that reads no files.
pub struct FakeRunner {
    pub calls: Mutex<Vec<(String, String, Vec<String>)>>,
}

impl FakeRunner {
    /// A runner with an empty call log.
    pub fn new() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl ScriptRunner for FakeRunner {
    fn id(&self) -> &'static str {
        "fake"
    }

    fn matches(&self, source: &Path) -> bool {
        source
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| ["package.json", "Makefile", "justfile"].contains(&n))
    }

    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let file = request.worktree.join(request.source);
        let program = match file.file_name().and_then(|n| n.to_str()) {
            Some("package.json") => vec!["pnpm".to_owned(), "run".to_owned()],
            Some("Makefile") => vec!["make".to_owned()],
            _ => vec!["just".to_owned()],
        };
        self.calls.lock().expect("lock").push((
            file.display().to_string(),
            request.name.to_owned(),
            request.extra_args.to_vec(),
        ));
        let mut argv = program;
        argv.push(request.name.to_owned());
        argv.extend(request.extra_args.iter().cloned());
        Ok(ScriptPlan {
            argv,
            cwd: file.parent().map(Path::to_path_buf).unwrap_or_default(),
            body: Some(format!("echo {}", request.name)),
            notes: Vec::new(),
            detail: None,
        })
    }
}

/// Secrets provider with fixed values per profile.
#[derive(Default)]
pub struct FakeSecrets {
    pub values: BTreeMap<Option<String>, BTreeMap<String, String>>,
    pub remove: BTreeSet<String>,
    pub locked: BTreeSet<Option<String>>,
    pub calls: Mutex<Vec<(Option<String>, bool)>>,
}

#[async_trait]
impl SecretsProvider for FakeSecrets {
    fn id(&self) -> &'static str {
        "fake-secrets"
    }

    async fn env(&self, request: &SecretsRequest<'_>) -> Result<SecretEnv, SecretsError> {
        let profile = request.profile.map(str::to_owned);
        self.calls
            .lock()
            .expect("lock")
            .push((profile.clone(), request.explicit));
        if self.locked.contains(&profile) {
            return Err(SecretsError::Locked {
                message: "1password needs a sign-in".into(),
            });
        }
        Ok(SecretEnv {
            set: self.values.get(&profile).cloned().unwrap_or_default(),
            remove: self.remove.clone(),
            files: BTreeMap::new(),
        })
    }
}

/// Snapshot view of a launch: values are fake test data, so they are printed.
pub fn launch_view(launch: &Launch) -> Value {
    let env: serde_json::Map<String, Value> = launch
        .env
        .values
        .iter()
        .map(|(key, value)| {
            let value = if key == "DEVLANES_ORIGINS" {
                "<lane origins>".to_owned()
            } else {
                value.clone()
            };
            (
                key.clone(),
                json!(format!("{:?}: {value}", launch.env.sources[key])),
            )
        })
        .collect();
    json!({
        "program": launch.program,
        "args": launch.args,
        "cwd": launch.cwd,
        "platform": launch.platform,
        "script": launch.script.as_ref().map(|s| json!({
            "source": s.source, "name": s.name, "runner": s.runner, "cwd": s.cwd,
        })),
        "contributors": launch.contributors,
        "notes": snapshot_notes(&launch.notes),
        "files": launch.files.iter().map(|f| f.path.display().to_string()).collect::<Vec<_>>(),
        "env": env,
    })
}

/// The Linux signal number the snapshots show for a stop signal, the form the
/// policy had before it became an enum.
fn signal_number(signal: StopSignal) -> i32 {
    match signal {
        StopSignal::Hangup => 1,
        StopSignal::Interrupt => 2,
        StopSignal::Quit => 3,
        StopSignal::Kill => 9,
        StopSignal::User1 => 10,
        StopSignal::User2 => 12,
        StopSignal::Terminate => 15,
    }
}

/// Snapshot view of a whole lane.
pub fn lane_view(lane: &ResolvedLane) -> Value {
    let services: serde_json::Map<String, Value> = lane
        .services
        .iter()
        .map(|(unit, s)| {
            (
                unit.clone(),
                json!({
                    "replica": format!("{}/{}", s.replica, s.replicas),
                    "port": s.port,
                    "ports": s.ports,
                    "url": s.url,
                    "public_url": s.public_url,
                    "hosts": s.hosts,
                    "ready": format!("{:?} within {:?}", s.ready.check, s.ready.timeout),
                    "stop": format!("signal {} after {:?}", signal_number(s.stop.signal), s.stop.timeout),
                    "pty": s.pty,
                    "autostart": s.autostart,
                    "hidden": s.hidden,
                    "tags": s.tags,
                    "literal_port": s.literal_port,
                    "sync": s.sync.iter().map(|s| format!("{:?}", s)).collect::<Vec<_>>(),
                    "launch": launch_view(&s.launch),
                }),
            )
        })
        .collect();
    let tasks: serde_json::Map<String, Value> = lane
        .tasks
        .iter()
        .map(|(name, t)| {
            (
                name.clone(),
                json!({
                    "when": t.when.map(|w| format!("{w:?}")),
                    "after_services": t.after_services,
                    "profiles": t.profiles,
                    "active": t.active,
                    "steps": t.steps,
                    "confirm": t.confirm,
                    "interactive": t.interactive,
                    "timeout": t.timeout.map(|d| format!("{d:?}")),
                    "pty": t.pty,
                    "launch": t.launch.as_ref().map(launch_view),
                }),
            )
        })
        .collect();
    let commands = |map: &indexmap::IndexMap<String, devlanes_resolve::ResolvedCommand>| -> Value {
        map.iter()
            .map(|(name, c)| {
                (
                    name.clone(),
                    json!({
                        "container": c.container,
                        "ignore_errors": c.ignore_errors,
                        "launch": launch_view(&c.launch),
                    }),
                )
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let view = json!({
        "lane": {"index": lane.facts.index, "name": lane.facts.name, "label": lane.facts.label},
        "domain": lane.domain,
        "port": lane.port,
        "vars": lane.vars,
        "profile": lane.profile,
        "infra_enabled": lane.infra_enabled,
        "routes": lane.routes.iter().map(|r| format!(
            "{} -> {}:{}{}{}", r.host, r.unit, r.port,
            if r.fallback { " (fallback)" } else { "" },
            if r.host_header_upstream { " (upstream host)" } else { "" },
        )).collect::<Vec<_>>(),
        "origins": lane.origins,
        "lane_env": lane.lane_env(),
        "errors": lane.errors.iter().map(|(k, e)| (k.clone(), Value::from(e.to_string()))).collect::<serde_json::Map<_, _>>(),
        "warnings": lane.warnings,
        "services": services,
        "tasks": tasks,
        "setup": commands(&lane.setup),
        "teardown": commands(&lane.teardown),
    });
    host_neutral(view)
}

/// The view as is: the snapshots are Unix output.
#[cfg(unix)]
fn host_neutral(view: Value) -> Value {
    view
}

/// The view with `/` for `\` in every string, so Windows compares against the
/// Unix snapshots: paths joined below a `/`-spelled fixture root get `\`
/// separators there (escaped as `\\` inside `Debug` text).
#[cfg(windows)]
fn host_neutral(view: Value) -> Value {
    match view {
        Value::String(text) => Value::String(text.replace("\\\\", "/").replace('\\', "/")),
        Value::Array(items) => Value::Array(items.into_iter().map(host_neutral).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, host_neutral(value)))
                .collect(),
        ),
        other => other,
    }
}

/// Launch notes the snapshots show: all of them on Unix.
#[cfg(unix)]
fn snapshot_notes(notes: &[String]) -> Vec<String> {
    notes.to_vec()
}

/// Launch notes the snapshots show: Windows-only notes (`on Windows ...`) are
/// left out so the Unix snapshots apply; tests assert them directly.
#[cfg(windows)]
fn snapshot_notes(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .filter(|note| !note.starts_with("on Windows "))
        .cloned()
        .collect()
}

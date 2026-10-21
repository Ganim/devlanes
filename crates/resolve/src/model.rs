use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc, time::Duration};

use devlanes_config::{Extensions, OpaqueExtensions, ProjectConfig, Scalar, TaskWhen};
use devlanes_core::{
    LaunchSpec, StopPolicy, TerminalSize,
    contrib::{Contributor, PreparedFile, ScriptInfo, ScriptRunner, SecretsProvider},
    env::Environment,
    ports::{PortBlock, PortName},
};
use indexmap::IndexMap;
use serde_json::Value;

use crate::{LaneFacts, ResolveError};

/// Readiness timeout when the config sets none.
pub const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(60);

static EMPTY_ENV: BTreeMap<String, String> = BTreeMap::new();
/// Stands for an empty namespace; layout code treats `null` as `{}`.
static EMPTY_NAMESPACE: Value = Value::Null;

/// Everything [`crate::resolve_lane`] needs for one lane. Construct with
/// [`ResolveInput::new`] and set the fields that apply.
pub struct ResolveInput<'a, E: Extensions = OpaqueExtensions> {
    pub config: &'a ProjectConfig<E>,
    pub facts: &'a LaneFacts,
    /// The lane's block from the machine-wide pool.
    pub block: PortBlock,
    /// Project-wide offsets for every name in [`crate::PortNeeds::names`].
    pub offsets: &'a BTreeMap<PortName, u16>,
    /// Environment of the client that started the lane: the lowest layer, and
    /// the first source of `${{ env.X }}`.
    pub client_env: &'a BTreeMap<String, String>,
    /// fnox or another provider; called once per distinct profile.
    pub secrets: Option<&'a dyn SecretsProvider>,
    /// The lane's `.env` (or the primary's). Feeds `${{ env.X }}` only; never injected.
    pub dotenv: &'a BTreeMap<String, String>,
    /// `infra` namespace: `{name: {port, ports: {...}}}`.
    pub infra: &'a Value,
    /// `lanes.main.svc` namespace from lane 0's [`LaneLayout::svc_namespace`].
    /// Lane 0 itself may leave it unset; its own services are used.
    pub lanes_main: Option<&'a Value>,
    /// `projects` namespace: `{project: {svc: {...}}}` for other running projects.
    pub projects: &'a Value,
    /// Requested profile from single-profile callers; counts as the first entry
    /// of `profiles`. With neither set, `default` applies when the config defines it.
    pub profile: Option<&'a str>,
    /// Requested profiles in order; later profile `env` values win.
    pub profiles: &'a [String],
    /// `--env K=V` values, the highest layer.
    pub overrides: &'a BTreeMap<String, String>,
    pub contributors: &'a [Arc<dyn Contributor>],
    pub script_runners: &'a [Arc<dyn ScriptRunner>],
    /// Trust variables from the CA, injected with the `DEVLANES_*` layer.
    pub ca_env: &'a BTreeMap<String, String>,
    /// Port the proxy's HTTPS listener is bound to. Public URLs and origins add
    /// `:<port>` unless it is 443; `None` renders them without a port.
    pub public_https_port: Option<u16>,
}

impl<'a, E: Extensions> ResolveInput<'a, E> {
    /// Input with empty environments, namespaces and integrations.
    pub fn new(
        config: &'a ProjectConfig<E>,
        facts: &'a LaneFacts,
        block: PortBlock,
        offsets: &'a BTreeMap<PortName, u16>,
    ) -> Self {
        Self {
            config,
            facts,
            block,
            offsets,
            client_env: &EMPTY_ENV,
            secrets: None,
            dotenv: &EMPTY_ENV,
            infra: &EMPTY_NAMESPACE,
            lanes_main: None,
            projects: &EMPTY_NAMESPACE,
            profile: None,
            profiles: &[],
            overrides: &EMPTY_ENV,
            contributors: &[],
            script_runners: &[],
            ca_env: &EMPTY_ENV,
            public_https_port: None,
        }
    }
}

/// Ports, URLs and routes of a lane, without launch plans. Cheap and synchronous:
/// no contributors, no secrets.
#[derive(Debug, Clone)]
pub struct LaneLayout {
    pub domain: String,
    /// `lane.port`.
    pub port: u16,
    /// Evaluated `lanes.vars` (or the lane's `matrix` row), in declaration order.
    pub vars: IndexMap<String, Scalar>,
    /// Keyed by unit: the service name, or `<service>.<N>` for replicas.
    pub services: IndexMap<String, ServiceLayout>,
    pub routes: Vec<RouteSpec>,
    pub origins: Vec<String>,
    pub warnings: Vec<String>,
    pub(crate) lane_env: BTreeMap<String, String>,
}

impl LaneLayout {
    /// The `svc` namespace of this lane (replica 1 of each service), for use as
    /// another lane's `lanes_main` when this is lane 0.
    pub fn svc_namespace(&self) -> Value {
        svc_namespace(self.services.values())
    }

    /// Variables for `devlanes with` and `devlanes env show`: `DEVLANES`,
    /// `DEVLANES_PROJECT`, `DEVLANES_LANE`, `DEVLANES_LANE_NAME`,
    /// `DEVLANES_PUBLIC_URL_<SERVICE>` for every service and the CA variables.
    pub fn lane_env(&self) -> &BTreeMap<String, String> {
        &self.lane_env
    }
}

/// Builds `{service: {name, port, ports, url, public_url, hosts}}` from replica 1.
pub(crate) fn svc_namespace<'a>(services: impl Iterator<Item = &'a ServiceLayout>) -> Value {
    Value::Object(
        services
            .filter(|s| s.replica == 1)
            .map(|s| (s.name.clone(), s.self_value()))
            .collect(),
    )
}

/// Ports and hostnames of one service replica.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceLayout {
    /// Service name in the config.
    pub name: String,
    /// Unit key: `name`, or `name.<replica>` when `replicas > 1`.
    pub unit: String,
    /// One-based.
    pub replica: u16,
    pub replicas: u16,
    /// `self.port`.
    pub port: u16,
    /// `self.ports`: named ports, without the primary one.
    pub ports: BTreeMap<String, u16>,
    /// `http://127.0.0.1:<port>`.
    pub url: String,
    /// `https://<first non-wildcard host>[:<public_https_port>]`.
    pub public_url: Option<String>,
    pub hosts: Vec<String>,
    /// `env.PORT` is a literal: the same port in every lane.
    pub literal_port: bool,
    pub hidden: bool,
    pub autostart: bool,
    pub tags: Vec<String>,
    /// Palette name from [`devlanes_core::color::unit_color`].
    pub color: String,
}

impl ServiceLayout {
    /// The value of `self` (and of `svc.<name>`) in expressions. `public_url` is
    /// absent without hosts, so using it fails unless filtered with `default`.
    pub fn self_value(&self) -> Value {
        let mut map = serde_json::Map::new();
        map.insert("name".into(), self.name.clone().into());
        map.insert("port".into(), self.port.into());
        map.insert(
            "ports".into(),
            Value::Object(
                self.ports
                    .iter()
                    .map(|(k, v)| (k.clone(), Value::from(*v)))
                    .collect(),
            ),
        );
        map.insert("url".into(), self.url.clone().into());
        if let Some(url) = &self.public_url {
            map.insert("public_url".into(), url.clone().into());
        }
        map.insert(
            "hosts".into(),
            Value::Array(self.hosts.iter().cloned().map(Value::from).collect()),
        );
        Value::Object(map)
    }
}

/// One proxy route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSpec {
    /// Exact host or `*.` wildcard.
    pub host: String,
    pub service: String,
    pub unit: String,
    /// Upstream port on 127.0.0.1 (`self.port` or a named port).
    pub port: u16,
    /// Which of the unit's ports `port` is: `default` or the route's named port.
    pub port_name: String,
    /// Used only when no other host matches.
    pub fallback: bool,
    /// Send `Host: 127.0.0.1:<port>` upstream instead of the browser's Host.
    pub host_header_upstream: bool,
}

/// A planned process. `Debug` lists env keys and layers only, never values or
/// arguments, since both can carry secrets.
pub struct Launch {
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Environment,
    /// Files the daemon writes before spawning.
    pub files: Vec<PreparedFile>,
    /// Non-fatal diagnostics from contributors and providers.
    pub notes: Vec<String>,
    /// Explicit or inferred platform.
    pub platform: Option<String>,
    pub script: Option<ScriptInfo>,
    /// Contributors applied, in order.
    pub contributors: Vec<&'static str>,
}

impl Launch {
    /// Builds the runner input. Clones the environment values.
    pub fn to_spec(&self, pty: bool, size: TerminalSize) -> LaunchSpec {
        LaunchSpec {
            program: self.program.clone(),
            args: self.args.clone(),
            cwd: self.cwd.clone(),
            env: self.env.values.clone(),
            pty,
            size,
        }
    }
}

impl fmt::Debug for Launch {
    /// Prints only non-secret structure.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Launch")
            .field("program", &self.program)
            .field("args", &format_args!("[{} args]", self.args.len()))
            .field("cwd", &self.cwd)
            .field("env", &self.env.sources)
            .field(
                "files",
                &self.files.iter().map(|f| &f.path).collect::<Vec<_>>(),
            )
            .field("platform", &self.platform)
            .field("contributors", &self.contributors)
            .finish_non_exhaustive()
    }
}

/// What "ready" means for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadySpec {
    /// Running is enough.
    Started,
    Tcp {
        port: u16,
    },
    Http {
        port: u16,
        path: String,
        status: Option<u16>,
    },
    /// Regex over ANSI-stripped output lines.
    Log {
        pattern: String,
    },
    /// Shell command retried until it exits 0.
    Cmd {
        shell: String,
        cwd: PathBuf,
    },
}

/// Readiness check plus its timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    pub check: ReadySpec,
    pub timeout: Duration,
}

/// `services.<name>.sync` entry with absolute paths.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncSpec {
    pub from: PathBuf,
    pub to: PathBuf,
    pub ignore: Vec<String>,
}

/// One service replica, ready to spawn.
#[derive(Debug)]
pub struct ResolvedService {
    pub name: String,
    pub unit: String,
    pub replica: u16,
    pub replicas: u16,
    pub launch: Launch,
    pub port: u16,
    pub ports: BTreeMap<String, u16>,
    pub url: String,
    pub public_url: Option<String>,
    pub hosts: Vec<String>,
    pub ready: Ready,
    pub stop: StopPolicy,
    pub pty: bool,
    pub autostart: bool,
    pub hidden: bool,
    pub tags: Vec<String>,
    /// Palette name from [`devlanes_core::color::unit_color`].
    pub color: String,
    pub sync: Vec<SyncSpec>,
    /// Same port in every lane; refuse to start while another lane runs it.
    pub literal_port: bool,
    pub preset: Option<String>,
}

/// One task. Steps-only tasks have no launch.
#[derive(Debug)]
pub struct ResolvedTask {
    pub name: String,
    pub launch: Option<Launch>,
    pub steps: Vec<String>,
    pub when: Option<TaskWhen>,
    /// `after: services`.
    pub after_services: bool,
    pub profiles: Option<Vec<String>>,
    /// Whether the `when` hook runs: `profiles` is unset or names any profile in effect.
    pub active: bool,
    pub confirm: Option<String>,
    pub interactive: bool,
    pub timeout: Option<Duration>,
    pub pty: bool,
    /// Palette name from [`devlanes_core::color::unit_color`]; tasks have no
    /// tags, so it follows the name.
    pub color: String,
}

/// One `infra.setup` or `infra.teardown` entry.
#[derive(Debug)]
pub struct ResolvedCommand {
    pub name: String,
    pub launch: Launch,
    /// Re-run whenever this container (re)starts.
    pub container: Option<String>,
    pub ignore_errors: bool,
    pub timeout: Option<Duration>,
}

/// A fully planned lane.
#[derive(Debug)]
pub struct ResolvedLane {
    pub facts: LaneFacts,
    pub domain: String,
    pub port: u16,
    pub vars: IndexMap<String, Scalar>,
    /// First profile in effect (`default` when defined and none was requested).
    pub profile: Option<String>,
    /// Every profile in effect, in the order given.
    pub profiles: Vec<String>,
    /// False only when every profile in effect evaluates `infra` to false.
    pub infra_enabled: bool,
    /// Every service replica that planned successfully, selected or not, keyed by unit.
    pub services: IndexMap<String, ResolvedService>,
    /// Declaration order.
    pub tasks: IndexMap<String, ResolvedTask>,
    pub setup: IndexMap<String, ResolvedCommand>,
    pub teardown: IndexMap<String, ResolvedCommand>,
    pub routes: Vec<RouteSpec>,
    pub origins: Vec<String>,
    /// Planning failures keyed by config path (`services.<name>`, `tasks.<name>`,
    /// `infra.setup.<name>`, `infra.teardown.<name>`). A failure fails only that
    /// unit, so an unrelated broken service does not block a start.
    pub errors: IndexMap<String, ResolveError>,
    pub warnings: Vec<String>,
    pub(crate) lane_env: BTreeMap<String, String>,
    pub(crate) svc: Value,
}

impl ResolvedLane {
    /// See [`LaneLayout::lane_env`].
    pub fn lane_env(&self) -> &BTreeMap<String, String> {
        &self.lane_env
    }

    /// See [`LaneLayout::svc_namespace`].
    pub fn svc_namespace(&self) -> &Value {
        &self.svc
    }

    /// Replicas of `service` in replica order.
    pub fn units_of<'s>(&'s self, service: &'s str) -> impl Iterator<Item = &'s ResolvedService> {
        self.services.values().filter(move |s| s.name == service)
    }

    /// Fails with the first planning error among `services`, so a start can
    /// report it (a locked vault included) before spawning anything.
    pub fn check_services<'s>(
        &self,
        services: impl IntoIterator<Item = &'s str>,
    ) -> Result<(), ResolveError> {
        for service in services {
            if let Some(error) = self.errors.get(&format!("services.{service}")) {
                return Err(error.clone());
            }
        }
        Ok(())
    }
}

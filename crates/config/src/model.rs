use crate::{ConfigDuration, Extensions, OpaqueExtensions, Scalar, ServiceConfig, Templated};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type Env = IndexMap<String, Templated<Scalar>>;

/// Fully merged project configuration. Map order is declaration order.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, bound = "E: Extensions")]
#[schemars(extend("examples" = [{"project":"acme","services":{"api":{"run_script":"apps/api/package.json#dev"}}}]))]
pub struct ProjectConfig<E: Extensions = OpaqueExtensions> {
    /// Source documents retained in memory for diagnostics after integration validation.
    #[serde(skip)]
    #[schemars(skip)]
    pub sources: crate::ConfigSources,
    /// Project identity shared by every worktree; lowercase letters, digits, dots, underscores, and dashes.
    #[schemars(extend("examples" = ["acme"]))]
    pub project: String,
    /// Tag settings. Units are colored by their first tag; colors are inferred from common tag names (api, web, worker, db, ...) when not set here.
    #[serde(default)]
    #[schemars(extend("examples" = [{"api":{"color":"cyan","description":"HTTP APIs"}}]))]
    pub tags: IndexMap<String, crate::TagConfig>,
    /// Reusable service settings, inherited with extends after local overrides are merged.
    #[serde(default)]
    #[schemars(extend("examples" = [{"fastify-api":{"platform":"node","tags":["api"],"ready":{"http":{"path":"/-/health"}}}}]))]
    pub templates: IndexMap<String, ServiceConfig<E>>,
    /// Worktree registration, per-lane variables, and port-block policy.
    #[serde(default)]
    pub lanes: LanesConfig,
    /// Shared and per-lane containers, setup commands, and teardown commands.
    #[serde(default)]
    pub infra: InfraConfig,
    /// Project-wide fnox profile for secrets resolved by the runtime.
    pub fnox: Option<FnoxConfig>,
    /// Environment overrides for tasks and services, after derived DEVLANES values.
    #[serde(default)]
    #[schemars(extend("examples" = [{"API_URL":"${{ svc.api.url }}"}]))]
    pub global_env: Env,
    /// One-shot commands; lifecycle hooks execute in declaration order.
    #[serde(default)]
    #[schemars(extend("examples" = [{"migrate":{"run_script":"apps/api/package.json#migrate","when":"lane.start"}}]))]
    pub tasks: IndexMap<String, TaskConfig>,
    /// Long-running processes started together after infrastructure and before-service tasks.
    #[serde(default)]
    #[schemars(extend("examples" = [{"api":{"run_script":"apps/api/package.json#dev","hosts":["api.${{ lane.domain }}"]}}]))]
    pub services: IndexMap<String, ServiceConfig<E>>,
    /// Named service selectors with optional environment and infrastructure overrides.
    #[serde(default)]
    #[schemars(extend("examples" = [{"default":["+api","+web"],"e2e":{"include":["*"],"run_scripts":{"dev":"start"}}}]))]
    pub profiles: IndexMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"primary":"main","hosts_file":"always","vars":{"db_name":"acme_${{ lane.name }}"}}]))]
/// Worktree registration policy and values computed for each lane.
pub struct LanesConfig {
    /// Branch assigned lane zero; defaults to the remote default branch, main, or master.
    #[schemars(extend("examples" = ["main"]))]
    pub primary: Option<String>,
    /// Action for newly discovered worktrees; defaults to register.
    pub new_worktrees: Option<NewWorktrees>,
    /// Stop processes or also clean lane data when a worktree disappears.
    pub removed_worktrees: Option<RemovedWorktrees>,
    /// Directory for newly created worktrees, relative to the project.
    #[schemars(extend("examples" = ["../${{ project.name }}-lanes"]))]
    pub worktree_dir: Option<Templated<String>>,
    /// Fixed port-block capacity; otherwise allocated with fifty percent headroom.
    #[schemars(extend("examples" = [80]))]
    pub block_size: Option<Templated<u16>>,
    /// Maximum assigned lanes; existing lanes are retained if this is lowered.
    #[schemars(extend("examples" = [6]))]
    pub max: Option<Templated<u32>>,
    /// Project-specific host-file hint, overriding the user's policy.
    pub hosts_file: Option<HostsFile>,
    /// Scalar lane values available through the lane expression namespace.
    #[serde(default)]
    #[schemars(extend("examples" = [{"db_name":"acme_${{ lane.name }}"}]))]
    pub vars: Env,
    /// Explicit lane values; entry N describes lane N. Mutually exclusive with vars.
    #[schemars(extend("examples" = [[{"port":20000,"domain":"acme.localhost"},{"port":20100,"domain":"acme-1.localhost"}]]))]
    pub matrix: Option<Vec<Env>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["register","setup","start"]))]
/// Action when a new worktree appears; defaults to register.
pub enum NewWorktrees {
    /// Reserve a lane without running setup or services.
    #[schemars(extend("examples" = ["register"]))]
    Register,
    /// Run infrastructure setup and lane.create tasks, leaving services stopped.
    #[schemars(extend("examples" = ["setup"]))]
    Setup,
    /// Start the worktree with the default profile.
    #[schemars(extend("examples" = ["start"]))]
    Start,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["stop","clean"]))]
/// Action when a worktree disappears; defaults to stop and retain its data.
pub enum RemovedWorktrees {
    /// Stop the lane's processes but keep its data.
    #[schemars(extend("examples" = ["stop"]))]
    Stop,
    /// Stop processes, run teardown, and free the lane.
    #[schemars(extend("examples" = ["clean"]))]
    Clean,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["auto","never","always"]))]
/// Whether project hostnames are also written to the system hosts file.
pub enum HostsFile {
    /// Write entries when the system cannot resolve the configured hostnames.
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// Do not write hosts entries.
    #[schemars(extend("examples" = ["never"]))]
    Never,
    /// Always write exact hostnames, including for runtimes without localhost subdomain support.
    #[schemars(extend("examples" = ["always"]))]
    Always,
}

/// Shell text runs in a shell; argv is passed directly to the executable.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["pnpm dev",["node","server.mjs","--port","${{ self.port }}"]]))]
pub enum Run {
    /// Shell command text; supports pipelines and multiline scripts.
    #[schemars(extend("examples" = ["pnpm dev"]))]
    Shell(Templated<String>),
    /// Executable and arguments passed directly without shell expansion.
    #[schemars(extend("examples" = [["node","server.mjs"]]))]
    Argv(Vec<Templated<String>>),
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["apps/api/package.json#dev",{"source":"justfile","name":"fixtures"}]))]
/// Script selected from a package manifest, Makefile, or justfile.
pub enum RunScript {
    /// Source file followed by # and the script name.
    #[schemars(extend("examples" = ["apps/api/package.json#dev"]))]
    Short(String),
    /// Source file and script name written as separate fields.
    #[schemars(extend("examples" = [{"source":"justfile","name":"fixtures"}]))]
    Object(ScriptSource),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"source":"justfile","name":"fixtures"}]))]
/// A source file and the script or target to run from it.
pub struct ScriptSource {
    /// Script source relative to the worktree, such as package.json, Makefile, or justfile.
    /// Required, unless an override or extends child keeps the inherited source.
    #[schemars(extend("examples" = ["apps/api/package.json","Makefile","justfile"]))]
    #[serde(default)]
    pub source: String,
    #[schemars(extend("examples" = ["dev","fixtures"]))]
    /// Script, target, or recipe name within the source file.
    /// Required, unless an override or extends child keeps the inherited name.
    #[serde(default)]
    pub name: String,
}

impl RunScript {
    /// Resolve either syntax into a file and script name, rejecting malformed shorthand.
    pub fn source_and_name(&self) -> Result<(&str, &str), crate::ConfigError> {
        let parts = match self {
            Self::Short(value) => value.rsplit_once('#'),
            Self::Object(value) => Some((value.source.as_str(), value.name.as_str())),
        };
        parts
            .filter(|(source, name)| !source.is_empty() && !name.is_empty())
            .ok_or_else(|| {
                crate::ConfigError::plain("run_script requires `<file>#<name>` or { source, name }")
            })
    }

    /// Remap only the script name for a selected profile, preserving its source file.
    pub fn source_and_name_for_profile<'a>(
        &'a self,
        profile: Option<&'a ProfileOptions>,
    ) -> Result<(&'a str, &'a str), crate::ConfigError> {
        let (source, name) = self.source_and_name()?;
        Ok((
            source,
            profile.map_or(name, |profile| profile.remap_script(name)),
        ))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"profile":"dev"}]))]
/// Secrets profile selection for the project or a single unit.
pub struct FnoxConfig {
    /// Secrets profile passed to fnox; uses fnox's default profile when omitted.
    #[schemars(extend("examples" = ["dev","admin-api"]))]
    pub profile: Option<Templated<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"run_script":"apps/api/package.json#migrate","when":"lane.start"}]))]
/// One-shot command, script, or named steps, run on demand or from a lifecycle hook.
pub struct TaskConfig {
    /// Shell text or an argument vector. Mutually exclusive with run_script and steps.
    pub run: Option<Run>,
    /// Script source and name, such as package.json#migrate or Makefile#build.
    pub run_script: Option<RunScript>,
    /// Working directory relative to the lane's worktree.
    #[schemars(extend("examples" = ["apps/api"]))]
    pub cwd: Option<Templated<String>>,
    /// Task-specific environment overrides.
    #[serde(default)]
    #[schemars(extend("examples" = [{"API_URL":"${{ svc.api.url }}"}]))]
    pub env: Env,
    /// Task-specific fnox profile.
    pub fnox: Option<FnoxConfig>,
    /// Lifecycle hook; omitted tasks run only on demand.
    pub when: Option<TaskWhen>,
    /// Delay a create/start hook until all selected services are ready.
    pub after: Option<TaskAfter>,
    /// Profiles for which this lifecycle hook runs automatically.
    #[schemars(extend("examples" = [["default","backend"]]))]
    pub profiles: Option<Vec<String>>,
    /// Sequential task names or infra.setup/infra.teardown entries, always run explicitly.
    #[schemars(extend("examples" = [["infra.teardown.database","infra.setup.database","migrate","seed"]]))]
    pub steps: Option<Vec<String>>,
    /// Confirmation text; noninteractive clients must explicitly approve.
    #[schemars(extend("examples" = ["Reset ${{ lane.db_name }} and re-seed it?"]))]
    pub confirm: Option<Templated<String>>,
    /// Require an attached terminal; incompatible with lifecycle hooks.
    #[schemars(extend("examples" = [true,false]))]
    pub interactive: Option<Templated<bool>>,
    /// Maximum run duration, for example 10m.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
    /// Allocate a pseudoterminal instead of pipes.
    #[schemars(extend("examples" = [true,false]))]
    pub pty: Option<Templated<bool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(extend("examples" = ["lane.create","lane.start","lane.stop","lane.clean"]))]
/// Lifecycle event that runs a task automatically.
pub enum TaskWhen {
    /// Run until the task succeeds once for this lane.
    #[schemars(extend("examples" = ["lane.create"]))]
    #[serde(rename = "lane.create")]
    Create,
    /// Run each time the lane starts.
    #[schemars(extend("examples" = ["lane.start"]))]
    #[serde(rename = "lane.start")]
    Start,
    /// Run when the lane stops, but not on daemon shutdown.
    #[schemars(extend("examples" = ["lane.stop"]))]
    #[serde(rename = "lane.stop")]
    Stop,
    /// Run during lane cleanup before infrastructure teardown.
    #[schemars(extend("examples" = ["lane.clean"]))]
    #[serde(rename = "lane.clean")]
    Clean,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["services"]))]
/// Move a create/start task to a later startup phase.
pub enum TaskAfter {
    /// Run after selected services are ready; skip if any selected service failed.
    #[schemars(extend("examples" = ["services"]))]
    Services,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = [["+api","+web"],{"include":["+web"],"infra":false}]))]
/// Named service selection with optional environment, infrastructure, and script overrides.
pub enum ProfileConfig {
    /// Service names, globs, or +tag selectors.
    #[schemars(extend("examples" = [["+api","+web"]]))]
    Selectors(Vec<String>),
    /// Selection and overrides expressed as a mapping.
    #[schemars(extend("examples" = [{"include":["+web"],"infra":false}]))]
    Object(ProfileOptions),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"include":["+api","+web"],"run_scripts":{"dev":"start"},"env":{"NODE_ENV":"production"}}]))]
/// Service selectors and overrides applied when this profile is active.
pub struct ProfileOptions {
    /// Service names, globs, and +tag selectors included by this profile.
    #[schemars(extend("examples" = [["+api","+web"],["*"]]))]
    pub include: Option<Vec<String>>,
    /// Selectors removed from the included service set.
    #[schemars(extend("examples" = [["+desktop","sms-stub"]]))]
    pub exclude: Option<Vec<String>>,
    /// Set false to skip containers and infra setup.
    #[schemars(extend("examples" = [false,true]))]
    pub infra: Option<Templated<bool>>,
    /// Profile environment overrides, applied above service environment values.
    #[serde(default)]
    #[schemars(extend("examples" = [{"NODE_ENV":"production"}]))]
    pub env: Env,
    /// Remap service script names once, such as dev to start; source files stay unchanged.
    #[serde(default)]
    #[schemars(schema_with = "script_remaps_schema", extend("examples" = [{"dev":"start"}]))]
    pub run_scripts: IndexMap<String, String>,
}

impl ProfileOptions {
    /// Return a single-step script remapping, or the original name when no mapping exists.
    pub fn remap_script<'a>(&'a self, name: &'a str) -> &'a str {
        self.run_scripts.get(name).map_or(name, String::as_str)
    }
}

/// Express nonempty script keys and values to YAML editors as well as runtime validation.
fn script_remaps_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "type":"object",
        "propertyNames":{"type":"string","minLength":1,"pattern":"\\S"},
        "additionalProperties":{"type":"string","minLength":1,"pattern":"\\S"}
    })
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["api.${{ lane.domain }}",{"host":"*.${{ lane.domain }}","fallback":true}]))]
/// Hostname routed by the HTTP proxy, optionally with route settings.
pub enum HostConfig {
    /// Hostname routed to the service's primary port. Dot-separated labels of letters,
    /// digits and inner hyphens, with an optional leading `*.` wildcard; case and a
    /// trailing dot are ignored.
    #[schemars(extend("examples" = ["api.${{ lane.domain }}"]))]
    Name(Templated<String>),
    /// Hostname with optional named port, fallback, and Host-header policy.
    #[schemars(extend("examples" = [{"host":"*.${{ lane.domain }}","fallback":true}]))]
    Route(HostRoute),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"host":"inspect.${{ lane.domain }}","port":"inspector"},{"host":"*.${{ lane.domain }}","fallback":true}]))]
/// HTTP proxy route to a service or container port.
pub struct HostRoute {
    /// Exact hostname or `*.` wildcard pattern, evaluated for the current lane. Labels
    /// hold letters, digits and inner hyphens; case and a trailing dot are ignored.
    #[schemars(extend("examples" = ["api.${{ lane.domain }}","*.${{ lane.domain }}"]))]
    pub host: Templated<String>,
    #[schemars(extend("examples" = ["inspector"]))]
    /// Named port to route to; uses the unit's primary port when omitted.
    pub port: Option<String>,
    #[schemars(extend("examples" = [true,false]))]
    /// Use this route only when no other hostname matches; defaults to false.
    pub fallback: Option<Templated<bool>>,
    /// Preserve the browser's Host header by default, or rewrite it for the upstream.
    pub host_header: Option<HostHeader>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["preserve","upstream"]))]
/// Host header sent to the upstream HTTP server.
pub enum HostHeader {
    /// Keep the Host header supplied by the client.
    #[schemars(extend("examples" = ["preserve"]))]
    Preserve,
    /// Send the upstream address and port as the Host header.
    #[schemars(extend("examples" = ["upstream"]))]
    Upstream,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["started",{"http":{"path":"/healthz"}},{"tcp":true}]))]
/// Readiness policy; defaults to TCP for services with hosts and started for other services.
pub enum ReadyConfig {
    /// Treat a running process as ready without a probe.
    #[schemars(extend("examples" = ["started"]))]
    Started(Started),
    /// Wait for a TCP, HTTP, log, or command check to succeed.
    #[schemars(extend("examples" = [{"http":{"path":"/healthz"}},{"tcp":true}]))]
    Check(ReadyCheck),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["started"]))]
/// Readiness without an application-level probe.
pub enum Started {
    /// The process has started; no TCP, HTTP, log, or command probe is needed.
    #[schemars(extend("examples" = ["started"]))]
    Started,
}

/// Exactly one readiness mechanism is accepted by semantic validation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"http":{"path":"/healthz"}},{"tcp":true},{"log":"Connected to Redis"},{"cmd":"curl -fsS ${{ self.url }}/healthz"}]))]
pub struct ReadyCheck {
    /// Wait for the primary port with true, or a named port such as inspector.
    pub tcp: Option<TcpReady>,
    /// Wait for an HTTP response matching the requested path and status.
    pub http: Option<HttpReady>,
    /// Regular expression matched against output with terminal escape codes removed.
    #[schemars(extend("examples" = ["Connected to Redis"]))]
    pub log: Option<Templated<String>>,
    #[schemars(extend("examples" = ["curl -fsS ${{ self.url }}/healthz"]))]
    /// Retry this command until it exits successfully.
    pub cmd: Option<Run>,
    /// Maximum time to wait for the readiness check.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = [true,"inspector"]))]
/// TCP port to probe for readiness.
pub enum TcpReady {
    /// Set true to probe the unit's primary port; false is invalid.
    #[schemars(extend("examples" = [true]))]
    Enabled(bool),
    /// Probe a named extra port instead of the primary port.
    #[schemars(extend("examples" = ["inspector"]))]
    Port(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"path":"/healthz","status":200,"timeout":"60s"}]))]
/// HTTP readiness probe against the unit's local server.
pub struct HttpReady {
    /// Request path, such as /healthz.
    #[schemars(extend("examples" = ["/healthz","/-/health"]))]
    pub path: Option<Templated<String>>,
    #[schemars(extend("examples" = [200]))]
    /// HTTP response status required for readiness, between 100 and 599.
    pub status: Option<Templated<u16>>,
    /// Maximum time to wait for the HTTP probe to succeed.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"from":"apps/extension/.output/chrome-mv3","ignore":["*.map"]}]))]
/// Directory copied to a stable local or remote-client path.
pub struct SyncConfig {
    /// Directory to copy, relative to the lane's worktree.
    #[schemars(extend("examples" = ["apps/extension/.output/chrome-mv3"]))]
    pub from: Templated<String>,
    #[schemars(extend("examples" = ["~/devlanes/sync/acme/${{ lane.name }}/chrome-mv3"]))]
    /// Destination on the client; defaults to a stable project/lane path under ~/devlanes/sync.
    pub to: Option<Templated<String>>,
    /// Glob patterns excluded from the copy and subsequent updates.
    #[serde(default)]
    #[schemars(extend("examples" = [["*.map"]]))]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["docker-compose.yml",["docker-compose.yml","docker-compose.dev.yml"]]))]
/// Compose files used to resolve and start infrastructure.
pub enum ComposeFiles {
    /// One Compose file relative to the project.
    #[schemars(extend("examples" = ["docker-compose.yml"]))]
    File(String),
    /// Ordered Compose files, merged in the same order as repeated -f arguments.
    #[schemars(extend("examples" = [["docker-compose.yml","docker-compose.dev.yml"]]))]
    Files(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"compose":"docker-compose.yml","hidden":["postgres"]}]))]
/// Containers and per-lane setup/teardown commands started before application services.
pub struct InfraConfig {
    /// Compose file or ordered list of Compose files.
    pub compose: Option<ComposeFiles>,
    /// Shared Compose project name; defaults to Compose's own name.
    #[schemars(extend("examples" = ["acme"]))]
    pub project_name: Option<Templated<String>>,
    /// Compose services never started by devlanes.
    #[schemars(extend("examples" = [["caddy"]]))]
    pub exclude: Option<Vec<String>>,
    /// Compose services instantiated separately in each lane.
    #[schemars(extend("examples" = [["mailpit"]]))]
    pub per_lane: Option<Vec<String>>,
    /// Compose profiles enabled when resolving and starting infrastructure.
    #[schemars(extend("examples" = [["development"]]))]
    pub profiles: Option<Vec<String>>,
    /// Variables for Compose interpolation, separate from process global_env.
    #[serde(default)]
    #[schemars(extend("examples" = [{"ACME_DB_PORT":15432}]))]
    pub env: Env,
    /// Container name to HTTP proxy route declarations.
    #[serde(default)]
    #[schemars(extend("examples" = [{"s3mock":["s3.${{ lane.domain }}"]}]))]
    pub hosts: IndexMap<String, Vec<HostConfig>>,
    /// Containers whose ports must not be mirrored to remote clients.
    #[schemars(extend("examples" = [["postgres"]]))]
    pub hidden: Option<Vec<String>>,
    /// Plain Docker containers managed alongside Compose services.
    #[serde(default)]
    #[schemars(extend("examples" = [{"cerbos":{"image":"ghcr.io/cerbos/cerbos:0.40.0","ports":{"http":3592},"per_lane":true}}]))]
    pub containers: IndexMap<String, ContainerConfig>,
    /// Named commands run once per lane after containers become healthy.
    #[serde(default)]
    #[schemars(extend("examples" = [{"database":"docker compose exec -T postgres createdb acme_${{ lane.name }}"}]))]
    pub setup: IndexMap<String, InfraCommand>,
    /// Named cleanup commands run when a lane is explicitly cleaned.
    #[serde(default)]
    #[schemars(extend("examples" = [{"database":"docker compose exec -T postgres dropdb --if-exists acme_${{ lane.name }}"}]))]
    pub teardown: IndexMap<String, InfraCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"image":"ghcr.io/cerbos/cerbos:0.40.0","ports":{"http":3592,"grpc":3593},"per_lane":true}]))]
/// Plain Docker container managed alongside Compose services.
pub struct ContainerConfig {
    /// Docker image to run; required after local overrides are merged.
    #[schemars(extend("examples" = ["ghcr.io/cerbos/cerbos:0.40.0"]))]
    pub image: Option<Templated<String>>,
    #[schemars(extend("examples" = [["server","--config=/policies/config.yaml"]]))]
    /// Command replacing the image's default command.
    pub command: Option<Run>,
    #[schemars(extend("examples" = [["./infra/cerbos:/policies:ro"]]))]
    /// Docker source:destination[:mode] mounts; relative sources use the owning worktree.
    pub volumes: Option<Vec<Templated<String>>>,
    #[serde(default)]
    #[schemars(extend("examples" = [{"http":3592,"grpc":3593}]))]
    /// Named container ports; host ports are allocated by devlanes.
    pub ports: IndexMap<String, Templated<u16>>,
    #[schemars(extend("examples" = [true,false]))]
    /// Create one container per lane; defaults to a shared project container.
    pub per_lane: Option<Templated<bool>>,
    /// Environment variables passed to this container.
    #[serde(default)]
    #[schemars(extend("examples" = [{"LOG_LEVEL":"debug"}]))]
    pub env: Env,
    /// Readiness check for the container.
    pub ready: Option<ReadyConfig>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["docker compose exec -T postgres createdb acme_${{ lane.name }}",{"run":["aws","s3","mb","s3://${{ lane.bucket }}"],"ignore_errors":true}]))]
/// Named infrastructure command in shorthand or object form.
pub enum InfraCommand {
    /// Command or script with lifecycle and failure-handling settings.
    #[schemars(extend("examples" = [{"run":"docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }}","ignore_errors":true}]))]
    Object(Box<InfraCommandOptions>),
    /// Shell command or argv without extra options.
    #[schemars(extend("examples" = ["docker compose exec -T postgres createdb acme_${{ lane.name }}"]))]
    Command(Run),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"run":"docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }}","container":"rabbitmq","ignore_errors":true}]))]
/// Setup or teardown command with optional container restart tracking.
pub struct InfraCommandOptions {
    /// Shell text or argv; mutually exclusive with run_script.
    pub run: Option<Run>,
    /// Script source and name; mutually exclusive with run.
    pub run_script: Option<RunScript>,
    #[schemars(extend("examples" = ["rabbitmq"]))]
    /// Container whose restart invalidates this setup entry's completion record, so it runs again.
    pub container: Option<String>,
    #[schemars(extend("examples" = [true,false]))]
    /// Continue when the command fails; defaults to false.
    pub ignore_errors: Option<Templated<bool>>,
    /// Working directory relative to the lane's worktree.
    #[schemars(extend("examples" = ["infra"]))]
    pub cwd: Option<Templated<String>>,
    #[serde(default)]
    #[schemars(extend("examples" = [{"AWS_DEFAULT_REGION":"us-east-1"}]))]
    /// Environment overrides for this command.
    pub env: Env,
    #[schemars(extend("examples" = ["15s","10m"]))]
    /// Maximum run duration for this command.
    pub timeout: Option<Templated<ConfigDuration>>,
}

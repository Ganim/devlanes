use crate::{BuiltinExtensions, ConfigDuration, Extensions, Scalar, ServiceConfig, Templated};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type Env = IndexMap<String, Templated<Scalar>>;

/// Fully merged project configuration. Map order is declaration order.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, bound = "E: Extensions")]
pub struct ProjectConfig<E: Extensions = BuiltinExtensions> {
    /// Project identity shared by every worktree; lowercase letters, digits, dots, underscores, and dashes.
    pub project: String,
    /// Tag settings. Units are colored by their first tag; colors are inferred from common tag names (api, web, worker, db, ...) when not set here.
    #[serde(default)]
    pub tags: IndexMap<String, crate::TagConfig>,
    /// Reusable service settings, inherited with extends after local overrides are merged.
    #[serde(default)]
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
    pub global_env: Env,
    /// One-shot commands; lifecycle hooks execute in declaration order.
    #[serde(default)]
    pub tasks: IndexMap<String, TaskConfig>,
    /// Long-running processes started together after infrastructure and before-service tasks.
    #[serde(default)]
    pub services: IndexMap<String, ServiceConfig<E>>,
    /// Named service selectors with optional environment and infrastructure overrides.
    #[serde(default)]
    pub profiles: IndexMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LanesConfig {
    /// Branch assigned lane zero; defaults to the remote default branch, main, or master.
    pub primary: Option<String>,
    /// Action for newly discovered worktrees; defaults to register.
    pub new_worktrees: Option<NewWorktrees>,
    /// Stop processes or also clean lane data when a worktree disappears.
    pub removed_worktrees: Option<RemovedWorktrees>,
    /// Directory for newly created worktrees, relative to the project.
    pub worktree_dir: Option<Templated<String>>,
    /// Fixed port-block capacity; otherwise allocated with fifty percent headroom.
    pub block_size: Option<Templated<u16>>,
    /// Maximum assigned lanes; existing lanes are retained if this is lowered.
    pub max: Option<Templated<u32>>,
    /// Project-specific host-file hint, overriding the user's policy.
    pub hosts_file: Option<HostsFile>,
    /// Scalar lane values available through the lane expression namespace.
    #[serde(default)]
    pub vars: Env,
    /// Explicit lane values; entry N describes lane N. Mutually exclusive with vars.
    pub matrix: Option<Vec<Env>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NewWorktrees {
    Register,
    Setup,
    Start,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemovedWorktrees {
    Stop,
    Clean,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostsFile {
    Auto,
    Never,
    Always,
}

/// Shell text runs in a shell; argv is passed directly to the executable.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Run {
    Shell(Templated<String>),
    Argv(Vec<Templated<String>>),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RunScript {
    Short(String),
    Object(ScriptSource),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScriptSource {
    pub source: String,
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
pub struct FnoxConfig {
    pub profile: Option<Templated<String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskConfig {
    /// Shell text or an argument vector. Mutually exclusive with run_script and steps.
    pub run: Option<Run>,
    /// Script source and name, such as package.json#migrate or Makefile#build.
    pub run_script: Option<RunScript>,
    /// Working directory relative to the lane's worktree.
    pub cwd: Option<Templated<String>>,
    /// Task-specific environment overrides.
    #[serde(default)]
    pub env: Env,
    /// Task-specific fnox profile.
    pub fnox: Option<FnoxConfig>,
    /// Lifecycle hook; omitted tasks run only on demand.
    pub when: Option<TaskWhen>,
    /// Delay a create/start hook until all selected services are ready.
    pub after: Option<TaskAfter>,
    /// Profiles for which this lifecycle hook runs automatically.
    pub profiles: Option<Vec<String>>,
    /// Sequential task names or infra.setup/infra.teardown entries, always run explicitly.
    pub steps: Option<Vec<String>>,
    /// Confirmation text; noninteractive clients must explicitly approve.
    pub confirm: Option<Templated<String>>,
    /// Require an attached terminal; incompatible with lifecycle hooks.
    pub interactive: Option<Templated<bool>>,
    /// Maximum run duration, for example 10m.
    pub timeout: Option<Templated<ConfigDuration>>,
    /// Allocate a pseudoterminal instead of pipes.
    pub pty: Option<Templated<bool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum TaskWhen {
    #[serde(rename = "lane.create")]
    Create,
    #[serde(rename = "lane.start")]
    Start,
    #[serde(rename = "lane.stop")]
    Stop,
    #[serde(rename = "lane.clean")]
    Clean,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TaskAfter {
    Services,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ProfileConfig {
    Selectors(Vec<String>),
    Object(ProfileOptions),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileOptions {
    /// Service names, globs, and +tag selectors included by this profile.
    pub include: Option<Vec<String>>,
    /// Selectors removed from the included service set.
    pub exclude: Option<Vec<String>>,
    /// Set false to skip containers and infra setup.
    pub infra: Option<Templated<bool>>,
    /// Profile environment overrides, applied above service environment values.
    #[serde(default)]
    pub env: Env,
    /// Remap service script names once, such as dev to start; source files stay unchanged.
    #[serde(default)]
    #[schemars(schema_with = "script_remaps_schema")]
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum HostConfig {
    Name(Templated<String>),
    Route(HostRoute),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HostRoute {
    pub host: Templated<String>,
    pub port: Option<String>,
    pub fallback: Option<Templated<bool>>,
    pub host_header: Option<HostHeader>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HostHeader {
    Preserve,
    Upstream,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ReadyConfig {
    Started(Started),
    Check(ReadyCheck),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Started {
    Started,
}

/// Exactly one readiness mechanism is accepted by semantic validation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadyCheck {
    pub tcp: Option<TcpReady>,
    pub http: Option<HttpReady>,
    pub log: Option<Templated<String>>,
    pub cmd: Option<Run>,
    pub timeout: Option<Templated<ConfigDuration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum TcpReady {
    Enabled(bool),
    Port(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpReady {
    pub path: Option<Templated<String>>,
    pub status: Option<Templated<u16>>,
    pub timeout: Option<Templated<ConfigDuration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SyncConfig {
    pub from: Templated<String>,
    pub to: Option<Templated<String>>,
    #[serde(default)]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ComposeFiles {
    File(String),
    Files(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InfraConfig {
    /// Compose file or ordered list of Compose files.
    pub compose: Option<ComposeFiles>,
    /// Shared Compose project name; defaults to Compose's own name.
    pub project_name: Option<Templated<String>>,
    /// Compose services never started by devlanes.
    pub exclude: Option<Vec<String>>,
    /// Compose services instantiated separately in each lane.
    pub per_lane: Option<Vec<String>>,
    /// Compose profiles enabled when resolving and starting infrastructure.
    pub profiles: Option<Vec<String>>,
    /// Variables for Compose interpolation, separate from process global_env.
    #[serde(default)]
    pub env: Env,
    /// Container name to HTTP proxy route declarations.
    #[serde(default)]
    pub hosts: IndexMap<String, Vec<HostConfig>>,
    /// Containers whose ports must not be mirrored to remote clients.
    pub hidden: Option<Vec<String>>,
    /// Plain Docker containers managed alongside Compose services.
    #[serde(default)]
    pub containers: IndexMap<String, ContainerConfig>,
    /// Named commands run once per lane after containers become healthy.
    #[serde(default)]
    pub setup: IndexMap<String, InfraCommand>,
    /// Named cleanup commands run when a lane is explicitly cleaned.
    #[serde(default)]
    pub teardown: IndexMap<String, InfraCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContainerConfig {
    pub image: Option<Templated<String>>,
    pub command: Option<Run>,
    pub volumes: Option<Vec<Templated<String>>>,
    #[serde(default)]
    pub ports: IndexMap<String, Templated<u16>>,
    pub per_lane: Option<Templated<bool>>,
    #[serde(default)]
    pub env: Env,
    pub ready: Option<ReadyConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum InfraCommand {
    Object(Box<InfraCommandOptions>),
    Command(Run),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InfraCommandOptions {
    pub run: Option<Run>,
    pub run_script: Option<RunScript>,
    pub container: Option<String>,
    pub ignore_errors: Option<Templated<bool>>,
    pub cwd: Option<Templated<String>>,
    #[serde(default)]
    pub env: Env,
    pub timeout: Option<Templated<ConfigDuration>>,
}

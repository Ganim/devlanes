use crate::{ConfigDuration, Extensions, OpaqueExtensions, Scalar, ServiceConfig, Templated};
use indexmap::IndexMap;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub type Env = IndexMap<String, Templated<Scalar>>;

/// devlanes project file: the services, tasks and infrastructure devlanes runs once per
/// Git worktree. Each worktree is a lane with its own ports, hostnames and data.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, bound = "E: Extensions")]
#[schemars(extend("examples" = [{"project":"acme","services":{"api":{"run_script":{"file":"apps/api/package.json","name":"dev"}}}}]))]
pub struct ProjectConfig<E: Extensions = OpaqueExtensions> {
    /// Source documents retained in memory for diagnostics after integration validation.
    #[serde(skip)]
    #[schemars(skip)]
    pub sources: crate::ConfigSources,
    /// Project name, the same in every worktree of the repository. It builds each lane's
    /// default domain (`<project>.localhost` for lane 0, `<project>-<N>.localhost` for lane N),
    /// names lanes in commands (`acme@2`), the containers devlanes creates and
    /// `DEVLANES_PROJECT`, and other projects reach this one as `projects.<project>`.
    /// Lowercase letters, digits, dots, underscores and dashes.
    #[schemars(extend("examples" = ["acme"]))]
    pub project: String,
    /// Optional color and description per tag. Services list their tags in their own `tags`,
    /// and tags group them: `+api` selects every service tagged `api` in profiles and in
    /// commands such as `devlanes start +api`. A unit's color comes from the first of its tags
    /// with a `color` set here, else the first tag naming a category such as api, web, worker
    /// or db, else its preset or name.
    /// Tags not listed here work too.
    #[serde(default)]
    #[schemars(extend("examples" = [{"api":{"color":"cyan","description":"HTTP APIs"}}]))]
    pub tags: IndexMap<String, crate::TagConfig>,
    /// Named service settings that services reuse with `extends: <template>`. The service
    /// inherits every setting of the template and its own settings win: mappings (`env`,
    /// `ready`, `stop`, preset options, `run_script`) merge by key, lists (`tags`,
    /// `hosts`, `sync`) and single values replace. Setting `run` or `run_script` drops the
    /// inherited other one, and one `ready` probe (`tcp`, `http`, `log`, `cmd`) drops the
    /// inherited probe. Templates may extend other templates, are never started themselves,
    /// and are applied after `devlanes.local.yaml` is merged.
    #[serde(default)]
    #[schemars(extend("examples" = [{"fastify-api":{"platform":"node","tags":["api"],"ready":{"http":{"path":"/-/health"}}}}]))]
    pub templates: IndexMap<String, ServiceConfig<E>>,
    /// Which worktrees get a lane, where new worktrees are created, how many ports each
    /// lane reserves, and values computed per lane (`lane.<name>`).
    #[serde(default)]
    pub lanes: LanesConfig,
    /// Containers the lanes depend on (databases, brokers, object stores), started and
    /// waited on before setup, tasks and services, plus the commands that create and remove
    /// each lane's data in them.
    #[serde(default)]
    pub infra: InfraConfig,
    /// Variables for every service and task of the lane, above devlanes' own variables
    /// (`PORT`, `DEVLANES_*`) and below each unit's `env` and the profile's `env`. Use it for
    /// values many units share, such as a database URL built from `${{ infra.postgres.port }}`.
    /// Never set `PORT` here: it would replace every service's own port.
    #[serde(default)]
    #[schemars(extend("examples" = [{"API_URL":"${{ services.api.url }}"}]))]
    pub global_env: Env,
    /// One-shot commands such as installs, migrations and seeds, run by hand with
    /// `devlanes run <name>` or automatically at a lane event set with `when`. Tasks of the
    /// same event run in declaration order.
    #[serde(default)]
    #[schemars(extend("examples" = [{"migrate":{"run_script":{"file":"apps/api/package.json","name":"migrate"},"when":"lane.start"}}]))]
    pub tasks: IndexMap<String, TaskConfig>,
    /// Long-running processes of each lane, started together after infrastructure, setup
    /// and `lane.start` tasks. Each service gets its own port as `PORT` and, with `hosts`,
    /// HTTPS hostnames. A service that exits shows as failed and is not restarted.
    #[serde(default)]
    #[schemars(extend("examples" = [{"api":{"run_script":{"file":"apps/api/package.json","name":"dev"},"hosts":"api.${{ lane.domain }}"}}]))]
    pub services: IndexMap<String, ServiceConfig<E>>,
    /// Named selections of services for `devlanes start --profile <name>`; `default` is
    /// used when no profile is given. A profile is a list of names, globs and `+tag`
    /// selectors, or a mapping that can also exclude services, skip infra, add `env` and
    /// remap script names.
    #[serde(default)]
    #[schemars(extend("examples" = [{"default":["+api","+web"],"e2e":{"include":["*"],"run_scripts":{"dev":"start"}}}]))]
    pub profiles: IndexMap<String, ProfileConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"primary":"main","hosts_file":"always","vars":{"db_name":"acme_${{ lane.name }}"}}]))]
/// Each Git worktree of the project is a lane with its own ports, hostnames and data.
/// These settings decide which worktrees get a lane, where new ones are created, how
/// many ports each lane reserves, and the values each lane computes.
pub struct LanesConfig {
    /// Branch whose worktree is lane 0, the main lane (`lanes.main` in expressions) and the
    /// only worktree that gets a lane by itself. Defaults to the remote's default branch,
    /// else `main` or `master`.
    #[schemars(extend("examples" = ["main"]))]
    pub primary: Option<String>,
    /// What devlanes does when a worktree gets its lane: only reserve it, also set it up,
    /// or also start it. Defaults to `register`. A worktree gets a lane the first time
    /// devlanes runs in it or `devlanes lane new` creates it, not merely because it exists.
    pub new_worktrees: Option<NewWorktrees>,
    /// What happens to a running lane whose worktree folder is deleted. Defaults to `stop`.
    /// A lane whose folder is gone and that runs nothing is cleaned; its logs stay until
    /// log retention removes them.
    pub removed_worktrees: Option<RemovedWorktrees>,
    /// Directory where `devlanes lane new` creates worktrees, relative to the project
    /// directory of the primary worktree. Defaults to `../<project>-lanes`. Expressions
    /// can only use `project.*` here.
    #[schemars(extend("examples" = ["../${{ project.name }}-lanes"]))]
    pub worktree_dir: Option<Templated<String>>,
    /// Number of ports reserved for each lane. By default devlanes counts the ports the
    /// config needs and adds half again, rounded up to ten. A fixed size smaller than what
    /// the config needs is an error.
    #[schemars(extend("examples" = [80]))]
    pub block_size: Option<Templated<u16>>,
    /// Most lanes the project may hold at once; further worktrees stay unassigned, without
    /// ports. Lowering it keeps existing lanes. Unset, the number of lanes is limited only
    /// by free port blocks.
    #[schemars(extend("examples" = [6]))]
    pub max: Option<Templated<u32>>,
    /// Whether this project's hostnames are also written to `/etc/hosts`, for programs
    /// that cannot resolve `*.localhost` by themselves (Wrangler's workerd, musl or pure-Go
    /// binaries). Overrides the user's `hostnames.hosts_file` (default `auto`), except that
    /// a user `never` always wins. Writing needs the helper installed by
    /// `devlanes certs trust --hosts`, and only exact names are written, never wildcards.
    pub hosts_file: Option<HostsFile>,
    /// Values computed for each lane and read as `${{ lane.<name> }}`, such as a database
    /// name, vhost, bucket or Redis index per lane. A var named `domain` replaces the
    /// default lane domain, and one named `port` fixes where the lane's port block starts.
    /// Cannot be combined with `matrix`.
    #[serde(default)]
    #[schemars(extend("examples" = [{"db_name":"acme_${{ lane.name }}"}]))]
    pub vars: Env,
    /// Fixed values per lane instead of computed `vars`: entry N holds lane N's values,
    /// read as `lane.<name>`. A lane whose index has no entry fails to resolve. Cannot be
    /// combined with `vars`.
    #[schemars(extend("examples" = [[{"port":20000,"domain":"acme.localhost"},{"port":20100,"domain":"acme-1.localhost"}]]))]
    pub matrix: Option<Vec<Env>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["register","setup","start"]))]
/// What devlanes does when a worktree gets its lane.
pub enum NewWorktrees {
    /// Only reserve the lane (its index, port block and domain); nothing runs until the
    /// lane is started.
    #[schemars(extend("examples" = ["register"]))]
    Register,
    /// Also run infrastructure setup and `lane.create` tasks, leaving services stopped.
    #[schemars(extend("examples" = ["setup"]))]
    Setup,
    /// Also start the lane with the default profile.
    #[schemars(extend("examples" = ["start"]))]
    Start,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["stop","clean"]))]
/// What happens to a running lane whose worktree folder is deleted.
pub enum RemovedWorktrees {
    /// Stop the lane's processes; once nothing runs, the lane is cleaned.
    #[schemars(extend("examples" = ["stop"]))]
    Stop,
    /// Clean the lane right away: stop its processes, run `lane.clean` tasks and
    /// `infra.teardown`, and free its index and ports.
    #[schemars(extend("examples" = ["clean"]))]
    Clean,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["auto","never","always"]))]
/// Whether routed hostnames are also written to `/etc/hosts`.
pub enum HostsFile {
    /// Write the exact hostnames only when the daemon's startup check finds that the system
    /// resolver does not answer `*.localhost` with a loopback address.
    #[schemars(extend("examples" = ["auto"]))]
    Auto,
    /// Never write hostnames to `/etc/hosts`. Set in the user config, it applies to every
    /// project whatever the project sets.
    #[schemars(extend("examples" = ["never"]))]
    Never,
    /// Always write the exact hostnames, even when the system resolver handles
    /// `*.localhost`, for programs that send `*.localhost` to DNS themselves.
    #[schemars(extend("examples" = ["always"]))]
    Always,
}

/// Command as shell text, run through `/bin/sh -c`, or as a list of program and
/// arguments run without a shell.
#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["pnpm dev",["node","server.mjs","--port","${{ self.port }}"]]))]
pub enum Run {
    /// Shell text run through `/bin/sh -c`; pipelines and multi-line scripts work.
    #[schemars(extend("examples" = ["pnpm dev"]))]
    Shell(Templated<String>),
    /// Program and arguments run directly, without shell expansion.
    #[schemars(extend("examples" = [["node","server.mjs"]]))]
    Argv(Vec<Templated<String>>),
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
#[schemars(deny_unknown_fields, transform = optional_script_keys)]
#[schemars(extend("examples" = [{"file":"apps/api/package.json","name":"dev"},{"file":"justfile","name":"fixtures"}]))]
/// Script from a project file, as a mapping with `file` and `name`. devlanes picks the
/// tool from the file name: package.json (the project's package manager), Makefile or
/// `*.mk`, justfile or `*.just`, deno.json, pyproject.toml (uv or poetry) and Cargo.toml (a
/// binary). In `devlanes.local.yaml` or a service that `extends` a template, either key can
/// be left out to keep the inherited one.
pub struct RunScript {
    /// File that defines the script, relative to the worktree, such as
    /// `apps/api/package.json`. The script runs in this file's directory. Required unless
    /// an override or `extends` child keeps the inherited file.
    #[schemars(extend("examples" = ["apps/api/package.json","Makefile","justfile"]))]
    pub file: String,
    #[schemars(extend("examples" = ["dev","fixtures"]))]
    /// Script, target, recipe, task or binary name in that file. Required unless an
    /// override or `extends` child keeps the inherited name.
    pub name: String,
}

/// Match the parser, which lets an override or `extends` child leave out either key;
/// `RunScript::file_and_name` requires both once merged.
fn optional_script_keys(schema: &mut schemars::Schema) {
    schema.remove("required");
}

impl RunScript {
    /// The file and script name, failing when either is missing after merging.
    pub fn file_and_name(&self) -> Result<(&str, &str), crate::ConfigError> {
        if self.file.is_empty() || self.name.is_empty() {
            return Err(crate::ConfigError::plain(
                "run_script needs both `file` and `name`, such as `{ file: apps/api/package.json, name: dev }`",
            ));
        }
        Ok((&self.file, &self.name))
    }

    /// The file and script name with the name swapped by a profile's `run_scripts`.
    pub fn file_and_name_for_profile<'a>(
        &'a self,
        profile: Option<&'a ProfileOptions>,
    ) -> Result<(&'a str, &'a str), crate::ConfigError> {
        let (file, name) = self.file_and_name()?;
        Ok((
            file,
            profile.map_or(name, |profile| profile.remap_script(name)),
        ))
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"run_script":{"file":"apps/api/package.json","name":"migrate"},"when":"lane.start"}]))]
/// A one-shot command, run by hand with `devlanes run <name>` or automatically at a lane
/// event set with `when`. Anything that ends by itself belongs here, not in `services`,
/// where an exit counts as a failure.
pub struct TaskConfig {
    /// Command to run: a string runs through `/bin/sh -c` (pipes and multi-line scripts
    /// work), a list runs the program directly with those arguments. Use exactly one of
    /// `run`, `run_script` or `steps`.
    pub run: Option<Run>,
    /// Script to run from a project file, as `{ file, name }`, such as
    /// `{ file: apps/api/package.json, name: migrate }`. It runs with that file's tool (the
    /// project's package manager for package.json) in the file's directory, and a missing
    /// script fails before anything starts. Prefer it to `run` for scripts that live in a
    /// file. Use exactly one of `run`, `run_script` or `steps`.
    pub run_script: Option<RunScript>,
    /// Working directory for `run`, relative to the lane's worktree; defaults to the worktree
    /// root. Cannot be combined with `run_script`, which runs in the script file's directory.
    #[schemars(extend("examples" = ["apps/api"]))]
    pub cwd: Option<Templated<String>>,
    /// Variables for this task only, above `global_env`.
    #[serde(default)]
    #[schemars(extend("examples" = [{"API_URL":"${{ services.api.url }}"}]))]
    pub env: Env,
    /// Lane event that runs this task automatically. Without it the task runs only by hand
    /// with `devlanes run <name>` or as a step of another task.
    pub when: Option<TaskWhen>,
    /// Set to `services` to run a `lane.create` or `lane.start` task after the started
    /// services are ready instead of before them, for work that goes through the running
    /// app, such as seeding through its API.
    pub after: Option<TaskAfter>,
    /// Run this task's `when` hook only while one of these profiles is active; `default`
    /// also matches a start without `--profile`. Runs by hand are not affected. Requires
    /// `when`.
    #[schemars(extend("examples" = [["default","backend"]]))]
    pub profiles: Option<Vec<String>>,
    /// Run these tasks and `infra.setup.<name>` or `infra.teardown.<name>` entries in order,
    /// stopping at the first failure, for jobs such as resetting a database. Each step runs
    /// whatever its own `when` says, and setup entries run again even when already done.
    /// Use instead of `run` or `run_script`.
    #[schemars(extend("examples" = [["infra.teardown.database","infra.setup.database","migrate","seed"]]))]
    pub steps: Option<Vec<String>>,
    /// Question asked before the task runs, for destructive tasks. Scripts and agents
    /// without a terminal must pass `--yes` to run it.
    #[schemars(extend("examples" = ["Reset ${{ lane.db_name }} and re-seed it?"]))]
    pub confirm: Option<Templated<String>>,
    /// Give the task your terminal for input and output, for logins, prompts and TUIs.
    /// Interactive tasks only run by hand, so they cannot have `when`. Defaults to false.
    #[schemars(extend("examples" = [true,false]))]
    pub interactive: Option<Templated<bool>>,
    /// Longest the task may run. When it runs out, the task is stopped (SIGTERM, then
    /// SIGKILL after 10 seconds) and counts as failed; a failed task that runs before
    /// services fails the start. No limit by default.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
    /// Run the task in a pseudoterminal instead of plain pipes, for tools that only print
    /// colors or progress to a terminal. Defaults to the value of `interactive`.
    #[schemars(extend("examples" = [true,false]))]
    pub pty: Option<Templated<bool>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[schemars(extend("examples" = ["lane.create","lane.start","lane.stop","lane.clean"]))]
/// Lane event that runs a task automatically.
pub enum TaskWhen {
    /// Run on each start until it has succeeded once for the lane, then no longer
    /// automatically; for installs, code generation and a first seed.
    #[schemars(extend("examples" = ["lane.create"]))]
    #[serde(rename = "lane.create")]
    Create,
    /// Run on every start of the lane, before services unless `after: services` is set;
    /// for migrations and other steps that are safe to repeat.
    #[schemars(extend("examples" = ["lane.start"]))]
    #[serde(rename = "lane.start")]
    Start,
    /// Run before the lane's processes stop when the whole lane stops: `devlanes stop`
    /// without unit names, `devlanes daemon stop` and `daemon restart`. Skipped when a
    /// signal (logout, system shutdown) stops the daemon.
    #[schemars(extend("examples" = ["lane.stop"]))]
    #[serde(rename = "lane.stop")]
    Stop,
    /// Run when the lane is cleaned (`devlanes lane clean`), after its processes stop and
    /// before `infra.teardown`.
    #[schemars(extend("examples" = ["lane.clean"]))]
    #[serde(rename = "lane.clean")]
    Clean,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["services"]))]
/// Later point in the start at which a `lane.create` or `lane.start` task runs.
pub enum TaskAfter {
    /// Run once every started service is ready; the task is skipped when one of them failed
    /// or the start timed out.
    #[schemars(extend("examples" = ["services"]))]
    Services,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = [["+api","+web"],{"include":["+web"],"infra":false}]))]
/// Services started by `devlanes start --profile <name>` (`default` when no profile is
/// given): a list of names, globs and `+tag` selectors, or a mapping that can also exclude
/// services, skip infra, add `env` and remap script names. Several `--profile` flags start
/// the union.
pub enum ProfileConfig {
    /// Service names, globs and `+tag` selectors; short for `{ include: [...] }`.
    #[schemars(extend("examples" = [["+api","+web"]]))]
    Selectors(Vec<String>),
    /// Selection and overrides as a mapping.
    #[schemars(extend("examples" = [{"include":["+web"],"infra":false}]))]
    Object(ProfileOptions),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"include":["+api","+web"],"run_scripts":{"dev":"start"},"env":{"NODE_ENV":"production"}}]))]
/// Profile written as a mapping: which services it starts and what it changes for them.
pub struct ProfileOptions {
    /// Services this profile starts: names, globs such as `crm-*`, and `+tag` selectors.
    /// Globs skip services with `autostart: false`; names and tags include them.
    #[schemars(extend("examples" = [["+api","+web"],["*"]]))]
    pub include: Option<Vec<String>>,
    /// Services removed from the `include` selection, by name, glob or `+tag`.
    #[schemars(extend("examples" = [["+desktop","sms-stub"]]))]
    pub exclude: Option<Vec<String>>,
    /// Set false to skip containers and `infra.setup`, for example to run only a frontend
    /// against another lane's backend. Services that reference `infra.*` then fail to
    /// resolve. With several profiles active, infra is skipped only when all of them set
    /// false. Defaults to true.
    #[schemars(extend("examples" = [false,true]))]
    pub infra: Option<Templated<bool>>,
    /// Variables for the services this profile starts, above their own `env`.
    #[serde(default)]
    #[schemars(extend("examples" = [{"NODE_ENV":"production"}]))]
    pub env: Env,
    /// Swap script names for services that use `run_script`: `{ dev: start }` runs the
    /// `start` script wherever a service's `run_script` names `dev`. The file stays the
    /// same, and each name is swapped once, not chained.
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
/// HTTPS hostname the devlanes proxy routes to this unit in the current lane: a plain
/// hostname, or a mapping with `host` plus `port`, `fallback` or `host_header`. Keep names
/// under `lane.domain` so lanes never claim the same name.
pub enum HostConfig {
    /// Hostname routed to the unit's main port. Dot-separated labels of letters, digits
    /// and inner hyphens, with an optional leading `*.` wildcard; case and a trailing dot
    /// are ignored.
    #[schemars(extend("examples" = ["api.${{ lane.domain }}"]))]
    Name(Templated<String>),
    /// Hostname with route options.
    #[schemars(extend("examples" = [{"host":"*.${{ lane.domain }}","fallback":true}]))]
    Route(HostRoute),
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["api.${{ lane.domain }}",["api.${{ lane.domain }}",{"host":"inspect.${{ lane.domain }}","port":"inspector"}]]))]
/// HTTPS hostnames the devlanes proxy routes to this unit, so a browser reaches it at
/// `https://<host>`: one hostname or route mapping, or a list of them.
pub enum Hosts {
    /// A single hostname or route.
    #[schemars(extend("examples" = ["api.${{ lane.domain }}"]))]
    One(HostConfig),
    /// Several hostnames or routes, in order.
    #[schemars(extend("examples" = [["api.${{ lane.domain }}","admin.${{ lane.domain }}"]]))]
    Many(Vec<HostConfig>),
}

impl std::ops::Deref for Hosts {
    type Target = [HostConfig];

    /// The entries as a list, whichever form was written.
    fn deref(&self) -> &[HostConfig] {
        match self {
            Self::One(host) => std::slice::from_ref(host),
            Self::Many(hosts) => hosts,
        }
    }
}

impl<'a> IntoIterator for &'a Hosts {
    type Item = &'a HostConfig;
    type IntoIter = std::slice::Iter<'a, HostConfig>;

    /// Iterate the entries in order.
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl From<Vec<HostConfig>> for Hosts {
    /// Wrap a list of entries.
    fn from(hosts: Vec<HostConfig>) -> Self {
        Self::Many(hosts)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"host":"inspect.${{ lane.domain }}","port":"inspector"},{"host":"*.${{ lane.domain }}","fallback":true}]))]
/// Hostname route with options: a named port, catch-all behaviour or a rewritten Host header.
pub struct HostRoute {
    /// Hostname to route, evaluated for each lane. A leading `*.` makes a wildcard that
    /// matches any depth below it but not the name itself; quote it in YAML. Labels hold
    /// letters, digits and inner hyphens; case and a trailing dot are ignored.
    #[schemars(extend("examples" = ["api.${{ lane.domain }}","*.${{ lane.domain }}"]))]
    pub host: Templated<String>,
    #[schemars(extend("examples" = ["inspector"]))]
    /// Send this hostname to one of the unit's named ports instead of its main port. For a
    /// service it is a port name as in `self.ports.<name>`, and naming it here allocates
    /// that port; in `infra.hosts` it is the container port for Compose services, or the
    /// port's name for plain containers.
    pub port: Option<String>,
    #[schemars(extend("examples" = [true,false]))]
    /// Make this route a catch-all: it is used only when no other route, exact or wildcard,
    /// matches the requested hostname. Typical for `*.${{ lane.domain }}` on a gateway that
    /// should answer every subdomain other services do not claim. Defaults to false.
    pub fallback: Option<Templated<bool>>,
    /// Host header the unit receives. Defaults to `preserve`, the hostname the browser
    /// asked for; `upstream` sends the local address, such as `127.0.0.1:<port>`, for
    /// servers that reject or misroute requests for a Host that is not their own address.
    pub host_header: Option<HostHeader>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["preserve","upstream"]))]
/// Host header sent to the unit behind a hostname.
pub enum HostHeader {
    /// Send the hostname the client requested, such as `api.acme-2.localhost`.
    #[schemars(extend("examples" = ["preserve"]))]
    Preserve,
    /// Send the local address and port the proxy connects to, such as `127.0.0.1:<port>`.
    #[schemars(extend("examples" = ["upstream"]))]
    Upstream,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["started",{"http":{"path":"/healthz"}},{"tcp":true}]))]
/// When a unit counts as ready. `devlanes start` waits for it, and `after: services` tasks
/// run only once every started service is ready. Write `started`, or a mapping with one of
/// `tcp`, `http`, `log` or `cmd` and an optional `timeout`.
pub enum ReadyConfig {
    /// Ready as soon as the unit runs, without a probe.
    #[schemars(extend("examples" = ["started"]))]
    Started(Started),
    /// Ready once a `tcp`, `http`, `log` or `cmd` check passes.
    #[schemars(extend("examples" = [{"http":{"path":"/healthz"}},{"tcp":true}]))]
    Check(ReadyCheck),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(extend("examples" = ["started"]))]
/// Readiness without a probe.
pub enum Started {
    /// Count the unit ready as soon as it runs, without probing anything. A dev server
    /// whose program crashed under a file watcher still counts as ready, so prefer a check
    /// when there is something to probe.
    #[schemars(extend("examples" = ["started"]))]
    Started,
}

/// Readiness probe: exactly one of `tcp`, `http`, `log` or `cmd`, retried until it passes
/// or `timeout` runs out.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"http":{"path":"/healthz"}},{"tcp":true},{"log":"Connected to Redis"},{"cmd":"curl -fsS ${{ self.url }}/healthz"}]))]
pub struct ReadyCheck {
    /// Ready once a port accepts a TCP connection on 127.0.0.1 or ::1, tried every 150 ms.
    /// `true` uses the unit's main port; a port name such as `inspector` uses that named
    /// port. It only proves the socket is open, not that the app works.
    pub tcp: Option<TcpReady>,
    /// Ready once a GET request to `path` on the unit's main port answers with `status`, or
    /// with a 2xx status when `status` is unset. Redirects are not followed. Best when the
    /// app has a health route that fails while it is broken. Each request sends
    /// `User-Agent: devlanes-ready`, so an app can leave the checks out of its request logs.
    pub http: Option<Box<HttpReady>>,
    /// Ready once a line of output matches this regular expression, with colors and other
    /// terminal escape codes removed; recent lines printed before the check began count
    /// too. Suits workers without a port: have them print a line once connected.
    #[schemars(extend("examples" = ["Connected to Redis"]))]
    pub log: Option<Templated<String>>,
    #[schemars(extend("examples" = ["curl -fsS ${{ self.url }}/healthz"]))]
    /// Ready once this command exits 0. It runs on the host through `/bin/sh` (a list is
    /// quoted into one command line) with its output discarded, is retried until the
    /// timeout, and each attempt is cut off after 10 seconds. For a service it gets the
    /// service's environment and working directory.
    pub cmd: Option<Run>,
    /// How long the check may take to pass; defaults to 60s. When it runs out, a service
    /// is marked failed (its process keeps running and is not restarted) and
    /// `after: services` tasks are skipped; for an infra container the start fails. Raise
    /// it for services that compile before they listen.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = [true,"inspector"]))]
/// Port a `tcp` readiness check connects to: `true` for the main port, or a port name.
pub enum TcpReady {
    /// `true` probes the unit's main port; `false` is rejected.
    #[schemars(extend("examples" = [true]))]
    Enabled(bool),
    /// Named port to probe instead of the main port.
    #[schemars(extend("examples" = ["inspector"]))]
    Port(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"path":"/healthz","status":200,"timeout":"60s"}]))]
/// HTTP readiness probe against the unit's main port. Passes on a 2xx answer when `status`
/// is unset, or on exactly `status` when it is set. Redirects are not followed.
pub struct HttpReady {
    /// Path requested with GET, such as `/healthz`; defaults to `/`.
    #[schemars(extend("examples" = ["/healthz","/-/health"]))]
    pub path: Option<Templated<String>>,
    #[schemars(extend("examples" = [200]))]
    /// Status code that counts as ready, between 100 and 599; only that exact code passes.
    /// When unset, any status from 200 to 299 counts, so a 404 or a redirect keeps waiting
    /// (redirects are not followed).
    /// Set it to accept another code, such as `302` for a route that redirects to a login.
    pub status: Option<Templated<u16>>,
    /// Host header the check sends, for apps that answer only on their own hostname, such
    /// as `${{ self.hosts[0] }}`. The request still goes to 127.0.0.1; only the header
    /// changes. Defaults to `127.0.0.1:<port>`. Spaces and control characters are refused.
    #[schemars(extend("examples" = ["${{ self.hosts[0] }}","api.example.invalid"]))]
    pub host: Option<Templated<String>>,
    /// How long the HTTP check may take to pass, replacing the check's `timeout`; defaults
    /// to 60s. When it runs out, a service is marked failed but keeps running, and for an
    /// infra container the start fails.
    #[schemars(extend("examples" = ["15s","10m"]))]
    pub timeout: Option<Templated<ConfigDuration>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"from":"apps/extension/.output/chrome-mv3","ignore":["*.map"]}]))]
/// Folder copied to a fixed path and kept up to date, for build output another program
/// loads from a stable location, such as an unpacked browser extension. Also streams to
/// machines connected with `devlanes remote connect`.
pub struct SyncConfig {
    /// Folder to copy, relative to the lane's worktree.
    #[schemars(extend("examples" = ["apps/extension/.output/chrome-mv3"]))]
    pub from: Templated<String>,
    #[schemars(extend("examples" = ["~/devlanes/sync/acme/${{ lane.name }}/chrome-mv3"]))]
    /// Local destination; `~/` is your home and other relative paths start at the worktree.
    /// Defaults to `~/devlanes/sync/<project>/<lane>/<folder>`. Machines connected with
    /// `devlanes remote connect` ignore it and use their own path.
    pub to: Option<Templated<String>>,
    /// Glob patterns of files left out of the copy and of later updates.
    #[serde(default)]
    #[schemars(extend("examples" = [["*.map"]]))]
    pub ignore: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["docker-compose.yml",["docker-compose.yml","docker-compose.dev.yml"]]))]
/// Compose file, or files merged like repeated `-f` arguments.
pub enum ComposeFiles {
    /// One Compose file, relative to the project.
    #[schemars(extend("examples" = ["docker-compose.yml"]))]
    File(String),
    /// Compose files merged in order, like repeated `-f` arguments.
    #[schemars(extend("examples" = [["docker-compose.yml","docker-compose.dev.yml"]]))]
    Files(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"compose":"docker-compose.yml","hidden":["postgres"]}]))]
/// Containers the lanes depend on, started and waited on before setup, tasks and services,
/// plus the commands that create and remove each lane's data in them. Processes reach a
/// container through its host port, `${{ infra.<container>.port }}`.
pub struct InfraConfig {
    /// Compose file, or a list of files merged like repeated `-f`, relative to the project.
    /// Its services are shared by every lane unless listed in `per_lane`; the shared
    /// project runs from the primary worktree with that worktree's `.env` as `--env-file`.
    pub compose: Option<ComposeFiles>,
    /// Name of the shared Compose project, fixed so `docker compose -p <name>` reaches the
    /// same containers outside devlanes. Defaults to the name Compose picks itself.
    #[schemars(extend("examples" = ["acme"]))]
    pub project_name: Option<Templated<String>>,
    /// Compose services devlanes never starts, such as a local reverse proxy the devlanes
    /// proxy replaces.
    #[schemars(extend("examples" = [["caddy"]]))]
    pub exclude: Option<Vec<String>>,
    /// Compose services run once per lane instead of shared, each in its own Compose
    /// project with ports from the lane's block and its own named volumes, removed by
    /// `devlanes lane clean`. Needs Compose 2.24.4 or newer.
    #[schemars(extend("examples" = [["mailpit"]]))]
    pub per_lane: Option<Vec<String>>,
    /// Compose profiles to enable when resolving and starting the Compose services.
    #[schemars(extend("examples" = [["development"]]))]
    pub profiles: Option<Vec<String>>,
    /// Variables for `${VAR}` interpolation in the Compose files only; processes do not see
    /// them (use `global_env` for those).
    #[serde(default)]
    #[schemars(extend("examples" = [{"ACME_DB_PORT":15432}]))]
    pub env: Env,
    /// HTTPS hostnames for containers, keyed by Compose service or plain container name,
    /// so a browser can open a container's web port: a MinIO console or presigned S3 URLs,
    /// Mailpit, a mock OIDC provider. A hostname routes to the container's first published
    /// port unless its route sets `port`; each key takes one hostname or route, or a list.
    #[serde(default)]
    #[schemars(extend("examples" = [{"s3mock":"s3.${{ lane.domain }}"}]))]
    pub hosts: IndexMap<String, Hosts>,
    /// Containers whose ports are not mirrored to machines connected with
    /// `devlanes remote connect`.
    #[schemars(extend("examples" = [["postgres"]]))]
    pub hidden: Option<Vec<String>>,
    /// Docker containers run without a Compose file, keyed by name and reached as
    /// `infra.<name>` in expressions.
    #[serde(default)]
    #[schemars(extend("examples" = [{"cerbos":{"image":"ghcr.io/cerbos/cerbos:0.40.0","ports":{"http":3592},"per_lane":true}}]))]
    pub containers: IndexMap<String, ContainerConfig>,
    /// Named commands that prepare each lane's data, such as creating its database in a
    /// shared Postgres. They run once per lane after the containers are ready, all at the
    /// same time, and are recorded as done so later starts skip them; a failure stops the
    /// start unless the entry sets `ignore_errors`.
    #[serde(default)]
    #[schemars(extend("examples" = [{"database":"docker compose exec -T postgres createdb acme_${{ lane.name }}"}]))]
    pub setup: IndexMap<String, InfraCommand>,
    /// Named commands that remove a lane's data when the lane is cleaned
    /// (`devlanes lane clean`), one after another; never on stop. A failure is reported
    /// and the cleanup goes on.
    #[serde(default)]
    #[schemars(extend("examples" = [{"database":"docker compose exec -T postgres dropdb --if-exists acme_${{ lane.name }}"}]))]
    pub teardown: IndexMap<String, InfraCommand>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"image":"ghcr.io/cerbos/cerbos:0.40.0","ports":{"http":3592,"grpc":3593},"per_lane":true}]))]
/// Docker container run without a Compose file, shared by every lane or one per lane.
/// Processes reach it through `${{ infra.<name>.ports.<port> }}`.
pub struct ContainerConfig {
    /// Image to run. Required. A running container with the same image, command, env,
    /// mounts and ports is reused; any change recreates it.
    #[schemars(extend("examples" = ["ghcr.io/cerbos/cerbos:0.40.0"]))]
    pub image: Option<Templated<String>>,
    #[schemars(extend("examples" = [["server","--config=/policies/config.yaml"]]))]
    /// Command replacing the image's default: a list is used as is, a string runs through
    /// `/bin/sh -c` inside the container.
    pub command: Option<Run>,
    #[schemars(extend("examples" = [["./infra/cerbos:/policies:ro"]]))]
    /// Mounts as `source:destination[:mode]`. `./relative` and `/absolute` sources are bind
    /// mounts, relative ones from the owning worktree; a bare name is a named volume kept
    /// per project or per lane.
    pub volumes: Option<Vec<Templated<String>>>,
    #[serde(default)]
    #[schemars(extend("examples" = [{"http":3592,"grpc":3593}]))]
    /// Container ports by name. devlanes publishes each on a host port on 127.0.0.1, read
    /// as `${{ infra.<name>.ports.<port> }}`; `infra.<name>.port` is the first one.
    pub ports: IndexMap<String, Templated<u16>>,
    #[schemars(extend("examples" = [true,false]))]
    /// Run one container per lane instead of one shared by every lane. Defaults to false.
    /// Per-lane containers and their named volumes are removed by `devlanes lane clean`.
    pub per_lane: Option<Templated<bool>>,
    /// Environment variables set inside the container.
    #[serde(default)]
    #[schemars(extend("examples" = [{"LOG_LEVEL":"debug"}]))]
    pub env: Env,
    /// Extra check the start waits for once the container is up (the image's healthcheck,
    /// else its first port accepting TCP), before setup, tasks and services run. Write
    /// `started` or one of `tcp`, `http`, `log` (matched against the container's logs) or
    /// `cmd` (run on the host). If it does not pass within its timeout (default 60s), the
    /// start fails.
    pub ready: Option<ReadyConfig>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
#[serde(untagged)]
#[schemars(extend("examples" = ["docker compose exec -T postgres createdb acme_${{ lane.name }}",{"run":["aws","s3","mb","s3://${{ lane.bucket }}"],"ignore_errors":true}]))]
/// Command that prepares (`setup`) or removes (`teardown`) a lane's data: a command string
/// or list, or a mapping with `run` and options such as `container`, `ignore_errors` and
/// `timeout`. Task `steps` can run it as `infra.setup.<name>` or
/// `infra.teardown.<name>`.
pub enum InfraCommand {
    /// Command with options.
    #[schemars(extend("examples" = [{"run":"docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }}","ignore_errors":true}]))]
    Object(Box<InfraCommandOptions>),
    /// Command string or list without options.
    #[schemars(extend("examples" = ["docker compose exec -T postgres createdb acme_${{ lane.name }}"]))]
    Command(Run),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(extend("examples" = [{"run":"docker compose exec -T rabbitmq rabbitmqctl add_vhost ${{ lane.vhost }}","container":"rabbitmq","ignore_errors":true}]))]
/// Setup or teardown command with options.
pub struct InfraCommandOptions {
    /// Command to run: a string runs through `/bin/sh -c`, a list runs the program
    /// directly. Write the command you mean, such as `docker compose exec -T postgres ...`;
    /// devlanes runs it as written. Required.
    pub run: Option<Run>,
    #[schemars(extend("examples" = ["rabbitmq"]))]
    /// Run this setup entry again whenever the named container has restarted since it last
    /// ran, for data that lives in the container and may be gone, such as a RabbitMQ vhost.
    pub container: Option<String>,
    #[schemars(extend("examples" = [true,false]))]
    /// Count a failed or timed-out command as done instead of failing the start, for
    /// commands whose "already exists" error is fine. Defaults to false.
    pub ignore_errors: Option<Templated<bool>>,
    /// Working directory, relative to the lane's worktree; defaults to the worktree root.
    #[schemars(extend("examples" = ["infra"]))]
    pub cwd: Option<Templated<String>>,
    #[serde(default)]
    #[schemars(extend("examples" = [{"AWS_DEFAULT_REGION":"us-east-1"}]))]
    /// Variables for this command only.
    pub env: Env,
    #[schemars(extend("examples" = ["15s","10m"]))]
    /// Longest the command may run. When it runs out, the command is stopped (SIGTERM,
    /// then SIGKILL after 10 seconds) and counts as failed, so a setup entry fails the
    /// start unless `ignore_errors` is set. No limit by default.
    pub timeout: Option<Templated<ConfigDuration>>,
}

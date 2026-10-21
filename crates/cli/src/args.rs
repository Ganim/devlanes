use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};
use devlanes_proto::{Context, ExecParams, LogQuery, Method, Request, Selection};
use serde_json::json;

use crate::{Error, OutputMode, Runtime};

/// Heading for flags that every command accepts.
const GLOBAL: &str = "Global options";

const LONG_ABOUT: &str = "\
Run a dev environment per git worktree, side by side.

Every worktree of a project is a lane with its own ports, hostnames such as
web.acme-2.localhost and environment, so branches run at the same time. A
per-user daemon owns the processes and containers and keeps them running when
the terminal closes; this command and the TUI are its clients. Without a
command, devlanes opens the TUI on a terminal and prints status otherwise.";

const AFTER_HELP: &str = "\
Examples:
  devlanes start                                bring up this worktree's lane
  devlanes status                               lanes and units of this project
  devlanes logs acme@2:api --tail 200           last 200 lines of api in lane 2
  devlanes with --profile e2e -- pnpm test:e2e  start, run the tests, stop
  devlanes stop --mine                          stop what this agent session started

References:
  A lane is [project@]lane: a lane number (2), a branch (feat/checkout), a
  lane or worktree directory name, or a path. Without project@ the project of
  the current directory is used.
  A unit is [lane:]unit, such as api, 2:api or acme@feat/checkout:web.
  Commands that take several units also accept globs (\"api-*\") and tags (+web).";

const START_AFTER: &str = "\
Examples:
  devlanes start                       the whole lane of this worktree
  devlanes start api web               only these services
  devlanes start --profile e2e         the units of the e2e profile
  devlanes start 2:+api --timeout 5m   units tagged api in lane 2";

const LOGS_AFTER: &str = "\
Examples:
  devlanes logs api --tail 200
  devlanes logs acme@2:api --since 5m --grep 'timeout|refused'
  devlanes logs api -f --timeout 30s
  devlanes logs --all --since start";

const WITH_AFTER: &str = "\
Examples:
  devlanes with -- pnpm test
  devlanes with --profile e2e -- pnpm test:e2e
  devlanes with api --logs-dir devlanes-logs -- cargo test --test api";

/// Public parser for the binary and shell completion frontend.
#[derive(Debug, Parser)]
#[command(
    name = "devlanes",
    version,
    about = "Run a dev environment per git worktree, side by side",
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    /// Print JSON: one document, or NDJSON for streams (same as --output json)
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub json: bool,
    /// Output mode [default: agent when an AI agent runs devlanes, plain in CI or without a terminal, else human]
    #[arg(long, global = true, value_enum, value_name = "MODE", help_heading = GLOBAL)]
    pub output: Option<OutputMode>,
    /// Use this project instead of the one containing the current directory
    #[arg(long, global = true, value_name = "PROJECT", help_heading = GLOBAL)]
    pub project: Option<String>,
    /// Use this lane ([project@]lane) instead of the current worktree's
    #[arg(long, global = true, value_name = "LANE", help_heading = GLOBAL)]
    pub lane: Option<String>,
    /// Confirm actions that would ask first; needed when no terminal can prompt
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub yes: bool,
    /// Print nothing on success; errors still go to stderr
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub quiet: bool,
    /// Set a variable for started processes (start, restart, run TASK, exec); repeatable
    #[arg(long = "env", global = true, value_parser = parse_env, value_name = "KEY=VALUE", help_heading = GLOBAL)]
    pub env: Vec<(String, String)>,
    /// Limit waits and streams, such as 30s, 5m or 1h [default for start and wait: 180s]
    #[arg(long, global = true, value_parser = parse_duration, value_name = "DURATION", help_heading = GLOBAL)]
    pub timeout: Option<Duration>,
    /// Cap agent-mode output at this many bytes [default: 16384, minimum: 256]
    #[arg(long, global = true, value_parser = parse_budget, value_name = "BYTES", help_heading = GLOBAL)]
    pub budget: Option<usize>,
}

/// Commands resolve references in the daemon, including branch names and globs.
/// Declaration order is help order: daily use, lanes and infra, setup, remote.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Bring up a lane: infra, setup, tasks and services, then wait until ready
    #[command(
        long_about = "Bring up a lane: infra, setup, tasks and services, then wait until ready.\n\n\
Starts the lane of the current worktree, or only the named units. Units that\n\
already run are left alone, so starting twice is safe. Waits\n\
until every started unit is ready or one fails; Ctrl-C detaches and leaves\n\
everything running.",
        after_help = START_AFTER
    )]
    Start(StartArgs),
    /// Stop units, or the whole lane when none are named
    Stop(StopArgs),
    /// Stop and start units again with a fresh environment
    Restart(RestartArgs),
    /// Show lanes and units with state, ports, URLs, CPU and memory
    Status(StatusArgs),
    /// Read unit logs; each line carries a cursor to continue from
    #[command(
        long_about = "Read unit logs; each line carries a cursor to continue from.\n\n\
Lines of several units are merged by time. Each line starts with a cursor such\n\
as api#120 that --cursor and --around accept. In agent mode and without a\n\
terminal the output defaults to the last 100 lines within the output budget.",
        after_help = LOGS_AFTER
    )]
    Logs(LogArgs),
    /// Wait until a unit or lane is ready or failed
    #[command(long_about = "Wait until a unit or lane is ready or failed.\n\n\
Exits 0 when ready, 1 when it failed (with the reason and log tail) and 124\n\
when --timeout passes first [default: 180s].")]
    Wait {
        /// Unit ([lane:]unit) or lane ([project@]lane) to wait for
        #[arg(value_name = "TARGET")]
        target: String,
    },
    /// Start the lane, run a command against it, then stop what was started
    #[command(
        long_about = "Start the lane, run a command against it, then stop what was started.\n\n\
Waits for ready first; when a unit fails it prints the failure and exits 1\n\
without running the command. The command gets the lane's environment, including\n\
DEVLANES_PUBLIC_URL_<SERVICE> for every service. Afterwards devlanes writes\n\
--logs-dir, prints peak CPU and memory, stops what it started (and the daemon,\n\
if it started it) and exits with the command's exit code.",
        after_help = WITH_AFTER
    )]
    With(WithArgs),
    /// Run a task from devlanes.yaml and exit with its exit code
    #[command(
        long_about = "Run a task from devlanes.yaml and exit with its exit code.\n\n\
The task's output streams to this terminal. Ctrl-C asks before stopping a\n\
foreground task on a terminal and stops it in agent mode. Tasks with confirm:\n\
need --yes when no terminal can prompt."
    )]
    Run(RunArgs),
    /// Run an ad-hoc command as a unit of the lane instead of backgrounding it with &
    #[command(
        long_about = "Run an ad-hoc command as a unit of the lane instead of backgrounding it with &.\n\n\
The daemon owns the process: logs, stop, attach and status work on it by its\n\
--name, and it gets the lane's environment."
    )]
    Exec(ExecArgs),
    /// Attach this terminal to a unit's terminal; C-a d detaches
    Attach {
        /// Unit to attach to ([lane:]unit)
        #[arg(value_name = "UNIT")]
        unit: String,
    },
    /// Open the TUI; closing it never stops anything
    Ui,
    /// Create, list, clean and resume worktree lanes
    Lane {
        #[command(subcommand)]
        command: LaneCommand,
    },
    /// Explain failures: why a unit is not up, errors, events and resource use
    Debug {
        #[command(subcommand)]
        command: DebugCommand,
    },
    /// Show, export or enter the environment of a lane or unit
    Env {
        #[command(subcommand)]
        command: EnvCommand,
    },
    /// Print, open and list service URLs and routed hostnames
    Url {
        #[command(subcommand)]
        command: UrlCommand,
    },
    /// Start, stop and inspect the project's containers
    Infra {
        #[command(subcommand)]
        command: InfraCommand,
    },
    /// Validate devlanes.yaml
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Trust the devlanes CA and install the /etc/hosts helper
    Certs {
        #[command(subcommand)]
        command: CertsCommand,
    },
    /// Hand secrets from fnox to the daemon
    Secrets {
        #[command(subcommand)]
        command: SecretsCommand,
    },
    /// Connect to boxes over SSH and mirror their lanes here
    Remote {
        #[command(subcommand)]
        command: RemoteCommand,
    },
    /// Run, stop and install the background daemon
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    /// Check the daemon, Docker, proxy, CA, hosts file and output mode, each with a fix
    Doctor,
    /// Print the short guide for AI agents that use devlanes
    AgentGuide,
    /// Print shell completions; lane and unit names come from the running daemon
    Completions {
        /// Shell to generate completions for
        #[arg(value_name = "SHELL")]
        shell: clap_complete::Shell,
    },
    /// Serve a client's SSH connection on a box (started by remote connect)
    #[command(hide = true)]
    Tunnel {
        /// Speak the mux protocol on stdin and stdout
        #[arg(long, required = true)]
        stdio: bool,
    },
    /// Answer an ssh prompt through the connected client (SSH_ASKPASS)
    #[command(hide = true)]
    Askpass {
        /// Prompt text from ssh
        #[arg(value_name = "PROMPT")]
        prompt: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct StartArgs {
    /// Units to start ([lane:]unit, globs, +tag) [default: the whole lane]
    #[arg(value_name = "UNIT")]
    pub units: Vec<String>,
    /// Start the units of this profile from devlanes.yaml; repeat to combine profiles (later ones win on env)
    #[arg(long = "profile", value_name = "PROFILE")]
    pub profiles: Vec<String>,
    /// Open the TUI once the lane is up
    #[arg(long)]
    pub attach: bool,
    /// Start every selected service immediately, bypassing the machine-wide start gate for this run
    #[arg(long)]
    pub all_at_once: bool,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    /// Units to stop ([lane:]unit, globs, +tag) [default: the whole lane]
    #[arg(value_name = "UNIT")]
    pub units: Vec<String>,
    /// Only stop units this agent session started; with no units, across every lane
    #[arg(long)]
    pub mine: bool,
    /// Write each unit's log of this run to DIR as text files, for CI artifacts
    #[arg(long, value_name = "DIR")]
    pub logs_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct WithArgs {
    /// Units to start ([lane:]unit, globs, +tag) [default: the whole lane]
    #[arg(value_name = "UNIT")]
    pub units: Vec<String>,
    /// Start the units of this profile from devlanes.yaml; repeat to combine profiles (later ones win on env)
    #[arg(long = "profile", value_name = "PROFILE")]
    pub profiles: Vec<String>,
    /// Write every unit's log of the run to DIR as text files, for CI artifacts
    #[arg(long, value_name = "DIR")]
    pub logs_dir: Option<PathBuf>,
    /// Leave the lane running afterwards, for debugging
    #[arg(long)]
    pub keep: bool,
    /// Start every selected service immediately, bypassing the machine-wide start gate for this run
    #[arg(long)]
    pub all_at_once: bool,
    /// Command and arguments to run once the lane is ready
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RestartArgs {
    /// Units to restart ([lane:]unit, globs, +tag)
    #[arg(required = true, value_name = "UNIT")]
    pub units: Vec<String>,
    /// Start every selected service immediately, bypassing the machine-wide start gate for this run
    #[arg(long)]
    pub all_at_once: bool,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    // Own id: `lane` would collide with the global `--lane` and reject `status --lane X`.
    /// Lane to show ([project@]lane) [default: every lane of the current project]
    #[arg(id = "status_lane", value_name = "LANE")]
    pub lane: Option<String>,
    /// Keep printing lane changes until interrupted
    #[arg(long)]
    pub watch: bool,
    /// Only show units this agent session started
    #[arg(long)]
    pub mine: bool,
}

#[derive(Debug, Args)]
pub struct LogArgs {
    /// Units to read ([lane:]unit, globs, +tag)
    #[arg(required_unless_present_any = ["mine", "all"], value_name = "UNIT")]
    pub units: Vec<String>,
    /// Read every unit of the lane, grouped by unit
    #[arg(long, conflicts_with_all = ["units", "mine"])]
    pub all: bool,
    /// Show the last N lines [default: 100 in agent mode or without a terminal]
    #[arg(long, value_name = "N")]
    pub tail: Option<usize>,
    /// Keep streaming new lines; agent mode requires --timeout
    #[arg(short, long)]
    pub follow: bool,
    /// Only lines newer than a duration (5m), a time in milliseconds, or start (the lane's last start)
    #[arg(long, value_name = "WHEN")]
    pub since: Option<String>,
    /// Only lines matching this regular expression
    #[arg(long, value_name = "REGEX")]
    pub grep: Option<String>,
    /// Only lines at this level or above: debug, info, warn or error
    #[arg(long, value_name = "LEVEL")]
    pub level: Option<String>,
    /// Show the lines around this cursor, such as api#120
    #[arg(long, value_name = "CURSOR")]
    pub around: Option<String>,
    /// Lines before and after --around
    #[arg(short = 'C', long = "context", default_value_t = 3, value_name = "N")]
    pub context_lines: usize,
    /// Continue after this cursor, as printed by an earlier logs call
    #[arg(long, value_name = "CURSOR")]
    pub cursor: Option<String>,
    /// Only units this agent session started
    #[arg(long)]
    pub mine: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    /// Task to run, as named under tasks: in devlanes.yaml
    #[arg(
        required_unless_present = "list",
        conflicts_with = "list",
        value_name = "TASK"
    )]
    pub task: Option<String>,
    /// List the tasks and the script sources they use
    #[arg(long)]
    pub list: bool,
    /// Start the task in the background and return
    #[arg(long, conflicts_with = "list")]
    pub detach: bool,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    /// Unit name for the command, used by logs, stop and attach
    #[arg(long, value_name = "NAME")]
    pub name: String,
    /// Use pipes instead of a pseudoterminal
    #[arg(long)]
    pub no_pty: bool,
    /// Command and arguments to run
    #[arg(last = true, required = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Debug, Subcommand)]
pub enum DebugCommand {
    /// Explain why a unit is not running or not ready, with a fix for each reason
    Why {
        /// Unit to explain ([lane:]unit)
        #[arg(value_name = "UNIT")]
        unit: String,
    },
    /// List failed units, crash counts, recent error lines and port conflicts
    Errors {
        /// Count crashes and error lines from this long ago, such as 5m or 2h [default: all recorded runs]
        #[arg(long, value_name = "DURATION")]
        since: Option<String>,
    },
    /// Print the daemon's event stream: lane, unit, task and port events
    Events {
        /// Keep streaming until interrupted [default: print recent events]
        #[arg(short, long)]
        follow: bool,
    },
    /// Show CPU and memory per lane and unit
    Top,
}

#[derive(Debug, Subcommand)]
pub enum EnvCommand {
    /// Print the environment of the lane or of one unit as KEY=VALUE lines
    Show {
        /// Unit whose environment to print ([lane:]unit) [default: the lane's variables]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
        /// Show where each variable comes from; secret values are hidden
        #[arg(long)]
        explain: bool,
    },
    /// Print export KEY=VALUE lines for eval in a shell
    Export {
        /// Unit whose environment to export ([lane:]unit) [default: the lane's variables]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
    },
    /// Open a subshell with the lane's or one unit's environment
    Shell {
        /// Unit whose environment to use ([lane:]unit) [default: the lane's variables]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum UrlCommand {
    /// Print a service's URL, for scripts
    Print {
        /// Service whose URL to print ([lane:]unit)
        #[arg(value_name = "UNIT")]
        unit: String,
    },
    /// Open a service's URL in the browser; agent mode only prints it
    Open {
        /// Service to open ([lane:]unit)
        #[arg(value_name = "UNIT")]
        unit: String,
    },
    /// List routed hostnames with their lane, unit, port and state
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum CertsCommand {
    /// Trust the devlanes CA in the system and browser stores
    Trust {
        /// Also install the /etc/hosts helper for programs that cannot resolve *.localhost
        #[arg(long)]
        hosts: bool,
        /// Also send lookups for hostnames.tld to the devlanes DNS server (/etc/resolver on
        /// macOS, systemd-resolved on Linux)
        #[arg(long)]
        resolver: bool,
    },
    /// Remove the devlanes CA from the trust stores
    #[command(group(clap::ArgGroup::new("removal").args(["hosts", "resolver"]).multiple(true)))]
    Untrust {
        /// Also remove the /etc/hosts helper, its sudo rule and the devlanes block in /etc/hosts
        #[arg(long)]
        hosts: bool,
        /// Also remove the OS resolver file for hostnames.tld
        #[arg(long)]
        resolver: bool,
        /// Keep the CA trusted; with --hosts or --resolver, remove only those
        #[arg(long, requires = "removal")]
        keep_ca: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum SecretsCommand {
    /// Read secrets with fnox in this terminal and hand them to the daemon
    Unlock,
}

#[derive(Debug, Subcommand)]
pub enum RemoteCommand {
    /// Connect to a box over SSH and mirror its lanes, ports and hostnames here
    Connect {
        /// SSH destination, as in ~/.ssh/config
        #[arg(id = "connect_host", value_name = "HOST")]
        host: String,
        /// Return once connected instead of staying attached for prompts
        #[arg(long)]
        background: bool,
        /// If devlanes is missing on the box, install this machine's release there
        /// (HTTPS download, checksum verified, never replaces an existing binary)
        #[arg(long)]
        install: bool,
    },
    /// Disconnect from a box and remove its mirrored ports and hostnames
    Disconnect {
        /// Box to disconnect [default: every box]
        #[arg(id = "disconnect_host", value_name = "HOST")]
        host: Option<String>,
    },
    /// List known boxes and their connection state
    Ls,
    /// List the synced folders of running lanes
    Sync,
}

#[derive(Debug, Subcommand)]
pub enum LaneCommand {
    /// List every worktree lane, including moved, unassigned and missing ones
    Ls,
    /// Create a git worktree for a branch under lanes.worktree_dir and register its lane
    New {
        /// Branch to check out in the new worktree
        #[arg(value_name = "BRANCH")]
        branch: String,
        /// Start the new lane right away
        #[arg(long)]
        start: bool,
    },
    /// Stop a lane, run its clean tasks and teardown, and free its ports (needs --yes)
    Clean {
        // Own id: `lane` would collide with the global `--lane`.
        /// Lane to clean ([project@]lane)
        #[arg(id = "clean_lane", value_name = "LANE")]
        lane: String,
    },
    /// Clean lanes whose worktrees are gone and drop unused port offsets (needs --yes)
    Gc,
    /// Start the lanes that were running before a reboot or daemon restart
    #[command(
        long_about = "Start the lanes that were running before a reboot or daemon restart.\n\n\
The daemon never stores a shell's environment, so after a restart each lane\n\
needs one start or resume from a shell; resume uses this shell's environment\n\
for every lane whose last request was start."
    )]
    Resume {
        /// Start every selected service immediately, bypassing the machine-wide start gate for this run
        #[arg(long)]
        all_at_once: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum InfraCommand {
    /// Start shared and per-lane containers and wait until they are healthy
    Up,
    /// Stop the project's shared containers; their data is kept
    Down,
    /// Show the containers' health and ports
    Status,
}
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    /// Validate this worktree's devlanes.yaml, including preset options
    Check,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Show the daemon's version, pid, uptime and managed process count
    Status,
    /// Stop the daemon and every process it manages
    Stop,
    /// Restart the daemon; asks first when processes are running
    Restart,
    /// Print the daemon's own log file
    Logs {
        /// Keep printing new lines
        #[arg(short, long)]
        follow: bool,
        /// Lines to show [default: 100]
        #[arg(long, value_name = "N")]
        tail: Option<usize>,
    },
    /// Install a login service so the daemon starts at login and outside agent sandboxes
    Install,
    /// Remove the login service; a running daemon keeps running
    Uninstall,
    /// Stop everything and empty the state database; lanes get new numbers and ports (needs --yes)
    ResetState,
    /// Run the daemon in this process (spawned by the client)
    #[command(hide = true)]
    Run,
}

/// Work for the composition root; no variant claims a side effect has happened.
pub enum Action {
    With(crate::WithPlan),
    StopWithLogs {
        request: Request,
        logs_dir: PathBuf,
    },
    Rpc(Request),
    RpcThenUi(Request),
    Attach(Request),
    Open(Request),
    ExportEnv(Request),
    Shell(Request),
    Ui,
    Trust {
        hosts: bool,
        resolver: bool,
    },
    Untrust {
        hosts: bool,
        resolver: bool,
        keep_ca: bool,
    },
    Unlock(Context),
    DaemonRestart(Context),
    DaemonLogs {
        follow: bool,
        tail: Option<usize>,
    },
    DaemonInstall,
    DaemonUninstall,
    DaemonRun,
    AgentGuide,
    Completions(clap_complete::Shell),
    Tunnel,
    Askpass(Option<String>),
}

/// Client interruption policy never implicitly stops services.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptPolicy {
    Detach,
    ConfirmTaskStop,
    StopTask,
    ForwardCommandThenTeardown,
}

impl Cli {
    /// Captures cwd and captures environment only for process-starting methods.
    pub fn action(&self, runtime: &Runtime) -> Result<Action, Error> {
        self.action_with(runtime, std::env::current_dir()?, || {
            std::env::vars_os()
                .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
                .collect()
        })
    }

    /// Builds requests using injected context without touching global environment in tests.
    pub fn action_with(
        &self,
        runtime: &Runtime,
        cwd: PathBuf,
        environment: impl FnOnce() -> BTreeMap<String, String>,
    ) -> Result<Action, Error> {
        if !self.env.is_empty()
            && !(matches!(
                &self.command,
                Some(Command::Start(_) | Command::Restart(_) | Command::Exec(_))
            ) || matches!(&self.command, Some(Command::Run(args)) if !args.list))
        {
            return Err(Error::Usage(
                "--env is supported only by start, restart, run TASK, and exec".into(),
            ));
        }
        let mut context = Context {
            cwd,
            actor: runtime.actor.clone(),
            project: self.project.clone(),
            lane: self.lane.clone(),
            yes: self.yes,
            env: None,
        };
        let timeout_ms = self
            .timeout
            .map(|v| u64::try_from(v.as_millis()).unwrap_or(u64::MAX));
        let selection =
            |units: &[String], env: &[(String, String)], profiles: &[String], mine, all_at_once| {
                serde_json::to_value(Selection {
                    units: units.to_vec(),
                    env: env.iter().cloned().collect(),
                    profiles: profiles.to_vec(),
                    mine,
                    timeout_ms,
                    all_at_once,
                    ..Selection::default()
                })
            };
        let (method, params) = match &self.command {
            None if runtime.mode == OutputMode::Human
                && runtime.stdout_tty
                && runtime.ci.is_none() =>
            {
                return Ok(Action::Ui);
            }
            None => (Method::Status, json!({"mine": false})),
            Some(Command::Ui) => return Ok(Action::Ui),
            Some(Command::With(args)) => {
                context.env = Some(environment());
                let params = serde_json::to_value(Selection {
                    units: args.units.clone(),
                    profiles: args.profiles.clone(),
                    timeout_ms: Some(timeout_ms.unwrap_or(180_000)),
                    all_at_once: args.all_at_once,
                    ..Selection::default()
                })?;
                return Ok(Action::With(crate::WithPlan {
                    start: Request {
                        id: 0,
                        method: Method::Start,
                        params,
                        context,
                    },
                    command: args.command.clone(),
                    logs_dir: args.logs_dir.clone(),
                    keep: args.keep,
                }));
            }
            Some(Command::Start(args)) => {
                let mut params = selection(
                    &args.units,
                    &self.env,
                    &args.profiles,
                    false,
                    args.all_at_once,
                )?;
                params["timeout_ms"] = json!(timeout_ms.unwrap_or(180_000));
                (Method::Start, params)
            }
            Some(Command::Stop(args)) => (
                Method::Stop,
                selection(&args.units, &[], &[], args.mine, false)?,
            ),
            Some(Command::Restart(args)) => (
                Method::Restart,
                selection(&args.units, &self.env, &[], false, args.all_at_once)?,
            ),
            Some(Command::Status(args)) => {
                if runtime.mode == OutputMode::Agent && args.watch && self.timeout.is_none() {
                    return Err(Error::Usage(
                        "agent status --watch requires --timeout (for example --timeout 30s)"
                            .into(),
                    ));
                }
                if args.lane.is_some() {
                    context.lane.clone_from(&args.lane);
                }
                (
                    Method::Status,
                    json!({"mine": args.mine, "watch": args.watch}),
                )
            }
            Some(Command::Logs(args)) => {
                if runtime.mode == OutputMode::Agent && args.follow && self.timeout.is_none() {
                    return Err(Error::Usage(
                        "agent logs --follow requires --timeout (for example --timeout 30s)".into(),
                    ));
                }
                let bounded = runtime.mode == OutputMode::Agent || !runtime.stdout_tty;
                (
                    Method::Logs,
                    serde_json::to_value(crate::LaneLogQuery {
                        all: args.all,
                        query: LogQuery {
                            units: args.units.clone(),
                            tail: args.tail.or(bounded.then_some(100)),
                            max_bytes: self.budget.or(bounded.then_some(crate::DEFAULT_BUDGET)),
                            cursor: args.cursor.clone(),
                            since: args.since.clone(),
                            grep: args.grep.clone(),
                            level: args.level.clone(),
                            around: args.around.clone(),
                            context_lines: args.context_lines,
                            follow: args.follow,
                            mine: args.mine,
                        },
                    })?,
                )
            }
            Some(Command::Wait { target }) => (
                Method::Wait,
                json!({"target": target, "timeout_ms": timeout_ms}),
            ),
            Some(Command::Debug { command }) => match command {
                DebugCommand::Why { unit } => (Method::Why, json!({"unit": unit})),
                DebugCommand::Errors { since } => (Method::Errors, json!({"since": since})),
                DebugCommand::Events { follow } => {
                    if runtime.mode == OutputMode::Agent && *follow && self.timeout.is_none() {
                        return Err(Error::Usage(
                            "agent debug events --follow requires --timeout (for example --timeout 30s)"
                                .into(),
                        ));
                    }
                    (Method::Subscribe, json!({"follow": follow}))
                }
                DebugCommand::Top => (Method::Metrics, json!({})),
            },
            Some(Command::Run(args)) if args.list => (Method::TaskList, json!({})),
            Some(Command::Run(args)) => (
                Method::Run,
                json!({
                    "task": args.task, "detach": args.detach, "env": self.env.iter().cloned().collect::<BTreeMap<_,_>>(), "timeout_ms": timeout_ms
                }),
            ),
            Some(Command::Exec(args)) => (
                Method::Exec,
                serde_json::to_value(ExecParams {
                    name: args.name.clone(),
                    command: args.command.clone(),
                    env: self.env.iter().cloned().collect(),
                    pty: !args.no_pty,
                })?,
            ),
            Some(Command::Attach { unit }) => (Method::Attach, json!({"unit": unit})),
            Some(Command::Url { command }) => match command {
                UrlCommand::Print { unit } | UrlCommand::Open { unit } => {
                    (Method::Url, json!({"unit": unit}))
                }
                UrlCommand::Ls => (Method::Hosts, json!({})),
            },
            Some(Command::Env { command }) => match command {
                EnvCommand::Show { unit, explain } => {
                    (Method::Env, json!({"unit": unit, "explain": explain}))
                }
                EnvCommand::Export { unit } | EnvCommand::Shell { unit } => {
                    (Method::Env, json!({"unit": unit}))
                }
            },
            Some(Command::Lane { command }) => match command {
                LaneCommand::Resume { all_at_once } => (
                    Method::Resume,
                    json!({"timeout_ms": timeout_ms, "all_at_once": all_at_once}),
                ),
                LaneCommand::Ls => (Method::LaneList, json!({})),
                LaneCommand::New { branch, start } => {
                    (Method::LaneNew, json!({"branch": branch, "start": start}))
                }
                LaneCommand::Clean { lane } => {
                    context.lane = Some(lane.clone());
                    (Method::LaneClean, json!({"lane": lane}))
                }
                LaneCommand::Gc => (Method::LaneGc, json!({})),
            },
            Some(Command::Infra { command }) => (
                match command {
                    InfraCommand::Up => Method::InfraUp,
                    InfraCommand::Down => Method::InfraDown,
                    InfraCommand::Status => Method::InfraStatus,
                },
                json!({}),
            ),
            Some(Command::Config { .. }) => (Method::ConfigCheck, json!({})),
            Some(Command::Doctor) => (Method::Doctor, json!({})),
            Some(Command::Certs { command }) => match command {
                CertsCommand::Trust { hosts, resolver } => {
                    return Ok(Action::Trust {
                        hosts: *hosts,
                        resolver: *resolver,
                    });
                }
                CertsCommand::Untrust {
                    hosts,
                    resolver,
                    keep_ca,
                } => {
                    return Ok(Action::Untrust {
                        hosts: *hosts,
                        resolver: *resolver,
                        keep_ca: *keep_ca,
                    });
                }
            },
            Some(Command::Secrets {
                command: SecretsCommand::Unlock,
            }) => return Ok(Action::Unlock(context)),
            Some(Command::Daemon { command }) => match command {
                DaemonCommand::Status => (Method::DaemonStatus, json!({})),
                DaemonCommand::Stop => (Method::DaemonShutdown, json!({})),
                DaemonCommand::ResetState => (Method::DaemonResetState, json!({})),
                DaemonCommand::Restart => return Ok(Action::DaemonRestart(context)),
                DaemonCommand::Logs { follow, tail } => {
                    return Ok(Action::DaemonLogs {
                        follow: *follow,
                        tail: *tail,
                    });
                }
                DaemonCommand::Install => return Ok(Action::DaemonInstall),
                DaemonCommand::Uninstall => return Ok(Action::DaemonUninstall),
                DaemonCommand::Run => return Ok(Action::DaemonRun),
            },
            Some(Command::Remote { command }) => match command {
                RemoteCommand::Connect {
                    host,
                    background,
                    install,
                } => (
                    Method::Connect,
                    json!({"host": host, "background": background, "install": install}),
                ),
                RemoteCommand::Disconnect { host } => (Method::Disconnect, json!({"host": host})),
                RemoteCommand::Ls => (Method::HostList, json!({})),
                RemoteCommand::Sync => (Method::SyncStatus, json!({})),
            },
            Some(Command::AgentGuide) => return Ok(Action::AgentGuide),
            Some(Command::Completions { shell }) => return Ok(Action::Completions(*shell)),
            Some(Command::Tunnel { .. }) => return Ok(Action::Tunnel),
            Some(Command::Askpass { prompt }) => return Ok(Action::Askpass(prompt.clone())),
        };
        if matches!(
            method,
            Method::Start | Method::Restart | Method::Run | Method::Exec | Method::Resume
        ) {
            context.env = Some(environment());
        }
        let request = Request {
            id: 0,
            method,
            params,
            context,
        };
        Ok(match &self.command {
            Some(Command::Stop(args)) if args.logs_dir.is_some() => Action::StopWithLogs {
                request,
                logs_dir: args.logs_dir.clone().unwrap_or_default(),
            },
            // Agents get the start result only; the TUI needs a person at a terminal.
            Some(Command::Start(args)) if args.attach && runtime.mode != OutputMode::Agent => {
                Action::RpcThenUi(request)
            }
            Some(Command::Attach { unit })
                if runtime.mode == OutputMode::Agent && !runtime.stdin_tty =>
            {
                return Err(Error::Usage(format!(
                    "agent attach needs a terminal; read the output with {}",
                    crate::Hint::command(["logs", unit.as_str(), "-f", "--timeout", "30s"]).0
                )));
            }
            Some(Command::Attach { .. }) => Action::Attach(request),
            Some(Command::Url {
                command: UrlCommand::Open { .. },
            }) if runtime.mode != OutputMode::Agent => Action::Open(request),
            Some(Command::Env {
                command: EnvCommand::Export { .. },
            }) => Action::ExportEnv(request),
            Some(Command::Env {
                command: EnvCommand::Shell { .. },
            }) => Action::Shell(request),
            _ => Action::Rpc(request),
        })
    }

    /// Foreground tasks and with-owned commands have explicit interruption policies.
    pub fn interrupt_policy(&self, runtime: &Runtime) -> InterruptPolicy {
        if matches!(&self.command, Some(Command::With(_))) {
            return InterruptPolicy::ForwardCommandThenTeardown;
        }
        if matches!(&self.command, Some(Command::Run(args)) if !args.detach && !args.list) {
            if runtime.mode == OutputMode::Agent {
                return InterruptPolicy::StopTask;
            }
            if runtime.stdin_tty {
                return InterruptPolicy::ConfirmTaskStop;
            }
        }
        InterruptPolicy::Detach
    }

    /// Identifies commands whose JSON output is an NDJSON stream.
    pub fn streams(&self) -> bool {
        matches!(
            &self.command,
            Some(
                Command::Start(_)
                    | Command::With(_)
                    | Command::Restart(_)
                    | Command::Wait { .. }
                    | Command::Debug {
                        command: DebugCommand::Events { .. }
                    }
            )
        ) || matches!(&self.command, Some(Command::Run(args)) if !args.list && !args.detach)
            || matches!(&self.command, Some(Command::Logs(args)) if args.follow)
            || matches!(&self.command, Some(Command::Status(args)) if args.watch)
    }
}

/// Validates assignment names before building an explicit override map.
fn parse_env(value: &str) -> Result<(String, String), String> {
    let (key, value) = value.split_once('=').ok_or("expected KEY=VALUE")?;
    if key.is_empty() || key.contains('\0') || value.contains('\0') {
        return Err("environment assignment contains an empty name or NUL".into());
    }
    Ok((key.into(), value.into()))
}

/// Accepts milliseconds, seconds, minutes, or hours; bare numbers mean seconds.
fn parse_duration(value: &str) -> Result<Duration, String> {
    let (number, scale) = if let Some(n) = value.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = value.strip_suffix('s') {
        (n, 1_000)
    } else if let Some(n) = value.strip_suffix('m') {
        (n, 60_000)
    } else if let Some(n) = value.strip_suffix('h') {
        (n, 3_600_000)
    } else {
        (value, 1_000)
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0)
        .map(Duration::from_millis)
        .ok_or_else(|| "expected a positive duration, such as 30s or 2m".into())
}

/// Reserves enough output room for an actionable truncation notice.
fn parse_budget(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 256)
        .ok_or_else(|| "budget must be at least 256 bytes".into())
}

use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand, ValueEnum};
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

const MIGRATE_AFTER: &str = "\
Examples:
  devlanes migrate --dry-run           print what would be written
  devlanes migrate --from overmind     only the Procfile setup
  devlanes migrate --force             replace an existing devlanes.yaml";

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
    /// Print timestamped diagnostics of the client and daemon to stderr; -vv for more [env: DEVLANES_VERBOSE=1|2]
    #[arg(short, long, global = true, action = clap::ArgAction::Count, help_heading = GLOBAL)]
    pub verbose: u8,
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
        /// Unit ([lane:]unit) or lane ([project@]lane) to wait for [default: pick from a list]
        #[arg(value_name = "TARGET")]
        target: Option<String>,
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
        /// Unit to attach to ([lane:]unit) [default: pick from a list]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
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
    /// Convert mprocs, dekit, Procfile, pm2 and similar setups into devlanes.yaml
    #[command(
        long_about = "Write devlanes.yaml from an existing process-manager setup.\n\n\
Looks in the repository root (or the current directory outside Git) for mprocs,\n\
dekit, process-compose, Procfile (foreman, overmind, hivemind, honcho), pm2,\n\
tmuxinator, tmuxp, package.json scripts that run concurrently or npm-run-all,\n\
and mise tasks. Several setups are merged when their names do not clash. The\n\
result is checked like devlanes config check before it is written; what could\n\
not be carried over is printed and left as comments in the file. Runs without\n\
the daemon.",
        after_help = MIGRATE_AFTER
    )]
    Migrate(MigrateArgs),
    /// Trust the devlanes CA and install the hosts file writer
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
    /// Check the daemon, Docker, proxy, CA, hosts file and output mode, each with a fix; never starts the daemon
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
    /// Run the start in the TUI even when DEVLANES_NO_UI or tui.open_on_start turn that off [default on a terminal]
    #[arg(long, conflicts_with = "no_ui")]
    pub attach: bool,
    /// Print the start as text instead of running it in the TUI
    #[arg(long)]
    pub no_ui: bool,
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
    /// Units to restart ([lane:]unit, globs, +tag) [default: pick from a list]
    #[arg(value_name = "UNIT")]
    pub units: Vec<String>,
    /// Start every selected service immediately, bypassing the machine-wide start gate for this run
    #[arg(long)]
    pub all_at_once: bool,
    /// Print the restart as text instead of running it in the TUI
    #[arg(long)]
    pub no_ui: bool,
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
    /// Units to read ([lane:]unit, globs, +tag) [default: pick from a list]
    #[arg(value_name = "UNIT")]
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
    /// Task to run, as named under tasks: in devlanes.yaml [default: pick from a list]
    #[arg(conflicts_with = "list", value_name = "TASK")]
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
        /// Unit to explain ([lane:]unit) [default: pick from a list]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
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
    /// Write a redacted archive to attach to a bug report and print its path
    #[command(
        long_about = "Write a redacted archive to attach to a bug report and print its path.\n\n\
The archive holds the client and daemon versions, doctor, config check and status\n\
as JSON, the last lines of the daemon's log and, with -- ARGS, the output of\n\
devlanes -vv ARGS. Values of secret-looking variables, URL passwords and the home\n\
directory are replaced before anything is written; read the files before sending\n\
them anyway.",
        after_help = "Examples:\n  devlanes debug bundle\n  devlanes debug bundle --to bug.tar.gz -- start api"
    )]
    Bundle {
        /// Write the archive here [default: devlanes-debug-<time>.tar.gz in the temporary directory]
        #[arg(long, value_name = "PATH")]
        to: Option<PathBuf>,
        /// Lines of the daemon's log to include
        #[arg(long, value_name = "N", default_value_t = 500)]
        log_lines: usize,
        /// devlanes arguments to run again with -vv and include, such as: -- start api
        #[arg(last = true, value_name = "ARGS")]
        args: Vec<String>,
    },
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
    #[command(
        after_help = "Examples:\n  eval \"$(devlanes env export api)\"\n  devlanes env export api --shell pwsh | Out-String | Invoke-Expression\n\nIn PowerShell, assigning an empty value removes the variable, so variables with empty values end up unset."
    )]
    Export {
        /// Unit whose environment to export ([lane:]unit) [default: the lane's variables]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
        /// Syntax of the printed assignments
        #[arg(long, value_enum, default_value_t = ExportShell::Posix)]
        shell: ExportShell,
    },
    /// Open a subshell with the lane's or one unit's environment
    Shell {
        /// Unit whose environment to use ([lane:]unit) [default: the lane's variables]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
    },
}

/// Shell syntax for `env export`. POSIX is the default on every OS, so scripts
/// behave the same everywhere.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum ExportShell {
    /// `export NAME='value'`, for sh, bash, zsh and Git Bash
    #[default]
    #[value(alias = "sh")]
    Posix,
    /// `$env:NAME = 'value'`, for PowerShell; the script is ASCII, other text is decoded from Base64
    #[value(alias = "powershell")]
    Pwsh,
}

#[derive(Debug, Subcommand)]
pub enum UrlCommand {
    /// Print a service's URL, for scripts
    Print {
        /// Service whose URL to print ([lane:]unit) [default: pick from a list]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
    },
    /// Open a service's URL in the browser; agent mode only prints it
    Open {
        /// Service to open ([lane:]unit) [default: pick from a list]
        #[arg(value_name = "UNIT")]
        unit: Option<String>,
    },
    /// List routed hostnames with their lane, unit, port and state
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum CertsCommand {
    /// Trust the devlanes CA in the system and browser stores
    Trust {
        /// Also install the hosts file writer (a sudo helper on macOS and Linux, a service on Windows) for programs that cannot resolve *.localhost
        #[arg(long)]
        hosts: bool,
        /// Also send lookups for hostnames.tld to the devlanes DNS server (/etc/resolver on
        /// macOS, systemd-resolved on Linux, an NRPT rule on Windows)
        #[arg(long)]
        resolver: bool,
    },
    /// Remove the devlanes CA from the trust stores
    #[command(group(clap::ArgGroup::new("removal").args(["hosts", "resolver"]).multiple(true)))]
    Untrust {
        /// Also remove the hosts file writer (its sudo rule, or the Windows service) and the devlanes block in the hosts file
        #[arg(long)]
        hosts: bool,
        /// Also remove the OS resolver file for hostnames.tld (the NRPT rule on Windows)
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
        /// With --start, print the start as text instead of running it in the TUI
        #[arg(long, requires = "start")]
        no_ui: bool,
    },
    /// Stop a lane, run its clean tasks and teardown, and free its ports (needs --yes)
    Clean {
        // Own id: `lane` would collide with the global `--lane`.
        /// Lane to clean ([project@]lane) [default: pick from a list]
        #[arg(id = "clean_lane", value_name = "LANE")]
        lane: Option<String>,
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
        /// Print the starts as text instead of running them in the TUI
        #[arg(long)]
        no_ui: bool,
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

/// Names `--from` accepts; `devlanes-migrate` owns the list and a binary test keeps the
/// two equal.
pub const MIGRATE_SOURCES: [&str; 16] = [
    "auto",
    "mprocs",
    "dekit",
    "process-compose",
    "procfile",
    "pm2",
    "tmuxinator",
    "tmuxp",
    "package-json",
    "mise",
    "foreman",
    "overmind",
    "hivemind",
    "honcho",
    "concurrently",
    "npm-run-all",
];

#[derive(Debug, Args)]
pub struct MigrateArgs {
    /// Setup to convert; aliases name the tool (overmind is procfile) [default: auto, every setup found]
    #[arg(long, value_name = "SOURCE", value_parser = clap::builder::PossibleValuesParser::new(MIGRATE_SOURCES))]
    pub from: Option<String>,
    /// Print the generated YAML instead of writing it
    #[arg(long)]
    pub dry_run: bool,
    /// Replace an existing devlanes.yaml
    #[arg(long)]
    pub force: bool,
    /// Write to this file instead of devlanes.yaml next to the setup
    #[arg(long, value_name = "PATH")]
    pub to: Option<PathBuf>,
}

/// `devlanes migrate`, resolved: the flags and the directory it ran in.
#[derive(Debug, Clone)]
pub struct MigratePlan {
    pub from: Option<String>,
    pub dry_run: bool,
    pub force: bool,
    pub to: Option<PathBuf>,
    pub cwd: PathBuf,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Show the daemon's version, pid, uptime and managed process count; never starts it
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
    /// Start the daemon at login (launchd agent, systemd user unit, or a Windows logon entry)
    Install,
    /// Stop starting the daemon at login; a running daemon keeps running
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
    /// A start that runs inside the TUI: the request is sent first, and the TUI
    /// opens once the daemon accepted it. `forced` (`start --attach`) overrides
    /// `tui.open_on_start: false` in the user config.
    StartUi {
        request: Request,
        forced: bool,
    },
    Attach(Request),
    Open(Request),
    ExportEnv {
        request: Request,
        shell: ExportShell,
    },
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
    /// `devlanes migrate`; handled in the client without the daemon.
    Migrate(MigratePlan),
    /// `devlanes debug bundle`: never spawns a daemon.
    DebugBundle {
        to: Option<PathBuf>,
        log_lines: usize,
        args: Vec<String>,
        context: Context,
    },
    /// The command lacks an argument the daemon can list choices for; `context`
    /// says where to look (cwd, `--lane`, `--project`).
    Choose {
        choice: crate::Choice,
        context: Context,
    },
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
            // Names keep the spelling the OS reports (`Path` on Windows); the
            // daemon matches them case-insensitively there when it layers env.
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
            verbose: runtime.verbose,
        };
        if let Some(choice) = self.missing_choice() {
            return Ok(Action::Choose { choice, context });
        }
        let timeout_ms = self
            .timeout
            .map(|v| u64::try_from(v.as_millis()).unwrap_or(u64::MAX));
        // Starts stream their lane events for phase lines, NDJSON and CI groups;
        // agent output prints none of them. `wait` streams them only for the bar.
        let start_events = runtime.mode != OutputMode::Agent;
        let wait_events =
            runtime.mode == OutputMode::Human && runtime.stdout_tty && runtime.ci.is_none();
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
                    events: start_events,
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
                if start_events {
                    params["events"] = json!(true);
                }
                (Method::Start, params)
            }
            Some(Command::Stop(args)) => (
                Method::Stop,
                selection(&args.units, &[], &[], args.mine, false)?,
            ),
            Some(Command::Restart(args)) => {
                let mut params = selection(&args.units, &self.env, &[], false, args.all_at_once)?;
                if start_events {
                    params["events"] = json!(true);
                }
                (Method::Restart, params)
            }
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
                json!({"target": target, "timeout_ms": timeout_ms, "events": wait_events}),
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
                DebugCommand::Bundle {
                    to,
                    log_lines,
                    args,
                } => {
                    return Ok(Action::DebugBundle {
                        to: to.clone(),
                        log_lines: *log_lines,
                        args: args.clone(),
                        context,
                    });
                }
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
                EnvCommand::Export { unit, .. } | EnvCommand::Shell { unit } => {
                    (Method::Env, json!({"unit": unit}))
                }
            },
            Some(Command::Lane { command }) => match command {
                LaneCommand::Resume { all_at_once, .. } => (
                    Method::Resume,
                    json!({"timeout_ms": timeout_ms, "all_at_once": all_at_once, "events": start_events}),
                ),
                LaneCommand::Ls => (Method::LaneList, json!({})),
                LaneCommand::New { branch, start, .. } => (
                    Method::LaneNew,
                    json!({"branch": branch, "start": start, "events": *start && start_events}),
                ),
                LaneCommand::Clean { lane } => {
                    context.lane.clone_from(lane);
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
            Some(Command::Migrate(args)) => {
                return Ok(Action::Migrate(MigratePlan {
                    from: args.from.clone(),
                    dry_run: args.dry_run,
                    force: args.force,
                    to: args.to.clone(),
                    cwd: context.cwd.clone(),
                }));
            }
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
            _ if let Some(forced) = self.start_in_ui(runtime) => {
                Action::StartUi { request, forced }
            }
            // Agents get the start result only; the TUI needs a person at a terminal.
            Some(Command::Start(args)) if args.attach && runtime.mode != OutputMode::Agent => {
                Action::RpcThenUi(request)
            }
            Some(Command::Attach { unit })
                if runtime.mode == OutputMode::Agent && !runtime.stdin_tty =>
            {
                return Err(Error::Usage(format!(
                    "agent attach needs a terminal; read the output with {}",
                    crate::Hint::command([
                        "logs",
                        unit.as_deref().unwrap_or_default(),
                        "-f",
                        "--timeout",
                        "30s"
                    ])
                    .0
                )));
            }
            Some(Command::Attach { .. }) => Action::Attach(request),
            Some(Command::Url {
                command: UrlCommand::Open { .. },
            }) if runtime.mode != OutputMode::Agent => Action::Open(request),
            Some(Command::Env {
                command: EnvCommand::Export { shell, .. },
            }) => Action::ExportEnv {
                request,
                shell: *shell,
            },
            Some(Command::Env {
                command: EnvCommand::Shell { .. },
            }) => Action::Shell(request),
            _ => Action::Rpc(request),
        })
    }

    /// Whether this start runs inside the TUI, like mprocs: `start`, `restart`,
    /// `lane resume` and `lane new --start` on a terminal in human output,
    /// outside CI, without `--no-ui`, `DEVLANES_NO_UI` or `-v`. `Some(true)` when
    /// `start --attach` asked for it, which also overrides `DEVLANES_NO_UI` and
    /// `tui.open_on_start: false`; the caller applies that config setting.
    pub fn start_in_ui(&self, runtime: &Runtime) -> Option<bool> {
        let (no_ui, forced) = match &self.command {
            Some(Command::Start(args)) => (args.no_ui, args.attach),
            Some(Command::Restart(args)) => (args.no_ui, false),
            Some(Command::Lane {
                command: LaneCommand::Resume { no_ui, .. },
            }) => (*no_ui, false),
            Some(Command::Lane {
                command:
                    LaneCommand::New {
                        start: true, no_ui, ..
                    },
            }) => (*no_ui, false),
            _ => return None,
        };
        // Verbose diagnostics go to stderr, which the TUI's screen would hide.
        (runtime.can_open_ui() && runtime.verbose == 0 && !no_ui && (forced || !runtime.no_ui))
            .then_some(forced)
    }

    /// The choice to offer when the command lacks the argument it needs, `None`
    /// when it has it (or needs none). `logs --mine`/`--all`, `run --list` and
    /// commands with a default (stop, env) never ask.
    pub fn missing_choice(&self) -> Option<crate::Choice> {
        use crate::ChoiceKind as K;
        let (command, kind, multi): (Vec<&'static str>, K, bool) = match &self.command {
            Some(Command::Url {
                command: UrlCommand::Print { unit: None },
            }) => (vec!["url", "print"], K::Url, false),
            Some(Command::Url {
                command: UrlCommand::Open { unit: None },
            }) => (vec!["url", "open"], K::Url, false),
            Some(Command::Attach { unit: None }) => (vec!["attach"], K::Running, false),
            Some(Command::Wait { target: None }) => (vec!["wait"], K::Target, false),
            Some(Command::Debug {
                command: DebugCommand::Why { unit: None },
            }) => (vec!["debug", "why"], K::Unit, false),
            Some(Command::Restart(args)) if args.units.is_empty() => {
                (vec!["restart"], K::Unit, true)
            }
            Some(Command::Logs(args)) if args.units.is_empty() && !args.mine && !args.all => {
                (vec!["logs"], K::Unit, true)
            }
            Some(Command::Run(args)) if args.task.is_none() && !args.list => {
                (vec!["run"], K::Task, false)
            }
            Some(Command::Lane {
                command: LaneCommand::Clean { lane: None },
            }) => (vec!["lane", "clean"], K::Lane, false),
            _ => return None,
        };
        Some(crate::Choice {
            about: about(&command),
            command,
            kind,
            multi,
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

/// The subcommand path named by `args` (the command line without the program),
/// skipping flags and their values: `["url", "open"]` for `--lane x url open`.
pub fn command_path(args: &[String]) -> Vec<String> {
    let mut command = <Cli as clap::CommandFactory>::command();
    let mut path = Vec::new();
    let mut args = args.iter().peekable();
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if let Some(flag) = arg.strip_prefix("--") {
            // A long flag that takes a value consumes the next word.
            let takes_value = !flag.contains('=')
                && command
                    .get_arguments()
                    .chain(<Cli as clap::CommandFactory>::command().get_arguments())
                    .any(|a| a.get_long() == Some(flag) && a.get_action().takes_values());
            if takes_value {
                args.next();
            }
            continue;
        }
        if arg.starts_with('-') {
            continue;
        }
        match command.find_subcommand(arg) {
            Some(sub) => {
                path.push(arg.clone());
                command = sub.clone();
            }
            None => break,
        }
    }
    path
}

/// A clap usage error with the command's description after it, so a typo still
/// says what the command does: `<clap's message>\n\nurl open: Open a service's URL...`.
pub fn usage_error(message: &str, args: &[String]) -> String {
    let path = command_path(args);
    let names: Vec<&str> = path.iter().map(String::as_str).collect();
    let about = about(&names);
    let mut text = message.trim_end().to_owned();
    if !path.is_empty() && !about.is_empty() {
        text.push_str(&format!("\n\ndevlanes {}: {about}\n", names.join(" ")));
    } else {
        text.push('\n');
    }
    text
}

/// The one-line description of the subcommand at `path` (`["url", "open"]`).
pub fn about(path: &[&str]) -> String {
    let mut command = <Cli as clap::CommandFactory>::command();
    for name in path {
        match command.find_subcommand(name) {
            Some(sub) => command = sub.clone(),
            None => return String::new(),
        }
    }
    command
        .get_about()
        .map(ToString::to_string)
        .unwrap_or_default()
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

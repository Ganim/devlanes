use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use clap::{Args, Parser, Subcommand};
use devlanes_proto::{Context, ExecParams, LogQuery, Method, Request, Selection};
use serde_json::json;

use crate::{Error, OutputMode, Runtime};

/// Public parser for the binary and shell completion frontend.
#[derive(Debug, Parser)]
#[command(
    name = "devlanes",
    version,
    about = "Dev environments in worktree lanes"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(long, global = true)]
    pub json: bool,
    #[arg(long, global = true, value_enum)]
    pub output: Option<OutputMode>,
    #[arg(long, global = true)]
    pub project: Option<String>,
    #[arg(long, global = true)]
    pub lane: Option<String>,
    #[arg(long, global = true)]
    pub host: Option<String>,
    #[arg(long, global = true)]
    pub yes: bool,
    #[arg(long, global = true)]
    pub quiet: bool,
    #[arg(long = "env", global = true, value_parser = parse_env)]
    pub env: Vec<(String, String)>,
    #[arg(long, global = true, value_parser = parse_duration)]
    pub timeout: Option<Duration>,
    #[arg(long, global = true, value_parser = parse_budget)]
    pub budget: Option<usize>,
}

/// Commands resolve references in the daemon, including branch names and globs.
#[derive(Debug, Subcommand)]
pub enum Command {
    Start(StartArgs),
    With(WithArgs),
    Stop(StopArgs),
    Restart(RestartArgs),
    Status(StatusArgs),
    Logs(LogArgs),
    Errors {
        #[arg(long)]
        since: Option<String>,
    },
    Wait {
        target: String,
    },
    Why {
        unit: String,
    },
    Run(RunArgs),
    Exec(ExecArgs),
    Attach {
        unit: String,
    },
    Open {
        unit: String,
    },
    Url {
        unit: String,
    },
    Env(EnvArgs),
    Shell {
        unit: Option<String>,
    },
    Top,
    Resume,
    Ui,
    Lane {
        #[command(subcommand)]
        command: LaneCommand,
    },
    Infra {
        #[command(subcommand)]
        command: InfraCommand,
    },
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    Schema,
    Doctor,
    Hosts,
    Trust {
        #[arg(long)]
        hosts: bool,
    },
    Untrust,
    Unlock,
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
    },
    Connect {
        host: String,
        #[arg(long)]
        background: bool,
    },
    Disconnect {
        host: Option<String>,
    },
    Host {
        #[command(subcommand)]
        command: HostCommand,
    },
    Events {
        #[arg(short, long)]
        follow: bool,
    },
    AgentGuide,
    Completions {
        shell: clap_complete::Shell,
    },
    #[command(hide = true)]
    Tunnel {
        #[arg(long, required = true)]
        stdio: bool,
    },
    #[command(hide = true)]
    Askpass {
        prompt: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct StartArgs {
    pub units: Vec<String>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub attach: bool,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    pub units: Vec<String>,
    #[arg(long)]
    pub mine: bool,
    #[arg(long)]
    pub logs_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct WithArgs {
    pub units: Vec<String>,
    #[arg(long)]
    pub profile: Option<String>,
    #[arg(long)]
    pub logs_dir: Option<PathBuf>,
    #[arg(long)]
    pub keep: bool,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RestartArgs {
    #[arg(required = true)]
    pub units: Vec<String>,
}

#[derive(Debug, Args)]
pub struct StatusArgs {
    pub lane: Option<String>,
    #[arg(long)]
    pub watch: bool,
    #[arg(long)]
    pub mine: bool,
}

#[derive(Debug, Args)]
pub struct LogArgs {
    #[arg(required_unless_present_any = ["mine", "all"])]
    pub units: Vec<String>,
    #[arg(long, conflicts_with_all = ["units", "mine"])]
    pub all: bool,
    #[arg(long)]
    pub tail: Option<usize>,
    #[arg(short, long)]
    pub follow: bool,
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub grep: Option<String>,
    #[arg(long)]
    pub level: Option<String>,
    #[arg(long)]
    pub around: Option<String>,
    #[arg(short = 'C', long = "context", default_value_t = 3)]
    pub context_lines: usize,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long)]
    pub mine: bool,
}

#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(required_unless_present = "list", conflicts_with = "list")]
    pub task: Option<String>,
    #[arg(long)]
    pub list: bool,
    #[arg(long, conflicts_with = "list")]
    pub detach: bool,
}

#[derive(Debug, Args)]
pub struct ExecArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub no_pty: bool,
    #[arg(last = true, required = true)]
    pub command: Vec<String>,
}

#[derive(Debug, Args)]
pub struct EnvArgs {
    pub unit: Option<String>,
    #[arg(long, conflicts_with = "export")]
    pub explain: bool,
    #[arg(long)]
    pub export: bool,
}

#[derive(Debug, Subcommand)]
pub enum LaneCommand {
    Ls,
    New {
        branch: String,
        #[arg(long)]
        start: bool,
    },
    Clean {
        lane: String,
    },
    Gc,
}

#[derive(Debug, Subcommand)]
pub enum InfraCommand {
    Up,
    Down,
    Status,
}
#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    Status,
}
#[derive(Debug, Subcommand)]
pub enum ConfigCommand {
    Check,
}
#[derive(Debug, Subcommand)]
pub enum HostCommand {
    Ls,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    Status,
    Stop,
    Restart,
    Logs {
        #[arg(short, long)]
        follow: bool,
        #[arg(long)]
        tail: Option<usize>,
    },
    Install,
    Uninstall,
    ResetState,
    #[command(hide = true)]
    Run,
}

/// Work for the composition root; no variant claims a side effect has happened.
pub enum Action {
    With(crate::WithPlan),
    StopWithLogs { request: Request, logs_dir: PathBuf },
    Rpc(Request),
    RpcThenUi(Request),
    Attach(Request),
    Open(Request),
    ExportEnv(Request),
    Shell(Request),
    Ui,
    Trust { hosts: bool },
    Untrust,
    Unlock(Context),
    DaemonRestart(Context),
    DaemonLogs { follow: bool, tail: Option<usize> },
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
            host: self.host.clone(),
            yes: self.yes,
            env: None,
        };
        let timeout_ms = self
            .timeout
            .map(|v| u64::try_from(v.as_millis()).unwrap_or(u64::MAX));
        let selection =
            |units: &[String], env: &[(String, String)], profile: Option<String>, mine| {
                serde_json::to_value(Selection {
                    units: units.to_vec(),
                    env: env.iter().cloned().collect(),
                    profile,
                    mine,
                    timeout_ms,
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
                    profile: args.profile.clone(),
                    timeout_ms: Some(timeout_ms.unwrap_or(180_000)),
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
                let mut params = selection(&args.units, &self.env, args.profile.clone(), false)?;
                params["timeout_ms"] = json!(timeout_ms.unwrap_or(180_000));
                (Method::Start, params)
            }
            Some(Command::Stop(args)) => {
                (Method::Stop, selection(&args.units, &[], None, args.mine)?)
            }
            Some(Command::Restart(args)) => (
                Method::Restart,
                selection(&args.units, &self.env, None, false)?,
            ),
            Some(Command::Status(args)) => {
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
            Some(Command::Errors { since }) => (Method::Errors, json!({"since": since})),
            Some(Command::Wait { target }) => (
                Method::Wait,
                json!({"target": target, "timeout_ms": timeout_ms}),
            ),
            Some(Command::Why { unit }) => (Method::Why, json!({"unit": unit})),
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
            Some(Command::Open { unit } | Command::Url { unit }) => {
                (Method::Url, json!({"unit": unit}))
            }
            Some(Command::Env(args)) => (
                Method::Env,
                json!({"unit": args.unit, "explain": args.explain}),
            ),
            Some(Command::Shell { unit }) => (Method::Env, json!({"unit": unit})),
            Some(Command::Top) => (Method::Metrics, json!({})),
            Some(Command::Resume) => (Method::Resume, json!({"timeout_ms": timeout_ms})),
            Some(Command::Lane { command }) => match command {
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
            Some(Command::Sync { .. }) => (Method::SyncStatus, json!({})),
            Some(Command::Config { .. }) => (Method::ConfigCheck, json!({})),
            Some(Command::Schema) => (Method::Schema, json!({})),
            Some(Command::Doctor) => (Method::Doctor, json!({})),
            Some(Command::Hosts) => (Method::Hosts, json!({})),
            Some(Command::Trust { hosts }) => return Ok(Action::Trust { hosts: *hosts }),
            Some(Command::Untrust) => return Ok(Action::Untrust),
            Some(Command::Unlock) => return Ok(Action::Unlock(context)),
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
            Some(Command::Connect { host, background }) => (
                Method::Connect,
                json!({"host": host, "background": background}),
            ),
            Some(Command::Disconnect { host }) => (Method::Disconnect, json!({"host": host})),
            Some(Command::Host { .. }) => (Method::HostList, json!({})),
            Some(Command::Events { follow }) => (Method::Subscribe, json!({"follow": follow})),
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
            Some(Command::Start(args)) if args.attach => Action::RpcThenUi(request),
            Some(Command::Attach { .. }) => Action::Attach(request),
            Some(Command::Open { .. }) if runtime.mode != OutputMode::Agent => {
                Action::Open(request)
            }
            Some(Command::Env(args)) if args.export => Action::ExportEnv(request),
            Some(Command::Shell { .. }) => Action::Shell(request),
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
                    | Command::Events { .. }
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

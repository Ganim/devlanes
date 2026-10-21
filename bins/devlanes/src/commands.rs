//! Executes CLI actions: RPCs with streaming, local terminal work, and `with`.

use std::time::Duration;

use devlanes_cli::{
    AGENT_GUIDE, Action, Cli, Hint, Rendered, Report, Runtime, export_environment, run_with,
    stop_with_logs, write_completions,
};
use devlanes_client::{Client, Paths};
use devlanes_ipc::Frame;
use devlanes_proto::{Control, Method, Outcome, Request, RpcError, Selection};
use serde_json::{Value, json};

use crate::{attach, driver::ClientDriver, driver::rpc_error, output::emit};

/// Ctrl-C exit status when a client detaches; managed units keep running.
const DETACHED: u8 = 130;

/// Routes one action and returns the process exit status.
pub async fn dispatch(cli: &Cli, runtime: &Runtime, action: Action) -> u8 {
    match run(cli, runtime, action).await {
        Ok(code) => code,
        Err(error) => {
            let rendered = runtime.reporter().error(&error, None);
            emit(&rendered);
            rendered.exit_code
        }
    }
}

/// Paths are resolved per invocation so `DEVLANES_HOME` isolates tests completely.
fn paths() -> Result<Paths, RpcError> {
    Paths::discover().map_err(|error| {
        RpcError::new(
            "daemon_unreachable",
            json!({"message": error.to_string()}),
            4,
        )
    })
}

/// Connects, spawning the daemon unless this is a read-only probe of a stopped daemon.
async fn connect(paths: &Paths) -> Result<Client, RpcError> {
    Client::connect_or_spawn(paths).await.map_err(rpc_error)
}

/// Main action switch. Variants that need integrations not wired yet report it plainly.
async fn run(cli: &Cli, runtime: &Runtime, action: Action) -> Result<u8, RpcError> {
    match action {
        Action::Rpc(request) => rpc(cli, runtime, request).await,
        Action::RpcThenUi(request) => {
            let code = rpc(cli, runtime, request).await?;
            if code != 0 {
                return Ok(code);
            }
            ui().await
        }
        Action::Ui => ui().await,
        Action::Attach(request) => attach::run(&paths()?, request).await.map_err(|error| {
            RpcError::new("attach_failed", json!({"message": error.to_string()}), 1)
        }),
        Action::Open(request) => {
            let data = call(request).await?;
            let url = data
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            open_url(&url).await?;
            emit(&Rendered {
                stdout: format!("{url}\n"),
                ..Rendered::default()
            });
            Ok(0)
        }
        Action::ExportEnv(request) => {
            let data = call(request).await?;
            let env = serde_json::from_value(data.get("env").cloned().unwrap_or_default())
                .unwrap_or_default();
            let text = export_environment(&env).map_err(|error| {
                RpcError::new("usage", json!({"message": error.to_string()}), 2)
            })?;
            emit(&Rendered {
                stdout: text,
                ..Rendered::default()
            });
            Ok(0)
        }
        Action::Shell(request) => {
            let cwd = request.context.cwd.clone();
            let data = call(request).await?;
            let mut env: std::collections::BTreeMap<String, String> = std::env::vars().collect();
            let lane: std::collections::BTreeMap<String, String> =
                serde_json::from_value(data.get("env").cloned().unwrap_or_default())
                    .unwrap_or_default();
            env.extend(lane);
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            let status = tokio::process::Command::new(shell)
                .env_clear()
                .envs(env)
                .current_dir(cwd)
                .status()
                .await
                .map_err(|error| {
                    RpcError::new("spawn_failed", json!({"message": error.to_string()}), 1)
                })?;
            Ok(status
                .code()
                .and_then(|code| u8::try_from(code).ok())
                .unwrap_or(1))
        }
        Action::AgentGuide => {
            emit(&Rendered {
                stdout: AGENT_GUIDE.into(),
                ..Rendered::default()
            });
            Ok(0)
        }
        Action::Completions(shell) => {
            let mut buffer = Vec::new();
            write_completions(shell, &mut buffer)
                .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
            emit(&Rendered {
                stdout: String::from_utf8_lossy(&buffer).into_owned(),
                ..Rendered::default()
            });
            Ok(0)
        }
        Action::DaemonRestart(context) => {
            let paths = paths()?;
            if let Ok(mut client) = Client::connect(&paths).await {
                let processes = client.peer.managed_processes;
                if processes > 0
                    && !context.yes
                    && !confirm(
                        runtime,
                        &format!("restart the daemon and stop {processes} processes?"),
                    )?
                {
                    return Ok(devlanes_cli::exit::USAGE);
                }
                let context = devlanes_proto::Context {
                    yes: true,
                    ..context
                };
                client
                    .request(Method::DaemonShutdown, json!({}), context)
                    .await
                    .map_err(rpc_error)?;
                // The old daemon releases its lock only after stopping everything.
                for _ in 0..200 {
                    if Client::connect(&paths).await.is_err() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            let mut client = connect(&paths).await?;
            let data = client
                .request(
                    Method::DaemonStatus,
                    json!({}),
                    devlanes_proto::Context::default(),
                )
                .await
                .map_err(rpc_error)?;
            render_result(runtime, Method::DaemonStatus, &data, None)
        }
        Action::DaemonLogs { follow, tail } => daemon_logs(follow, tail.unwrap_or(100)).await,
        Action::DaemonInstall => crate::daemon::install(runtime).await,
        Action::DaemonUninstall => crate::daemon::uninstall(runtime).await,
        Action::DaemonRun => Err(RpcError::new(
            "usage",
            json!({"message": "daemon run must be the only action"}),
            2,
        )),
        Action::With(plan) => {
            let paths = paths()?;
            let client = connect(&paths).await?;
            let mut driver = ClientDriver::new(client, plan.start.context.clone());
            let outcome =
                run_with(&plan, runtime, &mut driver, &mut |rendered| emit(&rendered)).await;
            Ok(outcome.exit_code)
        }
        Action::StopWithLogs { request, logs_dir } => {
            let paths = paths()?;
            let client = connect(&paths).await?;
            let selection: Selection =
                serde_json::from_value(request.params.clone()).unwrap_or_default();
            let lane = request.context.lane.clone().unwrap_or_default();
            let mut driver = ClientDriver::new(client, request.context.clone());
            let outcome = stop_with_logs(
                &lane,
                selection,
                &logs_dir,
                runtime,
                &mut driver,
                &mut |rendered| emit(&rendered),
            )
            .await;
            Ok(outcome.exit_code)
        }
        other => crate::daemon::local_action(runtime, other).await,
    }
}

/// One-shot request returning the data, for actions that post-process it locally.
async fn call(request: Request) -> Result<Value, RpcError> {
    let paths = paths()?;
    let mut client = connect(&paths).await?;
    client
        .request(request.method, request.params, request.context)
        .await
        .map_err(rpc_error)
}

/// Streams events, renders the final result, and maps Ctrl-C to a detach.
async fn rpc(cli: &Cli, runtime: &Runtime, request: Request) -> Result<u8, RpcError> {
    let paths = paths()?;
    // Read-only daemon probes must not start a daemon just to say it isn't running.
    let mut client =
        if request.method == Method::DaemonStatus || request.method == Method::DaemonShutdown {
            match Client::connect(&paths).await {
                Ok(client) => client,
                Err(_) if request.method == Method::DaemonShutdown => {
                    emit(&Rendered {
                        stderr: "daemon is not running\n".into(),
                        ..Rendered::default()
                    });
                    return Ok(0);
                }
                Err(_) => connect(&paths).await?,
            }
        } else {
            connect(&paths).await?
        };
    let method = request.method;
    let follow_limit = cli.timeout.filter(|_| {
        cli.streams()
            && !matches!(
                method,
                Method::Start | Method::Restart | Method::Wait | Method::Resume
            )
    });
    let id = client
        .begin_request(method, request.params.clone(), request.context.clone())
        .await
        .map_err(rpc_error)?;
    let reporter = runtime.reporter();
    let stream = async {
        loop {
            match client.next_frame().await.map_err(rpc_error)? {
                Frame::Control(Control::Event(event)) => {
                    if let Ok(rendered) = reporter.event(method, &event) {
                        emit(&rendered);
                    }
                }
                Frame::Control(Control::Response(response)) if response.id == id => {
                    return match response.result {
                        Outcome::Ok { data } => {
                            render_result(runtime, method, &data, more_hint(method, &request))
                        }
                        Outcome::Error { error } => {
                            let rendered =
                                reporter.error(&error, hint_for(&error, &request).as_ref());
                            emit(&rendered);
                            Ok(rendered.exit_code)
                        }
                    };
                }
                _ => {}
            }
        }
    };
    let limited = async {
        match follow_limit {
            Some(limit) => tokio::time::timeout(limit, stream).await.unwrap_or(Ok(0)),
            None => stream.await,
        }
    };
    tokio::select! {
        result = limited => result,
        _ = tokio::signal::ctrl_c() => {
            // Ctrl-C detaches only: the daemon keeps doing what was asked (cli-design 4.3).
            if runtime.mode == devlanes_cli::OutputMode::Human && cli.streams() {
                emit(&Rendered { stderr: "detached; services keep running (devlanes stop to stop them)\n".into(), ..Rendered::default() });
            }
            Ok(DETACHED)
        }
    }
}

/// Renders the response data with the mode's reporter.
fn render_result(
    runtime: &Runtime,
    method: Method,
    data: &Value,
    more: Option<Hint>,
) -> Result<u8, RpcError> {
    let mut report = Report::new(method, data);
    if let Some(more) = more {
        report = report.with_more(more);
    }
    let rendered = runtime.reporter().result(&report).map_err(|error| {
        RpcError::new("render_failed", json!({"message": error.to_string()}), 1)
    })?;
    emit(&rendered);
    Ok(rendered.exit_code)
}

/// Truncated logs say how to get the rest.
fn more_hint(method: Method, request: &Request) -> Option<Hint> {
    (method == Method::Logs).then(|| {
        let units = request.params.get("units").and_then(Value::as_array);
        let unit = units
            .and_then(|u| u.first())
            .and_then(Value::as_str)
            .unwrap_or("");
        Hint::command(["logs", unit, "--tail", "500"])
    })
}

/// Common hints for daemon error codes; the CLI owns all wording.
fn hint_for(error: &RpcError, request: &Request) -> Option<Hint> {
    match error.code.as_str() {
        "project_running_elsewhere" => error
            .data
            .get("project")
            .and_then(Value::as_str)
            .map(|project| Hint::command(["stop", "--project", project])),
        "secrets_locked" => Some(Hint::command(["unlock"])),
        "unit_failed" | "ready_timeout" => {
            error.data.get("unit").and_then(Value::as_str).map(|unit| {
                let lane = request.context.lane.clone().unwrap_or_default();
                if lane.is_empty() {
                    Hint::command(["logs", unit])
                } else {
                    Hint::logs(&lane, unit, 200)
                }
            })
        }
        "confirmation_required" => Some(Hint::command(["daemon", "stop", "--yes"])),
        _ => None,
    }
}

/// Asks on a TTY; agents and CI must pass `--yes` instead.
fn confirm(runtime: &Runtime, question: &str) -> Result<bool, RpcError> {
    if !runtime.can_prompt() {
        return Err(RpcError::new(
            "confirmation_required",
            json!({"flag": "--yes"}),
            2,
        ));
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    std::io::stdin()
        .read_line(&mut answer)
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

/// The browser is launched by the client, never the daemon (it may be remote or headless).
async fn open_url(url: &str) -> Result<(), RpcError> {
    if url.is_empty() {
        return Err(RpcError::new("no_url", Value::Null, 3));
    }
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    tokio::process::Command::new(opener)
        .arg(url)
        .status()
        .await
        .map_err(|error| RpcError::new("open_failed", json!({"message": error.to_string()}), 1))?;
    Ok(())
}

/// Reads the daemon's own log file directly; works even when the daemon is wedged.
async fn daemon_logs(follow: bool, tail: usize) -> Result<u8, RpcError> {
    let paths = paths()?;
    let path = paths.logs.join("daemon.log");
    let mut args = vec!["-n".to_owned(), tail.to_string()];
    if follow {
        args.push("-F".into());
    }
    let status = tokio::process::Command::new("tail")
        .args(&args)
        .arg(&path)
        .status()
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    Ok(status
        .code()
        .and_then(|code| u8::try_from(code).ok())
        .unwrap_or(1))
}

/// Opens the TUI; it is just another client of the same daemon.
async fn ui() -> Result<u8, RpcError> {
    crate::daemon::ui(&paths()?).await
}

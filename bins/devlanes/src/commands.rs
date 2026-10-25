//! Executes CLI actions: RPCs with streaming, local terminal work, and `with`.

use std::time::Duration;

use devlanes_cli::{
    Action, Cli, CommandTitle, Hint, InterruptPolicy, Rendered, Report, ReporterStream, Runtime,
    export_environment, run_with, stop_with_logs, write_completions,
};
use devlanes_client::{Client, Paths};
use devlanes_ipc::Frame;
use devlanes_proto::{Control, Method, Outcome, Request, RpcError, Selection};
use serde_json::{Value, json};

use crate::{
    attach,
    driver::{ClientDriver, Signals, rpc_error},
    output::emit,
};

// Prompts read `CONIN$` on Windows, which has no `/dev/tty`.
#[cfg(windows)]
use crate::console::read_tty;

/// Ctrl-C exit status when a client detaches; managed units keep running.
const DETACHED: u8 = 130;

/// Routes one action and returns the process exit status.
pub async fn dispatch(cli: &Cli, runtime: &Runtime, action: Action) -> u8 {
    match run(cli, runtime, action).await {
        Ok(code) => code,
        Err(error) => {
            let hint = confirmation_hint(&error)
                .or_else(|| legacy_state_hint(&error))
                .or_else(|| fix_hint(&error))
                .or_else(|| crate::user_config::hint(&error));
            let rendered = runtime.reporter().error(&error, hint.as_ref());
            tracing::debug!(
                code = %error.code,
                exit_code = rendered.exit_code,
                "exit code: the command failed before a result"
            );
            emit(&rendered);
            rendered.exit_code
        }
    }
}

/// For `confirmation_required`: this very invocation again, with the flag the
/// refusal names (`devlanes lane clean 1 --yes`).
fn confirmation_hint(error: &RpcError) -> Option<Hint> {
    (error.code == "confirmation_required").then(|| {
        let flag = error
            .data
            .get("flag")
            .and_then(Value::as_str)
            .unwrap_or("--yes");
        Hint::rerun_with(std::env::args().skip(1), flag)
    })
}

/// An exact shell fix carried in the error data (`chmod 700 <dir>`), shown instead
/// of the code's generic hint.
fn fix_hint(error: &RpcError) -> Option<Hint> {
    error
        .data
        .get("fix")
        .and_then(Value::as_str)
        .map(|fix| Hint(fix.to_owned()))
}

/// A daemon that refused an old state.db: the reset works without it.
fn legacy_state_hint(error: &RpcError) -> Option<Hint> {
    let reason = error.data.get("reason").and_then(Value::as_str)?;
    (error.code == "daemon_unreachable" && reason == "state_legacy_schema")
        .then(|| Hint::command(["daemon", "reset-state", "--yes"]))
}

/// Paths are resolved per invocation so `DEVLANES_HOME` isolates tests completely.
fn paths() -> Result<Paths, RpcError> {
    let paths = Paths::discover().map_err(|error| {
        RpcError::new(
            "daemon_unreachable",
            json!({"message": error.to_string()}),
            4,
        )
    })?;
    tracing::debug!(
        home = %paths.home.display(),
        home_from = if std::env::var_os("DEVLANES_HOME").is_some_and(|v| !v.is_empty()) {
            "DEVLANES_HOME"
        } else {
            "$HOME/.devlanes"
        },
        user_config = %paths.home.join("config.yaml").display(),
        socket = %paths.socket.display(),
        daemon_log = %paths.logs.join("daemon.log").display(),
        "paths"
    );
    Ok(paths)
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
        Action::RpcThenOfferStart(request) => {
            let data = match call(request.clone()).await {
                Ok(data) => data,
                Err(error) => {
                    let rendered = runtime
                        .reporter()
                        .error(&error, hint_for(&error, &request).as_ref());
                    emit(&rendered);
                    return Ok(rendered.exit_code);
                }
            };
            let code = render_result(
                runtime,
                request.method,
                &data,
                more_hint(request.method, &request),
            )?;
            let Some(lane) = data.pointer("/lane/reference").and_then(Value::as_str) else {
                return Ok(code);
            };
            if code != 0 || !confirm(runtime, &format!("start {lane} now?"))? {
                return Ok(code);
            }
            // Exactly what the printed `next:` command does, TUI and all.
            let usage = |error: &dyn std::fmt::Display| {
                RpcError::new("usage", json!({"message": error.to_string()}), 2)
            };
            let cli = <Cli as devlanes_cli::clap::Parser>::try_parse_from([
                "devlanes", "start", "--lane", lane,
            ])
            .map_err(|error| usage(&error))?;
            let action = cli.action(runtime).map_err(|error| usage(&error))?;
            Box::pin(run(&cli, runtime, action)).await
        }
        Action::Ui => ui().await,
        Action::StartUi { request, forced } => {
            let paths = paths()?;
            if !forced && !crate::start_ui::open_on_start(&paths) {
                return rpc(cli, runtime, request).await;
            }
            let client = connect(&paths).await?;
            crate::start_ui::run(runtime, &paths, client, request.clone(), |error| {
                hint_for(error, &request)
            })
            .await
        }
        Action::Choose { choice, context } => {
            let args: Vec<String> = std::env::args().skip(1).collect();
            match crate::choose::choose(runtime, &paths()?, &choice, &context, &args).await? {
                crate::choose::Chosen::Exit(code) => Ok(code),
                // The same command with the picked values, parsed like typed ones.
                crate::choose::Chosen::Values(values) => {
                    let completed = devlanes_cli::completed_args(&args, &values);
                    let cli = <Cli as devlanes_cli::clap::Parser>::try_parse_from(
                        std::iter::once("devlanes".to_owned()).chain(completed),
                    )
                    .map_err(|error| {
                        RpcError::new("usage", json!({"message": error.to_string()}), 2)
                    })?;
                    let action = cli.action(runtime).map_err(|error| {
                        RpcError::new("usage", json!({"message": error.to_string()}), 2)
                    })?;
                    if matches!(action, Action::Choose { .. }) {
                        return Err(RpcError::new(
                            "usage",
                            json!({"message": "the chosen value did not complete the command"}),
                            2,
                        ));
                    }
                    Box::pin(run(&cli, runtime, action)).await
                }
            }
        }
        // The daemon's codes (not_found, terminal_required, ...) keep their wording
        // and exit status; only local terminal failures become attach_failed.
        Action::Attach(request) => {
            attach::run(runtime, &paths()?, request)
                .await
                .map_err(|error| match error.downcast::<devlanes_client::Error>() {
                    Ok(error) => rpc_error(error),
                    Err(error) => {
                        RpcError::new("attach_failed", json!({"message": error.to_string()}), 1)
                    }
                })
        }
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
        Action::ExportEnv { request, shell } => {
            let data = call(request).await?;
            let env = serde_json::from_value(data.get("env").cloned().unwrap_or_default())
                .unwrap_or_default();
            let exported = export_environment(&env, shell);
            // On stderr, so `eval "$(devlanes env export)"` still works.
            let stderr = exported
                .skipped
                .iter()
                .map(|name| {
                    format!("warning: skipped {name:?}: not a valid shell variable name or value\n")
                })
                .collect();
            emit(&Rendered {
                stdout: exported.script,
                stderr,
                ..Rendered::default()
            });
            Ok(0)
        }
        Action::Shell(request) => {
            let cwd = request.context.cwd.clone();
            let data = call(request).await?;
            // vars_os: a variable that is not UTF-8 is passed on, not a panic.
            let mut env: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> =
                std::env::vars_os().collect();
            let lane: std::collections::BTreeMap<String, String> =
                serde_json::from_value(data.get("env").cloned().unwrap_or_default())
                    .unwrap_or_default();
            env.extend(
                lane.into_iter()
                    .map(|(key, value)| (key.into(), value.into())),
            );
            let shell = std::env::var("SHELL").unwrap_or_else(|_| default_shell());
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
        Action::DaemonStart => daemon_start(runtime, &paths()?).await,
        Action::DaemonRestart(context) => daemon_restart(runtime, &paths()?, context).await,
        Action::DaemonLogs { follow, tail } => daemon_logs(follow, tail.unwrap_or(100)).await,
        Action::DebugBundle {
            to,
            log_lines,
            args,
            context,
        } => crate::bundle::run(runtime, to, log_lines, args, context).await,
        Action::DaemonInstall => crate::daemon::install(runtime).await,
        Action::DaemonUninstall => crate::daemon::uninstall(runtime).await,
        Action::DaemonRun => Err(RpcError::new(
            "usage",
            json!({"message": "daemon run must be the only action"}),
            2,
        )),
        Action::With(plan) => {
            let paths = paths()?;
            // Before connecting: a signal from here on cancels the start and still
            // reaches the teardown instead of killing this process.
            let signals = Signals::install()
                .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
            let client = connect(&paths).await?;
            let progress = crate::progress::wrap(runtime);
            // The title stays for the whole run, the test command included.
            let title = crate::terminal::Title::start(runtime, Some(CommandTitle::with(&plan)));
            let mut driver = ClientDriver::new(client, plan.start.context.clone(), paths)
                .with_signals(signals)
                .with_progress(progress)
                .with_title(title);
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
            let mut driver = ClientDriver::new(client, request.context.clone(), paths);
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

/// `daemon start`: spawns the daemon when none runs and prints its status. A
/// running daemon is left alone, so this never stops managed processes.
async fn daemon_start(runtime: &Runtime, paths: &Paths) -> Result<u8, RpcError> {
    let mut client = connect(paths).await?;
    if !client.spawned && !runtime.quiet {
        emit(&Rendered {
            stderr: "daemon is already running\n".into(),
            ..Rendered::default()
        });
    }
    let mut data = client
        .request(
            Method::DaemonStatus,
            json!({}),
            devlanes_proto::Context::default(),
        )
        .await
        .map_err(rpc_error)?;
    if data.is_object() {
        data["running"] = json!(true);
        data["started"] = json!(client.spawned);
    }
    render_result(runtime, Method::DaemonStatus, &data, None)
}

/// `daemon restart`: stops the daemon (any protocol), starts a new one and starts
/// again what ran. An invalid user config fails first: the new daemon would refuse
/// to start, and the old one would already have stopped every lane.
async fn daemon_restart(
    runtime: &Runtime,
    paths: &Paths,
    context: devlanes_proto::Context,
) -> Result<u8, RpcError> {
    crate::user_config::load(paths).await?;
    // What runs now comes back after the restart; `daemon stop` keeps it stopped.
    let mut running = Vec::new();
    let (env, actor) = (
        context.env.clone().unwrap_or_default(),
        context.actor.clone(),
    );
    // Any protocol: restarting is how an incompatible daemon gets replaced.
    match Client::connect_any(paths).await {
        Ok(mut client) => {
            if client.compatible() {
                running = client.running_lanes().await.unwrap_or_else(|error| {
                    tracing::warn!(%error, "could not read what runs; nothing is resumed");
                    Vec::new()
                });
            }
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
            // The old daemon's own pid, so the wait below ends when exactly
            // that process is gone.
            let pid = client
                .request(
                    Method::DaemonStatus,
                    json!({}),
                    devlanes_proto::Context::default(),
                )
                .await
                .ok()
                .and_then(|status| status.get("pid").and_then(Value::as_u64))
                .and_then(|pid| i32::try_from(pid).ok());
            let context = devlanes_proto::Context {
                yes: true,
                ..context
            };
            client
                .request(Method::DaemonShutdown, json!({}), context)
                .await
                .map_err(rpc_error)?;
            drop(client);
            wait_for_daemon_exit(paths, pid).await?;
        }
        Err(error) if error.is_not_running() => {}
        Err(error) => return Err(rpc_error(error)),
    }
    let mut client = connect(paths).await?;
    let mut data = client
        .request(
            Method::DaemonStatus,
            json!({}),
            devlanes_proto::Context::default(),
        )
        .await
        .map_err(rpc_error)?;
    if data.is_object() {
        data["running"] = json!(true);
    }
    let items = if running.is_empty() {
        Vec::new()
    } else {
        client.resume_running(&running, &env, &actor).await
    };
    // JSON output stays one document; text prints the starts after the status.
    if runtime.mode == devlanes_cli::OutputMode::Json {
        if data.is_object() {
            data["resumed"] = json!(items);
        }
        return render_result(runtime, Method::DaemonStatus, &data, None);
    }
    let status = render_result(runtime, Method::DaemonStatus, &data, None)?;
    if items.is_empty() {
        return Ok(status);
    }
    let page = json!({"items": items, "truncated": false, "next_cursor": null});
    let resumed = render_result(runtime, Method::Resume, &page, None)?;
    Ok(status.max(resumed))
}

/// Longest `daemon restart` waits for the old daemon, which first stops every unit
/// with its stop policy.
const DAEMON_EXIT_WAIT: Duration = Duration::from_secs(300);

/// Waits until the old daemon process `pid` has exited (it releases the lock only
/// after stopping everything); without a pid, until its socket stops answering.
async fn wait_for_daemon_exit(paths: &Paths, pid: Option<i32>) -> Result<(), RpcError> {
    let deadline = tokio::time::Instant::now() + DAEMON_EXIT_WAIT;
    // The PID is pinned to the start time read now, while the old daemon still
    // stops its units, so a process that reuses the PID later never counts as
    // the daemon. Start times need only query access, which an elevated
    // daemon grants too. The pipe is only the fallback when that fails: it
    // closes when the shutdown begins, before the lock is released.
    #[cfg(windows)]
    let identity = match pid.and_then(|pid| u32::try_from(pid).ok()) {
        Some(pid) => {
            let started =
                tokio::task::spawn_blocking(move || devlanes_sys_windows::proc::start_time(pid))
                    .await;
            match started {
                Ok(Ok(Some(started))) => Some((pid, started)),
                // No process has the PID any more.
                Ok(Ok(None)) => return Ok(()),
                Ok(Err(_)) | Err(_) => None,
            }
        }
        None => None,
    };
    loop {
        #[cfg(unix)]
        let gone = match pid.and_then(rustix::process::Pid::from_raw) {
            Some(pid) => rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH),
            None => Client::connect_any(paths)
                .await
                .is_err_and(|error| error.is_not_running()),
        };
        #[cfg(windows)]
        let gone = match identity {
            Some((pid, started)) => {
                let alive = tokio::task::spawn_blocking(move || {
                    devlanes_sys_windows::proc::matches(pid, started)
                })
                .await;
                // A failed query keeps waiting; the deadline still bounds it.
                matches!(alive, Ok(Ok(false)))
            }
            None => Client::connect_any(paths)
                .await
                .is_err_and(|error| error.is_not_running()),
        };
        if gone {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RpcError::new(
                "daemon_still_running",
                json!({"pid": pid, "waited_s": DAEMON_EXIT_WAIT.as_secs()}),
                1,
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
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
    let mut client = match request.method {
        // A daemon that cannot open state.db (STATE_LEGACY_SCHEMA) never starts, so
        // without a running one the reset happens here.
        Method::DaemonResetState => match Client::connect(&paths).await {
            Ok(client) => client,
            Err(error) if error.is_not_running() => {
                return reset_state_offline(runtime, &paths, request.context.yes).await;
            }
            Err(error) => return Err(rpc_error(error)),
        },
        // `daemon.shutdown` is frozen across protocols, so `daemon stop` reaches an
        // incompatible daemon too, and never spawns one just to stop it.
        Method::DaemonShutdown => match Client::connect_any(&paths).await {
            Ok(client) => client,
            Err(error) if error.is_not_running() => {
                if !runtime.quiet {
                    emit(&Rendered {
                        stderr: "daemon is not running\n".into(),
                        ..Rendered::default()
                    });
                }
                return Ok(0);
            }
            Err(error) => return Err(rpc_error(error)),
        },
        // `daemon status` answers on any protocol and never starts a daemon: it
        // reports what runs, and "not running" (exit 1) when nothing does.
        Method::DaemonStatus => match Client::connect_any(&paths).await {
            Ok(client) => {
                if let Some(note) = version_note(&client.peer) {
                    emit(&Rendered {
                        stderr: note,
                        ..Rendered::default()
                    });
                }
                client
            }
            Err(error) if error.is_not_running() => {
                let rendered = daemon_not_running(runtime, &paths);
                emit(&rendered);
                return Ok(rendered.exit_code);
            }
            Err(error) => return Err(rpc_error(error)),
        },
        // `doctor` never starts a daemon: without one it checks what it can here.
        Method::Doctor => match Client::connect(&paths).await {
            Ok(client) => client,
            Err(error) if error.is_not_running() => {
                let mut data = crate::local_doctor::facts(&paths).await;
                crate::notices::annotate_doctor(None, &mut data).await;
                data["service"] = crate::daemon::service_report().await;
                crate::local_doctor::add_wsl(&mut data).await;
                return render_result(runtime, Method::Doctor, &data, None);
            }
            Err(error) => return Err(rpc_error(error)),
        },
        _ => connect(&paths).await?,
    };
    let method = request.method;
    // Only open-ended streams end quietly at --timeout; a foreground `run` gets the
    // timeout from the daemon, which stops the task and answers `timeout`.
    let follow_limit = cli.timeout.filter(|_| {
        cli.streams()
            && !matches!(
                method,
                Method::Start | Method::Restart | Method::Wait | Method::Resume | Method::Run
            )
    });
    let open_ended = matches!(method, Method::Logs | Method::Subscribe | Method::Status);
    // The tab's progress bar while a start is followed; a no-op otherwise.
    let progress = crate::progress::Follower::start(runtime, &request);
    let id = client
        .begin_request(method, request.params.clone(), request.context.clone())
        .await
        .map_err(rpc_error)?;
    // Set right before Ctrl-C is caught below, so every way out drops it and
    // puts the previous title back.
    let mut title = crate::terminal::Title::start(runtime, devlanes_cli::command_title(&request));
    let reporter = runtime.reporter();
    // `… infra` lines that turn into `✓ infra` when the phase ends.
    let mut phases = devlanes_cli::PhaseLines::new(runtime);
    let mut events: u64 = 0;
    // Followed logs and a foreground task's output share one output budget; the
    // reserve keeps room for the summary printed after the stream.
    let mut log_stream = streamed_output(&request).map(|more| {
        match more {
            Some(more) => ReporterStream::new(reporter.as_ref(), method, more),
            None => ReporterStream::without_hint(reporter.as_ref(), method),
        }
        .reserve(RUN_SUMMARY_BYTES)
    });
    let stream = async {
        loop {
            match client.next_frame().await.map_err(rpc_error)? {
                Frame::Control(Control::Event(event))
                    if event.event == devlanes_proto::remote::PROMPT_EVENT =>
                {
                    answer_prompt(&mut client, &event).await?;
                }
                Frame::Control(Control::Event(event)) => {
                    events += 1;
                    tracing::trace!(event = %event.event, seq = event.seq, "event received");
                    progress.event(&event);
                    crate::terminal::title_event(&mut title, &event);
                    let rendered = match log_stream.as_mut() {
                        Some(stream) => stream.event(&event),
                        None => reporter.event(method, &event),
                    };
                    if let Ok(mut rendered) = rendered {
                        phases.rewrite(&event, &mut rendered.stdout);
                        emit(&rendered);
                    }
                }
                Frame::Control(Control::Response(response)) if response.id == id => {
                    tracing::debug!(events, "events received before the response");
                    return match response.result {
                        Outcome::Ok { mut data } => {
                            if method == Method::Doctor {
                                crate::notices::annotate_doctor(Some(&mut client), &mut data).await;
                                // Trust stores belong to this user and machine, so the
                                // CLI checks them, not the daemon.
                                if data.get("ca").is_some_and(Value::is_object) {
                                    data["ca"]["trust"] = crate::local::trust_report().await;
                                }
                                // The service definition is in this user's home, read here.
                                if data.is_object() {
                                    data["service"] = crate::daemon::service_report().await;
                                }
                                // Facts of this terminal, whatever daemon answered.
                                #[cfg(windows)]
                                if data.is_object() {
                                    data["terminal"] = crate::local_doctor::terminal_facts();
                                }
                                crate::local_doctor::add_wsl(&mut data).await;
                            }
                            // The counterpart of the not-running answer's `false`.
                            if method == Method::DaemonStatus && data.is_object() {
                                data["running"] = json!(true);
                            }
                            // The budget hid the end of a failed task's output: show its tail.
                            if let Some(stream) = &log_stream
                                && data.get("state").and_then(Value::as_str) == Some("failed")
                                && stream.exhausted()
                            {
                                data["log_tail"] = json!(stream.hidden_tail());
                            }
                            let code =
                                render_result(runtime, method, &data, more_hint(method, &request))?;
                            // A task's hidden output is in its unit's log; a steps-only
                            // task has no unit, so there is nothing to point at.
                            if let Some(stream) = &log_stream
                                && method == Method::Run
                                && stream.exhausted()
                                && let Some(unit) = data.get("unit").and_then(Value::as_str)
                            {
                                let lane =
                                    data.get("lane").and_then(Value::as_str).unwrap_or_default();
                                emit(&Rendered {
                                    stdout: format!("more: {}\n", Hint::logs(lane, unit, 500).0),
                                    ..Rendered::default()
                                });
                            }
                            Ok(code)
                        }
                        Outcome::Error { error } => {
                            let rendered =
                                reporter.error(&error, hint_for(&error, &request).as_ref());
                            tracing::debug!(
                                code = %error.code,
                                exit_code = rendered.exit_code,
                                "exit code: the daemon's error"
                            );
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
            Some(limit) => match tokio::time::timeout(limit, stream).await {
                Ok(result) => result,
                // Following logs, events or a watch for --timeout is a normal end.
                Err(_) if open_ended => Ok(0),
                Err(_) => Err(RpcError::new(
                    "timeout",
                    json!({}),
                    devlanes_cli::exit::TIMEOUT,
                )),
            },
            None => stream.await,
        }
    };
    let result = tokio::select! {
        result = limited => result,
        _ = tokio::signal::ctrl_c() => {
            // Detaching is not a failure: the bar just goes.
            progress.finish(true).await;
            return interrupted(cli, runtime, &paths, &request).await;
        }
    };
    progress.finish(matches!(result, Ok(0))).await;
    if let Some(title) = &title {
        title.finish(runtime, matches!(result, Ok(0)));
    }
    if result.is_ok() {
        crate::notices::after_command(&mut client, runtime, &paths, method).await;
    }
    result
}

/// Ctrl-C during a request, per [`InterruptPolicy`]: services never stop, but a
/// foreground task is stopped in agent mode and after a yes on a terminal.
async fn interrupted(
    cli: &Cli,
    runtime: &Runtime,
    paths: &Paths,
    request: &Request,
) -> Result<u8, RpcError> {
    let stop = match cli.interrupt_policy(runtime) {
        InterruptPolicy::StopTask => true,
        InterruptPolicy::ConfirmTaskStop => {
            let question = "\nstop the task? [y/N] ".to_owned();
            tokio::task::spawn_blocking(move || read_tty(question.trim_start(), "confirm"))
                .await
                .ok()
                .flatten()
                .is_some_and(|answer| matches!(answer.trim(), "y" | "Y" | "yes"))
        }
        InterruptPolicy::Detach | InterruptPolicy::ForwardCommandThenTeardown => false,
    };
    if stop && let Some(task) = request.params.get("task").and_then(Value::as_str) {
        // A fresh connection: the streaming one is still busy with the run request.
        let selection = json!({"units": [task]});
        let mut client = Client::connect(paths).await.map_err(rpc_error)?;
        let stopped = client
            .request(Method::Stop, selection, request.context.clone())
            .await;
        if !runtime.quiet {
            emit(&Rendered {
                stderr: match stopped {
                    Ok(_) => format!("stopped task {task}\n"),
                    Err(error) => format!("could not stop task {task}: {error}\n"),
                },
                ..Rendered::default()
            });
        }
        return Ok(DETACHED);
    }
    // Ctrl-C detaches only: the daemon keeps doing what was asked (cli-design 4.3).
    if runtime.mode == devlanes_cli::OutputMode::Human && cli.streams() {
        emit(&Rendered {
            stderr: "detached; services keep running (devlanes stop to stop them)\n".into(),
            ..Rendered::default()
        });
    }
    Ok(DETACHED)
}

/// `daemon status` when no daemon answers: said so in the mode's form, exit 1.
/// Nothing is started; the socket and home say where devlanes looked.
fn daemon_not_running(runtime: &Runtime, paths: &Paths) -> Rendered {
    let socket = paths.socket.display().to_string();
    let stdout = match runtime.mode {
        devlanes_cli::OutputMode::Json => format!(
            "{}\n",
            json!({"running": false, "socket": socket, "home": paths.home})
        ),
        devlanes_cli::OutputMode::Agent => format!("stopped daemon socket={socket}\n"),
        _ => format!(
            "daemon is not running ({} {socket}); devlanes daemon start starts it\n",
            devlanes_cli::endpoint_label(&socket)
        ),
    };
    Rendered {
        stdout,
        exit_code: 1,
        ..Rendered::default()
    }
}

/// A note when the daemon runs another version or protocol than this client.
fn version_note(peer: &devlanes_proto::Hello) -> Option<String> {
    let own = env!("CARGO_PKG_VERSION");
    if peer.protocol != devlanes_proto::PROTOCOL_VERSION {
        return Some(format!(
            "note: the daemon runs devlanes {} (protocol {}), this client {own} (protocol {}); run devlanes daemon restart\n",
            peer.version,
            peer.protocol,
            devlanes_proto::PROTOCOL_VERSION
        ));
    }
    (peer.version != own).then(|| {
        format!(
            "note: the daemon runs devlanes {}, this client {own}\n",
            peer.version
        )
    })
}

/// `daemon reset-state` while no daemon answers: with `--yes` (like the online
/// path; it never prompts), moves state.db and its sidecars aside under the daemon
/// lock, with the lane data folders, never deleting them. The next command spawns
/// a daemon with an empty database. The data folder comes from the user config, so
/// an invalid one is refused before anything moves.
async fn reset_state_offline(runtime: &Runtime, paths: &Paths, yes: bool) -> Result<u8, RpcError> {
    let json = runtime.mode == devlanes_cli::OutputMode::Json;
    let db = paths.db.clone();
    if crate::daemon::state_files(&db).is_empty() {
        emit(&Rendered {
            stdout: if json {
                format!(
                    "{}\n",
                    json!({"reset": false, "offline": true, "moved": []})
                )
            } else if runtime.quiet {
                String::new()
            } else {
                format!("no state database at {}; nothing to reset\n", db.display())
            },
            ..Rendered::default()
        });
        return Ok(0);
    }
    // Refuse while a daemon holds the lock, before asking for --yes.
    let home = paths.home.clone();
    drop(blocking(move || crate::daemon::hold_daemon_lock(&home)).await?);
    if !yes {
        return Err(RpcError::new(
            "confirmation_required",
            json!({"flag": "--yes", "offline": true}),
            2,
        ));
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    let stamp = crate::daemon::utc_stamp(seconds);
    let home = paths.home.clone();
    let data_root = crate::user_config::load(paths)
        .await?
        .lanes
        .data_root(&home);
    let moves =
        blocking(move || crate::daemon::move_state_aside(&home, &db, &data_root, &stamp)).await?;
    // Without a daemon nothing stops processes a crashed daemon left behind, and
    // their records leave with the old database.
    let warning = "warning: processes started by a crashed daemon may still run; the new daemon has no record of them\n";
    let stdout = if json {
        let moved: Vec<Value> = moves
            .iter()
            .map(|(from, to)| json!({"from": from, "to": to}))
            .collect();
        format!(
            "{}\n",
            json!({"reset": true, "offline": true, "moved": moved})
        )
    } else if runtime.quiet {
        String::new()
    } else {
        let mut text: String = moves
            .iter()
            .map(|(from, to)| format!("moved {} -> {}\n", from.display(), to.display()))
            .collect();
        text.push_str(
            "state reset without a daemon: lane numbers and ports will be reassigned; the next command starts a fresh daemon\n",
        );
        text
    };
    emit(&Rendered {
        stdout,
        stderr: warning.into(),
        ..Rendered::default()
    });
    Ok(0)
}

/// Runs filesystem work off the executor.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, RpcError> + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
}

/// Answers an ssh prompt that `connect` relayed (passphrase, OTP, host key) on the
/// controlling terminal. Without a terminal the empty answer cancels the prompt.
async fn answer_prompt(client: &mut Client, event: &devlanes_proto::Event) -> Result<(), RpcError> {
    let Some(stream_id) = event.data.get("stream_id").and_then(Value::as_u64) else {
        return Ok(());
    };
    let text = |key: &str| {
        event
            .data
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let (host, prompt, kind) = (text("host"), text("prompt"), text("kind"));
    // The text comes from ssh and the remote server: control characters could
    // rewrite the terminal, and the host prefix says who is asking.
    let prompt = format!("[{}] {}", printable(&host), printable(&prompt));
    let answer = tokio::task::spawn_blocking(move || read_tty(&prompt, &kind))
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    client
        .send_bytes(stream_id, bytes::Bytes::from(answer))
        .await
        .map_err(rpc_error)
}

/// `text` without control characters (escape sequences, carriage returns), with
/// line breaks kept as spaces.
fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c == '\n' || c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_owned()
}

/// Shows `prompt` on `/dev/tty` and reads one line, without echo for secrets.
/// Returns the line with its newline, so an empty answer stays distinct from cancel.
#[cfg(unix)]
fn read_tty(prompt: &str, kind: &str) -> Option<String> {
    use std::io::{BufRead, Write};
    let tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .ok()?;
    let mut out = &tty;
    if kind == "notify" {
        writeln!(out, "{prompt}").ok()?;
        return Some("\n".into());
    }
    write!(out, "{prompt} ").ok()?;
    out.flush().ok()?;
    let saved = (kind == "secret")
        .then(|| rustix::termios::tcgetattr(&tty).ok())
        .flatten();
    if let Some(saved) = &saved {
        let mut quiet = saved.clone();
        quiet.local_modes.remove(rustix::termios::LocalModes::ECHO);
        let _ = rustix::termios::tcsetattr(&tty, rustix::termios::OptionalActions::Now, &quiet);
    }
    let mut line = String::new();
    let read = std::io::BufReader::new(&tty).read_line(&mut line);
    if let Some(saved) = &saved {
        let _ = rustix::termios::tcsetattr(&tty, rustix::termios::OptionalActions::Now, saved);
        let _ = writeln!(out);
    }
    match read {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(format!("{}\n", line.trim_end_matches(['\r', '\n']))),
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
    // Read before the macro, which brings tracing's own `Value` into scope.
    let ready = data.get("ready").and_then(Value::as_bool);
    let timed_out = data.get("timed_out").and_then(Value::as_bool);
    let failures = data.get("failures").and_then(Value::as_array).map(Vec::len);
    let state = data.get("state").and_then(Value::as_str);
    let result_exit_code = data.get("exit_code").and_then(Value::as_i64);
    tracing::debug!(
        method = %devlanes_client::method_name(method),
        exit_code = rendered.exit_code,
        ready,
        timed_out,
        failures,
        state,
        result_exit_code,
        "exit code: from the result"
    );
    emit(&rendered);
    Ok(rendered.exit_code)
}

/// Room left after a budgeted stream for the result that follows it.
const RUN_SUMMARY_BYTES: usize = 2048;

/// Whether `request` streams log lines (`logs -f`, a foreground `run`), with the
/// continuation hint when it is known up front (a `run` learns it from its result).
fn streamed_output(request: &Request) -> Option<Option<Hint>> {
    let params = &request.params;
    match request.method {
        Method::Logs if params.get("follow").and_then(Value::as_bool) == Some(true) => {
            Some(more_hint(Method::Logs, request))
        }
        Method::Run if params.get("detach").and_then(Value::as_bool) != Some(true) => Some(None),
        // Watches and event streams share the agent's output budget too.
        Method::Status if params.get("watch").and_then(Value::as_bool) == Some(true) => Some(None),
        Method::Subscribe if params.get("follow").and_then(Value::as_bool) == Some(true) => {
            Some(None)
        }
        _ => None,
    }
}

/// `devlanes logs ARGS`, keeping the `--lane` and `--project` the user gave, so the
/// hint reads the same lane from any directory.
fn scoped_logs(request: &Request, args: &[&str]) -> Hint {
    let mut all: Vec<&str> = vec!["logs"];
    all.extend_from_slice(args);
    scoped(request, &all)
}

/// `devlanes ARGS` with the user's `--lane` and `--project`.
fn scoped(request: &Request, args: &[&str]) -> Hint {
    let mut all: Vec<&str> = args.to_vec();
    if let Some(lane) = &request.context.lane {
        all.extend(["--lane", lane.as_str()]);
    }
    if let Some(project) = &request.context.project {
        all.extend(["--project", project.as_str()]);
    }
    Hint::command(all)
}

/// Truncated logs say how to get the rest.
fn more_hint(method: Method, request: &Request) -> Option<Hint> {
    (method == Method::Logs).then(|| {
        let units = request.params.get("units").and_then(Value::as_array);
        let unit = units
            .and_then(|u| u.first())
            .and_then(Value::as_str)
            .unwrap_or("");
        scoped_logs(request, &[unit, "--tail", "500"])
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
        "unit_failed" | "ready_timeout" => {
            // A non-2xx answer to an http check is fixed in the config, not found in logs.
            if let Some(hint) = devlanes_cli::status_hint(&error.data) {
                return Some(hint);
            }
            let unit = error.data.get("unit").and_then(Value::as_str)?;
            // The daemon's canonical lane wins; otherwise the user's own selectors.
            Some(match error.data.get("lane").and_then(Value::as_str) {
                Some(lane) => Hint::logs(lane, unit, 200),
                None => scoped_logs(request, &[unit, "--tail", "200"]),
            })
        }
        "confirmation_required" => confirmation_hint(error),
        // What this command accepts, rather than every lane.
        "not_found" => match request.method {
            Method::Run => Some(Hint::command(["run", "--list"])),
            Method::Logs
            | Method::Env
            | Method::Why
            | Method::Url
            | Method::Attach
            | Method::Wait
            | Method::Stop
            | Method::Restart => Some(scoped(request, &["status"])),
            _ => None,
        },
        // `config check` is the command that just failed; point at the file instead.
        "config_error" if request.method == Method::ConfigCheck => {
            let file = error
                .data
                .get("file")
                .and_then(Value::as_str)
                .unwrap_or("devlanes.yaml");
            Some(Hint(match error.data.get("line").and_then(Value::as_u64) {
                Some(line) => format!("edit {file} at line {line}, then run this again"),
                None => format!("edit {file}, then run this again"),
            }))
        }
        // The same logs, fewer lines: one unit when several were asked for.
        "response_too_large" if request.method == Method::Logs => request
            .params
            .get("units")
            .and_then(Value::as_array)
            .and_then(|units| units.first())
            .and_then(Value::as_str)
            .map(|unit| scoped_logs(request, &[unit, "--tail", "200"])),
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
#[cfg(unix)]
async fn open_url(url: &str) -> Result<(), RpcError> {
    if url.is_empty() {
        return Err(RpcError::new("no_url", Value::Null, 3));
    }
    // Inside WSL the choice reads the mount table and PATH.
    let url_owned = url.to_owned();
    let opener = tokio::task::spawn_blocking(move || devlanes_sys::url_opener(&url_owned))
        .await
        .map_err(|error| RpcError::new("open_failed", json!({"message": error.to_string()}), 1))?;
    tokio::process::Command::new(&opener.program)
        .args(&opener.args)
        .status()
        .await
        .map_err(|error| RpcError::new("open_failed", json!({"message": error.to_string()}), 1))?;
    Ok(())
}

/// The browser is launched by the client, never the daemon. Windows hands
/// the URL to the shell, which opens only plain `http` and `https` URLs.
#[cfg(windows)]
async fn open_url(url: &str) -> Result<(), RpcError> {
    if url.is_empty() {
        return Err(RpcError::new("no_url", Value::Null, 3));
    }
    let failed = |message: String| RpcError::new("open_failed", json!({"message": message}), 1);
    let url = url.to_owned();
    tokio::task::spawn_blocking(move || devlanes_sys_windows::shell_execute::open_url(&url))
        .await
        .map_err(|error| failed(error.to_string()))?
        .map_err(|error| failed(error.to_string()))
}

/// The shell `env shell` starts when `$SHELL` is unset.
#[cfg(unix)]
fn default_shell() -> String {
    "/bin/sh".into()
}

/// The shell `env shell` starts when `$SHELL` is unset: `%ComSpec%`, which
/// Windows sets to its `cmd.exe`.
#[cfg(windows)]
fn default_shell() -> String {
    std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into())
}

/// Reads the daemon's own log file directly; works even when the daemon is
/// wedged. Windows has no `tail`, so a small follower does the same there.
#[cfg(windows)]
async fn daemon_logs(follow: bool, tail: usize) -> Result<u8, RpcError> {
    let path = paths()?.logs.join("daemon.log");
    let read = path.clone();
    // Reads and waits block; `-f` runs until the user stops it.
    tokio::task::spawn_blocking(move || {
        crate::follow::tail(&read, tail, follow, &mut std::io::stdout())
    })
    .await
    .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
    .map(|()| 0)
    .map_err(|error| {
        RpcError::new(
            "io",
            json!({"message": format!("{}: {error}", path.display())}),
            1,
        )
    })
}

/// Reads the daemon's own log file directly; works even when the daemon is wedged.
#[cfg(unix)]
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

#[cfg(test)]
mod tests {
    use devlanes_cli::clap::Parser;

    use super::*;

    /// A plain, non-interactive runtime.
    fn runtime() -> Runtime {
        let cli = Cli::parse_from(["devlanes", "--output", "plain"]);
        Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime")
    }

    /// Without a daemon, `daemon status` says so in each mode and exits 1.
    #[test]
    fn daemon_not_running_in_every_mode() {
        #[cfg(unix)]
        const HOME: &str = "/home/u/.devlanes";
        #[cfg(windows)]
        const HOME: &str = r"C:\Users\u\.devlanes";
        let paths = Paths::new(HOME).expect("paths");
        let socket = paths.socket.display().to_string();
        let mode = |name: &str| {
            let cli = Cli::parse_from(["devlanes", "--output", name]);
            Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime")
        };
        let json = daemon_not_running(&mode("json"), &paths);
        assert_eq!(json.exit_code, 1);
        let value: Value = serde_json::from_str(&json.stdout).expect("json");
        assert_eq!(value["running"], false);
        assert_eq!(value["socket"], socket);
        assert_eq!(value["home"], HOME);
        #[cfg(unix)]
        let endpoint = "socket";
        #[cfg(windows)]
        let endpoint = "pipe";
        assert_eq!(
            daemon_not_running(&mode("plain"), &paths).stdout,
            format!(
                "daemon is not running ({endpoint} {socket}); devlanes daemon start starts it\n"
            )
        );
        assert_eq!(
            daemon_not_running(&mode("agent"), &paths).stdout,
            format!("stopped daemon socket={socket}\n")
        );
    }

    /// `url open` on Windows hands the shell plain web URLs only; nothing
    /// here reaches a browser.
    #[cfg(windows)]
    #[tokio::test]
    async fn url_open_refuses_what_is_not_a_web_url() {
        assert_eq!(open_url("").await.map_err(|e| e.code), Err("no_url".into()));
        for url in [
            "file:///C:/Windows/System32/calc.exe",
            "https://example.invalid/\" --new-window \"x",
            "https://example.invalid/a b",
        ] {
            let error = open_url(url).await.expect_err(url);
            assert_eq!(error.code, "open_failed", "{url}");
            assert_eq!(error.exit_code, 1, "{url}");
        }
    }

    /// The restart wait ends once the old daemon's pid is gone.
    #[tokio::test]
    async fn restart_waits_for_the_old_pid() {
        #[cfg(unix)]
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
        #[cfg(windows)]
        let mut child = std::process::Command::new(
            devlanes_sys_windows::file::system_directory()
                .expect("System32")
                .join("cmd.exe"),
        )
        .args(["/d", "/c", "exit"])
        .spawn()
        .expect("spawn");
        let pid = i32::try_from(child.id()).expect("pid");
        child.wait().expect("reaped");
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(temp.path()).expect("paths");
        tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_daemon_exit(&paths, Some(pid)),
        )
        .await
        .expect("returns at once")
        .expect("gone");
    }

    /// A restart with an invalid user config fails with the strict load's
    /// `config_error` before it reaches the running daemon, which keeps its lanes.
    #[tokio::test]
    async fn restart_refuses_an_invalid_user_config_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        #[cfg_attr(windows, expect(unused_mut, reason = "the pipe name stays"))]
        let mut paths = Paths::new(temp.path().join("h")).expect("paths");
        #[cfg(unix)]
        {
            paths.socket = paths.run.join("daemon.sock");
        }
        paths.prepare().expect("prepare");
        let config = paths.home.join("config.yaml");
        std::fs::write(&config, "tui:\n  theme: neon\n").expect("config");
        let mut daemon = devlanes_ipc::local::LocalListener::bind(&paths.socket)
            .await
            .expect("socket");
        let context = devlanes_proto::Context {
            yes: true,
            ..devlanes_proto::Context::default()
        };
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            daemon_restart(&runtime(), &paths, context),
        )
        .await
        .expect("in time")
        .expect_err("invalid config");
        assert_eq!(error.code, "config_error");
        assert_eq!(error.data["file"], config.display().to_string());
        assert!(
            tokio::time::timeout(Duration::from_millis(100), daemon.accept())
                .await
                .is_err(),
            "the running daemon was never contacted"
        );
    }

    /// An older compatible daemon gets both versions named; another protocol also
    /// gets the restart command.
    #[test]
    fn version_notes_name_both_versions() {
        let mut peer = devlanes_proto::Hello::current(0);
        assert_eq!(version_note(&peer), None);
        peer.version = "0.0.1".into();
        let note = version_note(&peer).expect("note");
        assert!(
            note.contains("0.0.1") && note.contains(env!("CARGO_PKG_VERSION")),
            "{note}"
        );
        peer.protocol += 1;
        assert!(version_note(&peer).is_some_and(|note| note.contains("daemon restart")));
    }

    /// Offline reset never prompts: without --yes it refuses with the flag to add,
    /// with --yes it moves the database aside with the lane data folders, whose
    /// lane ids a new database hands out again. An invalid user config, which
    /// names the data folder, moves nothing.
    #[tokio::test]
    async fn offline_reset_needs_yes_and_moves_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut paths = Paths::new(temp.path().join("h")).expect("paths");
        paths.socket = paths.run.join("daemon.sock");
        paths.prepare().expect("prepare");
        std::fs::write(&paths.db, "old").expect("db");
        let lane = paths.home.join("data/acme/1");
        std::fs::create_dir_all(&lane).expect("lane data");
        std::fs::write(lane.join("dump.sql"), "rows").expect("lane file");
        // A process another test forks in the same moment shares the lock file's
        // description until it execs, so the lock can look taken for that instant.
        let reset = async |yes: bool| {
            let mut result = reset_state_offline(&runtime(), &paths, yes).await;
            for _ in 0..50 {
                if !matches!(&result, Err(error) if error.code == "daemon_running") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                result = reset_state_offline(&runtime(), &paths, yes).await;
            }
            result
        };
        let config = paths.home.join("config.yaml");
        std::fs::write(&config, "tui:\n  theme: neon\n").expect("config");
        let invalid = reset(true).await.expect_err("invalid config");
        assert_eq!(invalid.code, "config_error");
        assert!(paths.db.exists() && lane.exists(), "nothing moved");
        std::fs::remove_file(&config).expect("valid again");
        let refused = reset(false).await.expect_err("needs --yes");
        assert_eq!(refused.code, "confirmation_required");
        assert_eq!(refused.data["flag"], "--yes");
        assert!(paths.db.exists(), "nothing moved without --yes");
        assert_eq!(reset(true).await, Ok(0));
        assert!(!paths.db.exists(), "moved aside");
        assert!(!lane.exists(), "lane data moved aside");
        let aside: Vec<std::path::PathBuf> = std::fs::read_dir(paths.home.join("data"))
            .expect("data root")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(
            aside[0]
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(".legacy-")),
            "{aside:?}"
        );
        assert_eq!(
            std::fs::read_to_string(aside[0].join("acme/1/dump.sql")).expect("kept"),
            "rows"
        );
    }

    /// Hints keep the lane and project the user chose.
    #[test]
    fn log_hints_keep_lane_and_project() {
        let request = Request {
            id: 0,
            method: Method::Logs,
            params: json!({"units": ["api"], "follow": true}),
            context: devlanes_proto::Context {
                lane: Some("feat/x".into()),
                project: Some("acme".into()),
                ..devlanes_proto::Context::default()
            },
        };
        assert_eq!(
            more_hint(Method::Logs, &request)
                .map(|hint| hint.0)
                .as_deref(),
            Some("devlanes logs api --tail 500 --lane feat/x --project acme")
        );
        // A unit that does not exist points at what lists units; a task at run --list.
        let missing = RpcError::new("not_found", json!({"reference": "apu"}), 3);
        assert_eq!(
            hint_for(&missing, &request).map(|hint| hint.0).as_deref(),
            Some("devlanes status --lane feat/x --project acme")
        );
        let run = Request {
            method: Method::Run,
            ..request.clone()
        };
        assert_eq!(
            hint_for(&missing, &run).map(|hint| hint.0).as_deref(),
            Some("devlanes run --list")
        );
        // A config error found by `config check` points at the file, not at itself.
        let check = Request {
            method: Method::ConfigCheck,
            ..request.clone()
        };
        let invalid = RpcError::new(
            "config_error",
            json!({"file": "devlanes.yaml", "line": 4, "message": "unknown field"}),
            1,
        );
        assert_eq!(
            hint_for(&invalid, &check).map(|hint| hint.0).as_deref(),
            Some("edit devlanes.yaml at line 4, then run this again")
        );
        // An oversized logs answer points at a shorter read of the first unit.
        let too_large = RpcError::new("response_too_large", json!({}), 1);
        assert_eq!(
            hint_for(&too_large, &request).map(|hint| hint.0).as_deref(),
            Some("devlanes logs api --tail 200 --lane feat/x --project acme")
        );
        // A foreground run learns its unit (or that it has none) from its result.
        let run = Request {
            method: Method::Run,
            params: json!({"task": "seed"}),
            ..request
        };
        assert!(matches!(streamed_output(&run), Some(None)));
        let watch = Request {
            method: Method::Status,
            params: json!({"watch": true}),
            ..run
        };
        assert!(matches!(streamed_output(&watch), Some(None)));
    }
}

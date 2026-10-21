//! Executes CLI actions: RPCs with streaming, local terminal work, and `with`.

use std::time::Duration;

use devlanes_cli::{
    AGENT_GUIDE, Action, Cli, Hint, InterruptPolicy, Rendered, Report, ReporterStream, Runtime,
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

/// Ctrl-C exit status when a client detaches; managed units keep running.
const DETACHED: u8 = 130;

/// Routes one action and returns the process exit status.
pub async fn dispatch(cli: &Cli, runtime: &Runtime, action: Action) -> u8 {
    match run(cli, runtime, action).await {
        Ok(code) => code,
        Err(error) => {
            let hint = confirmation_hint(&error)
                .or_else(|| legacy_state_hint(&error))
                .or_else(|| fix_hint(&error));
            let rendered = runtime.reporter().error(&error, hint.as_ref());
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
        // The daemon's codes (not_found, terminal_required, ...) keep their wording
        // and exit status; only local terminal failures become attach_failed.
        Action::Attach(request) => {
            attach::run(&paths()?, request).await.map_err(|error| {
                match error.downcast::<devlanes_client::Error>() {
                    Ok(error) => rpc_error(error),
                    Err(error) => {
                        RpcError::new("attach_failed", json!({"message": error.to_string()}), 1)
                    }
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
        Action::ExportEnv(request) => {
            let data = call(request).await?;
            let env = serde_json::from_value(data.get("env").cloned().unwrap_or_default())
                .unwrap_or_default();
            let exported = export_environment(&env);
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
            // Any protocol: restarting is how an incompatible daemon gets replaced.
            match Client::connect_any(&paths).await {
                Ok(mut client) => {
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
                    wait_for_daemon_exit(&paths, pid).await?;
                }
                Err(error) if error.is_not_running() => {}
                Err(error) => return Err(rpc_error(error)),
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
            // Before connecting: a signal from here on cancels the start and still
            // reaches the teardown instead of killing this process.
            let signals = Signals::install()
                .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
            let client = connect(&paths).await?;
            let progress = crate::progress::wrap(&paths, runtime);
            let mut driver = ClientDriver::new(client, plan.start.context.clone(), paths)
                .with_signals(signals)
                .with_progress(progress);
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

/// Longest `daemon restart` waits for the old daemon, which first stops every unit
/// with its stop policy.
const DAEMON_EXIT_WAIT: Duration = Duration::from_secs(300);

/// Waits until the old daemon process `pid` has exited (it releases the lock only
/// after stopping everything); without a pid, until its socket stops answering.
async fn wait_for_daemon_exit(paths: &Paths, pid: Option<i32>) -> Result<(), RpcError> {
    let deadline = tokio::time::Instant::now() + DAEMON_EXIT_WAIT;
    loop {
        let gone = match pid.and_then(rustix::process::Pid::from_raw) {
            Some(pid) => rustix::process::test_kill_process(pid) == Err(rustix::io::Errno::SRCH),
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
        // `daemon status` also answers on any protocol; with no daemon it starts one,
        // like every other command.
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
            Err(error) if error.is_not_running() => connect(&paths).await?,
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
    let progress = crate::progress::Follower::start(&paths, runtime, &request);
    let id = client
        .begin_request(method, request.params.clone(), request.context.clone())
        .await
        .map_err(rpc_error)?;
    let reporter = runtime.reporter();
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
                    progress.event(&event);
                    let rendered = match log_stream.as_mut() {
                        Some(stream) => stream.event(&event),
                        None => reporter.event(method, &event),
                    };
                    if let Ok(rendered) = rendered {
                        emit(&rendered);
                    }
                }
                Frame::Control(Control::Response(response)) if response.id == id => {
                    return match response.result {
                        Outcome::Ok { mut data } => {
                            if method == Method::Doctor {
                                crate::notices::annotate_doctor(&mut client, &mut data).await;
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
/// lock, never deleting them. The next command spawns a daemon with an empty database.
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
    let moves = blocking(move || crate::daemon::move_state_aside(&home, &db, &stamp)).await?;
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
        "secrets_locked" => Some(Hint::command(["secrets", "unlock"])),
        "unit_failed" | "ready_timeout" => {
            let unit = error.data.get("unit").and_then(Value::as_str)?;
            // The daemon's canonical lane wins; otherwise the user's own selectors.
            Some(match error.data.get("lane").and_then(Value::as_str) {
                Some(lane) => Hint::logs(lane, unit, 200),
                None => scoped_logs(request, &[unit, "--tail", "200"]),
            })
        }
        "confirmation_required" => confirmation_hint(error),
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

#[cfg(test)]
mod tests {
    use devlanes_cli::clap::Parser;

    use super::*;

    /// A plain, non-interactive runtime.
    fn runtime() -> Runtime {
        let cli = Cli::parse_from(["devlanes", "--output", "plain"]);
        Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime")
    }

    /// The restart wait ends once the old daemon's pid is gone.
    #[tokio::test]
    async fn restart_waits_for_the_old_pid() {
        let mut child = std::process::Command::new("true").spawn().expect("spawn");
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
    /// with --yes it moves the database aside.
    #[tokio::test]
    async fn offline_reset_needs_yes_and_moves_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut paths = Paths::new(temp.path().join("h")).expect("paths");
        paths.socket = paths.run.join("daemon.sock");
        paths.prepare().expect("prepare");
        std::fs::write(&paths.db, "old").expect("db");
        let refused = reset_state_offline(&runtime(), &paths, false)
            .await
            .expect_err("needs --yes");
        assert_eq!(refused.code, "confirmation_required");
        assert_eq!(refused.data["flag"], "--yes");
        assert!(paths.db.exists(), "nothing moved without --yes");
        assert_eq!(reset_state_offline(&runtime(), &paths, true).await, Ok(0));
        assert!(!paths.db.exists(), "moved aside");
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

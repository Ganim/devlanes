use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{
    Actor, ActorKind, Event, LogLine, Method, Request, RpcError, Selection, events,
};
use serde_json::json;

/// Injects only test markers; never mutates the test runner's environment.
fn ci_runtime(cli: &Cli, vars: &[(&str, &str)]) -> Runtime {
    let signals = Signals {
        stdout_tty: true,
        stdin_tty: true,
        no_color: true,
        ..Signals::default()
    }
    .with_ci_env(|key| {
        vars.iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| (*value).into())
    });
    Runtime::resolve(cli, signals).expect("runtime")
}

#[test]
fn detection_table_and_precedence() {
    let cli = Cli::parse_from(["devlanes"]);
    let mut table = Vec::new();
    for (key, value) in [
        ("CI", ""),
        ("CI", "0"),
        ("CI", "false"),
        ("CI", "FALSE"),
        ("CI", "true"),
        ("GITHUB_ACTIONS", "true"),
        ("GITLAB_CI", "true"),
        ("BUILDKITE", "true"),
        ("CIRCLECI", "true"),
        ("TF_BUILD", "True"),
        ("JENKINS_URL", "https://jenkins.invalid"),
        ("TEAMCITY_VERSION", "2026.1"),
        ("BITBUCKET_BUILD_NUMBER", "42"),
    ] {
        let runtime = ci_runtime(&cli, &[(key, value)]);
        table.push(format!("{key}={value}: {}", runtime.output_description()));
        if runtime.ci.is_some() {
            assert!(!runtime.can_prompt());
            assert!(matches!(
                cli.action_with(&runtime, PathBuf::new(), BTreeMap::new)
                    .expect("action"),
                Action::Rpc(_)
            ));
        }
    }
    insta::assert_snapshot!(table.join("\n"), @r#"
    CI=: human
    CI=0: human
    CI=false: human
    CI=FALSE: human
    CI=true: plain (CI: generic)
    GITHUB_ACTIONS=true: plain (CI: github-actions)
    GITLAB_CI=true: plain (CI: gitlab)
    BUILDKITE=true: plain (CI: buildkite)
    CIRCLECI=true: plain (CI: circleci)
    TF_BUILD=True: plain (CI: azure-pipelines)
    JENKINS_URL=https://jenkins.invalid: plain (CI: jenkins)
    TEAMCITY_VERSION=2026.1: plain (CI: teamcity)
    BITBUCKET_BUILD_NUMBER=42: plain (CI: bitbucket)
    "#);
    let signals = Signals {
        ci: Some(CiKind::GithubActions),
        detected_actor: Some(Actor {
            kind: ActorKind::Agent,
            ..Actor::default()
        }),
        ..Signals::default()
    };
    assert_eq!(
        Runtime::resolve(&cli, signals.clone())
            .expect("runtime")
            .mode,
        OutputMode::Agent
    );
    let cli = Cli::parse_from(["devlanes", "--json"]);
    assert_eq!(
        Runtime::resolve(&cli, signals.clone())
            .expect("runtime")
            .mode,
        OutputMode::Json
    );
    let cli = Cli::parse_from(["devlanes", "--json", "--output", "human"]);
    assert_eq!(
        Runtime::resolve(&cli, signals).expect("runtime").mode,
        OutputMode::Human
    );
    let cli = Cli::parse_from(["devlanes"]);
    for provider in [
        CiKind::GithubActions,
        CiKind::Gitlab,
        CiKind::Generic,
        CiKind::Jenkins,
    ] {
        let signals = Signals {
            ci: Some(provider),
            stdout_tty: true,
            ..Signals::default()
        };
        assert_eq!(
            Runtime::resolve(&cli, signals.clone())
                .expect("runtime")
                .color,
            provider.supports_ansi()
        );
        assert!(
            Runtime::resolve(
                &cli,
                Signals {
                    force_color: true,
                    ..signals.clone()
                }
            )
            .expect("runtime")
            .color
        );
        assert!(
            !Runtime::resolve(
                &cli,
                Signals {
                    force_color: true,
                    no_color: true,
                    ..signals
                }
            )
            .expect("runtime")
            .color
        );
    }
}

#[tokio::test]
async fn github_escaping_and_append_only_summary() {
    let temp = tempfile::tempdir().expect("tempdir");
    let path = temp.path().join("summary");
    let provider = GithubActions {
        summary_path: Some(path.clone()),
    };
    let failure = UnitFailure {
        unit: "api:worker,100%\r\n::evil".into(),
        exit_code: Some(3),
        log_tail: vec![
            "50%\r\n::error::not another annotation".into(),
            "a:b,c".into(),
        ],
        ..UnitFailure::default()
    };
    insta::assert_snapshot!(provider.error_annotation(&failure), @"::error title=api%3Aworker%2C100%25%0D%0A%3A%3Aevil exited 3::50%25%0D%0A::error::not another annotation%0Aa:b,c");
    assert_eq!(
        provider.group_start("infra\n::error::oops%"),
        "::group::infra%0A::error::oops%25\n"
    );
    let summary = resource_summary(&[
        ResourcePeak {
            unit: "api".into(),
            peak_memory_bytes: Some(2 * 1_048_576),
            peak_cpu_percent: Some(125.5),
        },
        ResourcePeak {
            unit: "worker".into(),
            ..ResourcePeak::default()
        },
    ]);
    provider
        .append_summary("Earlier step\n")
        .await
        .expect("append");
    provider.append_summary(&summary).await.expect("append");
    insta::assert_snapshot!(tokio::fs::read_to_string(path).await.expect("summary"), @r#"
    Earlier step
    | Unit | Peak memory | Peak CPU |
    | --- | ---: | ---: |
    | api | 2.0 MiB | 125.5% |
    | worker | – | – |
    "#);
}

#[test]
fn phase_groups_and_failed_unit_annotation() {
    let cli = Cli::parse_from(["devlanes", "start"]);
    let runtime = ci_runtime(&cli, &[("GITHUB_ACTIONS", "true")]);
    let reporter = runtime.reporter();
    let mut output = String::new();
    for data in [
        json!({"lane":"acme@0","phase":"infra","state":"started"}),
        json!({"lane":"acme@0","phase":"infra","state":"done"}),
    ] {
        output.push_str(
            &reporter
                .event(
                    Method::Start,
                    &Event {
                        seq: 1,
                        event: events::PHASE.into(),
                        data,
                    },
                )
                .expect("event")
                .stdout,
        );
    }
    let data = json!({"unit":{"name":"api","lane":"acme@0","state":"failed","exit_code":1,"log_tail":["missing config", "at app.ts:1"]}});
    output.push_str(
        &reporter
            .event(
                Method::Start,
                &Event {
                    seq: 2,
                    event: events::UNIT_CHANGED.into(),
                    data: data.clone(),
                },
            )
            .expect("event")
            .stdout,
    );
    output.push_str(
        &reporter
            .result(&Report::new(Method::Start, &data))
            .expect("result")
            .stdout,
    );
    assert_eq!(output.matches("::error title=").count(), 1);
    insta::assert_snapshot!(output, @r#"
    ::group::infra
    lane.phase lane=acme@0 phase=infra state=started
    lane.phase lane=acme@0 phase=infra state=done
    ::endgroup::
    ::error title=api exited 1::missing config%0Aat app.ts:1
    unit.changed unit={"name":"api","lane":"acme@0","state":"failed","exit_code":1,"log_tail":["missing config","at app.ts:1"]}
    unit: {"name":"api","lane":"acme@0","state":"failed","exit_code":1,"log_tail":["missing config","at app.ts:1"]}
    "#);
}

#[test]
fn with_stop_and_all_logs_parse() {
    let cli = Cli::parse_from([
        "devlanes",
        "with",
        "+api",
        "--profile",
        "e2e",
        "--timeout",
        "2m",
        "--logs-dir",
        "artifacts",
        "--keep",
        "--",
        "pnpm",
        "test:e2e",
        "--reporter",
        "json",
    ]);
    let runtime = ci_runtime(&cli, &[("CI", "true")]);
    assert_eq!(
        cli.interrupt_policy(&runtime),
        InterruptPolicy::ForwardCommandThenTeardown
    );
    let Action::With(plan) = cli
        .action_with(&runtime, PathBuf::from("/fixture"), || {
            BTreeMap::from([("CLIENT".into(), "yes".into())])
        })
        .expect("action")
    else {
        panic!("with action")
    };
    assert_eq!(plan.command, ["pnpm", "test:e2e", "--reporter", "json"]);
    assert_eq!(plan.start.params["timeout_ms"], 120_000);
    assert_eq!(plan.start.params["units"], json!(["+api"]));
    assert!(plan.keep);
    assert_eq!(plan.start.context.env.expect("env")["CLIENT"], "yes");
    let cli = Cli::parse_from(["devlanes", "logs", "--all", "--since", "start"]);
    let Action::Rpc(request) = cli
        .action_with(&runtime, PathBuf::new(), BTreeMap::new)
        .expect("action")
    else {
        panic!("rpc")
    };
    assert_eq!(request.params["all"], true);
    assert_eq!(request.params["since"], "start");
    assert!(Cli::try_parse_from(["devlanes", "logs", "api", "--all"]).is_err());
    assert!(Cli::try_parse_from(["devlanes", "with"]).is_err());
    let cli = Cli::parse_from(["devlanes", "stop", "--logs-dir", "artifacts"]);
    assert!(matches!(
        cli.action_with(&runtime, PathBuf::new(), BTreeMap::new)
            .expect("action"),
        Action::StopWithLogs { .. }
    ));
}

struct FakeDriver {
    calls: Vec<String>,
    ready: bool,
    start_error: bool,
    stop_error: bool,
    command_exit: CommandExit,
    spawned: bool,
    whole_lane: bool,
}

impl Default for FakeDriver {
    /// Starts only api in an existing lane, keeping unrelated units out of the stop selection.
    fn default() -> Self {
        Self {
            calls: Vec::new(),
            ready: true,
            start_error: false,
            stop_error: false,
            command_exit: CommandExit::Code(0),
            spawned: true,
            whole_lane: false,
        }
    }
}

#[async_trait]
impl WithDriver for FakeDriver {
    /// Ownership remains available when readiness or transport fails.
    async fn start(
        &mut self,
        request: &Request,
        sink: &mut (dyn FnMut(Event) + Send),
    ) -> Result<WithStart, RpcError> {
        self.calls.push("start".into());
        assert_eq!(request.method, Method::Start);
        sink(Event {
            seq: 1,
            event: events::PHASE.into(),
            data: json!({"lane":"acme@0", "phase":"services", "state":"started"}),
        });
        if self.start_error {
            return Err(RpcError::new("connection_lost", json!({}), 4));
        }
        Ok(WithStart {
            ready: self.ready,
            failures: if self.ready {
                vec![]
            } else {
                vec![UnitFailure {
                    lane: "acme@0".into(),
                    unit: "api".into(),
                    exit_code: Some(1),
                    log_tail: vec![],
                }]
            },
        })
    }
    /// No test can accidentally stop all units in another lane.
    fn ownership(&self) -> WithOwnership {
        WithOwnership {
            lane: "acme@0".into(),
            units: vec!["acme@0:api".into()],
            whole_lane: self.whole_lane,
            daemon_spawned: self.spawned,
        }
    }
    /// Lane values override the shell without removing unrelated shell values.
    async fn env(&mut self, lane: &str) -> Result<BTreeMap<String, String>, RpcError> {
        assert_eq!(lane, "acme@0");
        self.calls.push("env".into());
        Ok(BTreeMap::from([
            ("LAYER".into(), "lane".into()),
            (
                "DEVLANES_PUBLIC_URL_API".into(),
                "https://api.acme.localhost".into(),
            ),
            ("SSL_CERT_FILE".into(), "/ca.pem".into()),
        ]))
    }
    /// Artifact queries must never inherit the agent's hundred-line presentation default.
    async fn logs(&mut self, lane: &str, query: LaneLogQuery) -> Result<Vec<UnitLogs>, RpcError> {
        assert_eq!(lane, "acme@0");
        assert!(query.all);
        assert_eq!(query.query.since.as_deref(), Some("start"));
        assert_eq!(query.query.tail, None);
        assert_eq!(query.query.max_bytes, None);
        self.calls.push("logs".into());
        Ok(vec![UnitLogs {
            unit: "api".into(),
            lines: vec![LogLine {
                unit: "api".into(),
                seq: 1,
                timestamp_ms: 1,
                text: "\x1b[31mmissing config\x1b[0m".into(),
                level: None,
            }],
        }])
    }
    /// Peak fixtures differ from instantaneous resource usage by contract.
    async fn metrics(&mut self, _lane: &str) -> Result<Vec<ResourcePeak>, RpcError> {
        self.calls.push("metrics".into());
        Ok(vec![ResourcePeak {
            unit: "api".into(),
            peak_memory_bytes: Some(1_048_576),
            peak_cpu_percent: Some(20.0),
        }])
    }
    /// Selective cleanup must not expand to a whole-lane stop.
    async fn stop(&mut self, lane: &str, selection: Selection) -> Result<(), RpcError> {
        assert_eq!(lane, "acme@0");
        assert_eq!(
            selection.units,
            if self.whole_lane {
                Vec::<String>::new()
            } else {
                vec!["acme@0:api".into()]
            }
        );
        self.calls.push("stop".into());
        if self.stop_error {
            Err(RpcError::new("stop_failed", json!({}), 1))
        } else {
            Ok(())
        }
    }
    /// Tests require daemon teardown even when unit teardown failed.
    async fn stop_daemon_if_spawned(&mut self) -> Result<(), RpcError> {
        self.calls.push("daemon.stop".into());
        Ok(())
    }
    /// Signal forwarding belongs to the driver; orchestration consumes its final status.
    async fn spawn_command(
        &mut self,
        argv: &[String],
        env: &BTreeMap<String, String>,
        cwd: &Path,
    ) -> Result<CommandExit, RpcError> {
        self.calls.push("command".into());
        assert_eq!(argv, ["tests", "--all"]);
        assert_eq!(cwd, Path::new("/fixture"));
        assert_eq!(env["LAYER"], "lane");
        assert_eq!(env["CLIENT"], "kept");
        assert_eq!(env["SSL_CERT_FILE"], "/ca.pem");
        assert_eq!(env["DEVLANES_PUBLIC_URL_API"], "https://api.acme.localhost");
        Ok(self.command_exit)
    }
}

/// Builds a plan with a tempdir artifact destination and synthetic shell environment.
fn plan(directory: &Path, keep: bool) -> (WithPlan, Runtime) {
    let cli = Cli::parse_from(["devlanes", "with", "--", "tests", "--all"]);
    let runtime = ci_runtime(&cli, &[("GITHUB_ACTIONS", "true")]);
    let Action::With(mut plan) = cli
        .action_with(&runtime, PathBuf::from("/fixture"), || {
            BTreeMap::from([
                ("LAYER".into(), "client".into()),
                ("CLIENT".into(), "kept".into()),
            ])
        })
        .expect("plan")
    else {
        panic!("with plan")
    };
    plan.logs_dir = Some(directory.join("logs"));
    plan.keep = keep;
    (plan, runtime)
}

#[tokio::test]
async fn with_success_and_command_failure_preserve_exit_codes_and_summary() {
    let mut snapshots = BTreeMap::new();
    for code in [0, 3] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (plan, mut runtime) = plan(temp.path(), false);
        runtime.summary_path = Some(temp.path().join("summary"));
        let mut driver = FakeDriver {
            command_exit: CommandExit::Code(code),
            ..FakeDriver::default()
        };
        let mut output = String::new();
        let outcome = run_with(&plan, &runtime, &mut driver, &mut |rendered| {
            output.push_str(&rendered.stdout)
        })
        .await;
        assert_eq!(outcome.exit_code, code);
        assert!(outcome.cleanup_errors.is_empty());
        assert_eq!(
            driver.calls,
            [
                "start",
                "env",
                "command",
                "logs",
                "metrics",
                "stop",
                "daemon.stop"
            ]
        );
        assert_eq!(
            tokio::fs::read_to_string(&outcome.log_files[0])
                .await
                .expect("log"),
            "missing config\n"
        );
        assert_eq!(
            output.matches("::group::").count(),
            output.matches("::endgroup::").count()
        );
        assert!(output.contains("| api | 1.0 MiB | 20.0% |"));
        assert!(
            tokio::fs::read_to_string(runtime.summary_path.expect("path"))
                .await
                .expect("summary")
                .contains(&format!("Exit code: {code}"))
        );
        snapshots.insert(code, format!("exit={}\n{}", outcome.exit_code, output));
    }
    insta::assert_json_snapshot!(snapshots, @r#"
    {
      "0": "exit=0\n::group::services\nlane.phase lane=acme@0 phase=services state=started\n::endgroup::\n| Unit | Peak memory | Peak CPU |\n| --- | ---: | ---: |\n| api | 1.0 MiB | 20.0% |\n",
      "3": "exit=3\n::group::services\nlane.phase lane=acme@0 phase=services state=started\n::endgroup::\n| Unit | Peak memory | Peak CPU |\n| --- | ---: | ---: |\n| api | 1.0 MiB | 20.0% |\n"
    }
    "#);
}

#[tokio::test]
async fn with_start_failure_writes_logs_and_stops_without_running_command() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (plan, runtime) = plan(temp.path(), false);
    let mut driver = FakeDriver {
        ready: false,
        ..FakeDriver::default()
    };
    let mut output = String::new();
    let outcome = run_with(&plan, &runtime, &mut driver, &mut |rendered| {
        output.push_str(&rendered.stdout)
    })
    .await;
    assert_eq!(outcome.exit_code, 1);
    assert_eq!(
        driver.calls,
        ["start", "logs", "metrics", "stop", "daemon.stop"]
    );
    assert_eq!(outcome.log_files.len(), 1);
    insta::assert_snapshot!(output, @r#"
    ::group::services
    lane.phase lane=acme@0 phase=services state=started
    ::endgroup::
    ::error title=api exited 1::missing config
    failed acme@0:api exit=1
      | missing config
    | Unit | Peak memory | Peak CPU |
    | --- | ---: | ---: |
    | api | 1.0 MiB | 20.0% |
    "#);
}

#[tokio::test]
async fn with_keep_existing_daemon_signals_and_cleanup_errors() {
    for (keep, spawned, start_error, stop_error, code) in [
        (true, true, false, false, 130),
        (false, false, false, false, 130),
        (false, true, true, false, 1),
        (false, true, false, true, 130),
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let (plan, runtime) = plan(temp.path(), keep);
        let mut driver = FakeDriver {
            spawned,
            start_error,
            stop_error,
            command_exit: CommandExit::Signal(2),
            ..FakeDriver::default()
        };
        let outcome = run_with(&plan, &runtime, &mut driver, &mut |_| {}).await;
        assert_eq!(outcome.exit_code, code);
        assert_eq!(driver.calls.contains(&"stop".into()), !keep);
        assert_eq!(
            driver.calls.contains(&"daemon.stop".into()),
            !keep && spawned
        );
        assert_eq!(driver.calls.contains(&"command".into()), !start_error);
        assert_eq!(!outcome.cleanup_errors.is_empty(), stop_error);
        assert_eq!(outcome.log_files.len(), 1);
    }
}

#[tokio::test]
async fn artifact_names_are_safe_and_collision_free() {
    assert_ne!(log_filename("api/web"), log_filename("api_web"));
    assert!(!log_filename("../../outside").contains('/'));
    let temp = tempfile::tempdir().expect("tempdir");
    let files = write_log_artifacts(
        temp.path(),
        &[
            UnitLogs {
                unit: "../../outside".into(),
                lines: vec![],
            },
            UnitLogs {
                unit: "api_web".into(),
                lines: vec![],
            },
        ],
    )
    .await
    .expect("artifacts");
    assert!(files.iter().all(|path| path.parent() == Some(temp.path())));
    write_log_artifacts(
        temp.path(),
        &[UnitLogs {
            unit: "../../outside".into(),
            lines: vec![],
        }],
    )
    .await
    .expect("repeat artifact write");
}

#[tokio::test]
async fn artifact_and_summary_errors_cannot_skip_teardown() {
    let temp = tempfile::tempdir().expect("tempdir");
    let (plan, mut runtime) = plan(temp.path(), false);
    tokio::fs::write(
        plan.logs_dir.as_ref().expect("logs directory"),
        "blocking file",
    )
    .await
    .expect("fixture");
    runtime.summary_path = Some(temp.path().to_path_buf());
    let mut driver = FakeDriver {
        command_exit: CommandExit::Code(3),
        ..FakeDriver::default()
    };
    let outcome = run_with(&plan, &runtime, &mut driver, &mut |_| {}).await;
    assert_eq!(outcome.exit_code, 3);
    assert_eq!(outcome.cleanup_errors.len(), 2);
    assert!(
        driver
            .calls
            .ends_with(&["stop".into(), "daemon.stop".into()])
    );
}

#[tokio::test]
async fn stop_artifacts_and_summary_do_not_stop_daemon() {
    let temp = tempfile::tempdir().expect("tempdir");
    let cli = Cli::parse_from(["devlanes", "stop"]);
    let runtime = ci_runtime(&cli, &[("CI", "true")]);
    let mut driver = FakeDriver::default();
    let outcome = stop_with_logs(
        "acme@0",
        Selection {
            units: vec!["acme@0:api".into()],
            ..Selection::default()
        },
        temp.path(),
        &runtime,
        &mut driver,
        &mut |_| {},
    )
    .await;
    assert_eq!(outcome.exit_code, 0);
    assert_eq!(outcome.log_files.len(), 1);
    assert_eq!(driver.calls, ["logs", "metrics", "stop"]);
}

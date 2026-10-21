//! Missing arguments: which commands ask, the options, the picker and the
//! messages when nobody can pick.

use clap::Parser;
use devlanes_cli::*;
use devlanes_proto::{LaneSummary, RpcError};
use serde_json::{Value, json};

/// A runtime for `mode`; human mode without terminals, as in a pipe.
fn runtime(mode: OutputMode) -> Runtime {
    let cli = Cli::parse_from(["devlanes"]);
    Runtime::resolve(
        &cli,
        Signals {
            env_output: Some(mode.as_str().into()),
            no_color: true,
            ..Signals::default()
        },
    )
    .expect("runtime")
}

/// A lane as `status` sends it (shape captured from a real daemon).
fn lane() -> LaneSummary {
    let unit = |name: &str, state: &str, kind: &str, public: bool| {
        let mut urls = vec![json!("http://127.0.0.1:26000")];
        if public {
            urls.push(json!(format!("https://{name}.acme.localhost:8443")));
        }
        if kind != "service" {
            urls.clear();
        }
        json!({"name": name, "lane": "acme@0", "state": state, "pid": null, "exit_code": null,
               "ports": {}, "urls": urls, "actor": {"kind": "human"}, "reason": null,
               "kind": kind, "autostart": true, "hidden": false, "tags": []})
    };
    serde_json::from_value(json!({
        "reference": "acme@0", "project": "acme", "index": 0, "branch": "main",
        "path": "/work/acme", "state": "running", "config_changed": false,
        "units": [unit("api", "ready", "service", true), unit("web", "idle", "service", true),
                  unit("queue", "starting", "service", false), unit("seed", "exited", "task", false)],
    }))
    .expect("lane")
}

/// The choice for a parsed command line.
fn choice(args: &[&str]) -> Option<Choice> {
    Cli::parse_from(std::iter::once("devlanes").chain(args.iter().copied())).missing_choice()
}

#[test]
fn commands_without_their_argument_ask() {
    let asks: Vec<(String, ChoiceKind, bool)> = [
        &["url", "open"][..],
        &["url", "print"],
        &["attach"],
        &["wait"],
        &["debug", "why"],
        &["restart"],
        &["logs"],
        &["logs", "-f", "--tail", "5"],
        &["run"],
        &["lane", "clean"],
    ]
    .iter()
    .map(|args| {
        let choice = choice(args).unwrap_or_else(|| panic!("{args:?} should ask"));
        assert!(!choice.about.is_empty(), "{args:?}");
        (choice.name(), choice.kind, choice.multi)
    })
    .collect();
    insta::assert_debug_snapshot!(asks);
    // Given the argument, or with a default, nothing asks.
    for args in [
        &["url", "open", "api"][..],
        &["logs", "--all"],
        &["logs", "--mine"],
        &["run", "--list"],
        &["stop"],
        &["env", "show"],
        &["start"],
        &["lane", "clean", "acme@2"],
    ] {
        assert_eq!(choice(args), None, "{args:?}");
    }
    // The action carries the context to look options up in.
    let cli = Cli::parse_from(["devlanes", "--lane", "acme@2", "url", "open"]);
    let action = cli
        .action_with(
            &runtime(OutputMode::Plain),
            "/work/acme".into(),
            Default::default,
        )
        .expect("action");
    match action {
        Action::Choose { choice, context } => {
            assert_eq!(choice.command, ["url", "open"]);
            assert_eq!(context.lane.as_deref(), Some("acme@2"));
        }
        _ => panic!("expected a choice"),
    }
}

#[test]
fn options_follow_the_kind() {
    let lane = lane();
    let values = |kind| -> Vec<String> {
        unit_options(kind, &lane)
            .into_iter()
            .map(|option| format!("{}  {}", option.value, option.detail))
            .collect()
    };
    insta::assert_debug_snapshot!(
        (
            values(ChoiceKind::Url),
            values(ChoiceKind::Running),
            values(ChoiceKind::Unit),
            values(ChoiceKind::Target)
        ),
        @r#"
    (
        [
            "api  ready · https://api.acme.localhost:8443",
            "web  idle · https://web.acme.localhost:8443",
            "queue  starting · http://127.0.0.1:26000",
        ],
        [
            "api  ready · https://api.acme.localhost:8443",
            "queue  starting · http://127.0.0.1:26000",
        ],
        [
            "api  ready · https://api.acme.localhost:8443",
            "web  idle · https://web.acme.localhost:8443",
            "queue  starting · http://127.0.0.1:26000",
            "seed  exited · task",
        ],
        [
            "acme@0  the whole lane · running",
            "api  ready · https://api.acme.localhost:8443",
            "web  idle · https://web.acme.localhost:8443",
            "queue  starting · http://127.0.0.1:26000",
            "seed  exited · task",
        ],
    )
    "#
    );
    let tasks = task_options(&json!({"items": [
        {"name": "migrate", "when": "lane.start"}, {"name": "seed", "when": null},
    ]}));
    assert_eq!(tasks[0].detail, "runs on lane.start");
    assert_eq!(tasks[1].detail, "on demand");
    assert_eq!(
        lane_for_cwd(std::slice::from_ref(&lane), "/work/acme/web/src".as_ref())
            .map(|lane| lane.reference.as_str()),
        Some("acme@0")
    );
    assert!(lane_for_cwd(std::slice::from_ref(&lane), "/elsewhere".as_ref()).is_none());
}

#[test]
fn missing_argument_lists_options_and_the_command() {
    let choice = choice(&["url", "open"]).expect("choice");
    let options = unit_options(ChoiceKind::Url, &lane());
    let args: Vec<String> = ["--lane", "acme@0", "url", "open"]
        .map(str::to_owned)
        .to_vec();
    let mut output = String::new();
    for mode in [OutputMode::Human, OutputMode::Agent, OutputMode::Json] {
        let rendered = missing_argument(&choice, &options, &args, &runtime(mode));
        assert_eq!(rendered.exit_code, 2);
        assert!(rendered.stdout.is_empty());
        output.push_str(&format!("--- {}\n{}", mode.as_str(), rendered.stderr));
    }
    insta::assert_snapshot!(output);
    let json: Value = serde_json::from_str(
        &missing_argument(&choice, &options, &args, &runtime(OutputMode::Json)).stderr,
    )
    .expect("json");
    assert_eq!(json["error"]["data"]["options"][0]["value"], "api");
    assert_eq!(json["error"]["data"]["total"], 3);
    // Long lists are capped with a count.
    let many: Vec<ChoiceOption> = (0..30)
        .map(|n| ChoiceOption {
            value: format!("unit{n}"),
            detail: "idle".into(),
        })
        .collect();
    let rendered = missing_argument(&choice, &many, &args, &runtime(OutputMode::Plain));
    assert!(
        rendered.stderr.contains("and 10 more"),
        "{}",
        rendered.stderr
    );
    // A value goes before `--`.
    assert_eq!(
        completed_args(&["run".into(), "--".into(), "x".into()], &["seed".into()]),
        ["run", "seed", "--", "x"]
    );
}

#[test]
fn no_options_say_why() {
    let mut output = String::new();
    for (args, why) in [
        (
            &["url", "open"][..],
            NoOptions::NoConfig {
                cwd: "/work/empty".into(),
            },
        ),
        (
            &["attach"],
            NoOptions::NoLane {
                cwd: "/work/acme".into(),
            },
        ),
        (
            &["url", "open"],
            NoOptions::Empty {
                lane: "acme@0".into(),
            },
        ),
        (
            &["attach"],
            NoOptions::Empty {
                lane: "acme@0".into(),
            },
        ),
        (
            &["run"],
            NoOptions::Empty {
                lane: "acme@0".into(),
            },
        ),
    ] {
        let choice = choice(args).expect("choice");
        for mode in [OutputMode::Plain, OutputMode::Json] {
            let rendered = no_options(&choice, &why, &runtime(mode));
            output.push_str(&format!(
                "--- {} {} (exit {})\n{}",
                choice.name(),
                mode.as_str(),
                rendered.exit_code,
                rendered.stderr
            ));
        }
    }
    assert!(!output.contains("required"), "{output}");
    insta::assert_snapshot!(output);
}

#[test]
fn picker_moves_filters_and_chooses() {
    let options = unit_options(ChoiceKind::Unit, &lane());
    let mut single = Picker::new(
        "devlanes debug why: choose a unit".into(),
        options.clone(),
        false,
    );
    insta::assert_snapshot!(single.lines(10).join("\n"), @r"
    devlanes debug why: choose a unit
    > api    ready · https://api.acme.localhost:8443
      web    idle · https://web.acme.localhost:8443
      queue  starting · http://127.0.0.1:26000
      seed   exited · task
    up/down move · enter choose · type to filter · esc cancel
    ");
    assert_eq!(single.key(PickerKey::Down), PickerStep::Continue);
    assert_eq!(
        single.key(PickerKey::Enter),
        PickerStep::Done(vec!["web".into()])
    );
    // Typing filters; the cursor goes back to the first match.
    let mut filtered = Picker::new("t".into(), options.clone(), false);
    filtered.key(PickerKey::Char('e'));
    filtered.key(PickerKey::Char('e'));
    assert_eq!(
        filtered.key(PickerKey::Enter),
        PickerStep::Done(vec!["seed".into()])
    );
    let mut nothing = Picker::new("t".into(), options.clone(), false);
    nothing.key(PickerKey::Char('z'));
    assert!(
        nothing
            .lines(10)
            .iter()
            .any(|line| line.contains("no match"))
    );
    assert_eq!(nothing.key(PickerKey::Enter), PickerStep::Continue);
    assert_eq!(nothing.key(PickerKey::Cancel), PickerStep::Cancel);
    // Several: space toggles, enter returns the chosen ones in list order.
    let mut multi = Picker::new("devlanes logs: choose units".into(), options, true);
    multi.key(PickerKey::Down);
    multi.key(PickerKey::Down);
    multi.key(PickerKey::Toggle);
    multi.key(PickerKey::Up);
    multi.key(PickerKey::Up);
    multi.key(PickerKey::Toggle);
    insta::assert_snapshot!(multi.lines(10).join("\n"), @r"
    devlanes logs: choose units
    > [x] api    ready · https://api.acme.localhost:8443
      [ ] web    idle · https://web.acme.localhost:8443
      [x] queue  starting · http://127.0.0.1:26000
      [ ] seed   exited · task
    up/down move · space choose · enter confirm · type to filter · esc cancel
    ");
    assert_eq!(
        multi.key(PickerKey::Enter),
        PickerStep::Done(vec!["api".into(), "queue".into()])
    );
}

#[test]
fn typos_get_a_suggestion() {
    let reporter = runtime(OutputMode::Plain).reporter();
    let typo = RpcError::new(
        "not_found",
        json!({"reference": "acme@0:apu", "candidates": ["api", "web", "queue"]}),
        3,
    );
    let rendered = reporter.error(&typo, None);
    assert!(
        rendered
            .stderr
            .contains("acme@0:apu not found; did you mean api?; options: api, web, queue"),
        "{}",
        rendered.stderr
    );
    let many: Vec<String> = (0..20).map(|n| format!("unit{n}")).collect();
    let far = RpcError::new(
        "not_found",
        json!({"reference": "zzz", "candidates": many}),
        3,
    );
    let rendered = reporter.error(&far, None);
    assert!(
        rendered.stderr.contains("zzz not found; 20 options"),
        "{}",
        rendered.stderr
    );
}

#[test]
fn usage_errors_name_the_command() {
    let args: Vec<String> = ["--lane", "x", "url", "open", "--bogus"]
        .map(str::to_owned)
        .to_vec();
    assert_eq!(command_path(&args), ["url", "open"]);
    let text = usage_error("error: unexpected argument '--bogus' found", &args);
    assert!(
        text.ends_with(
            "devlanes url open: Open a service's URL in the browser; agent mode only prints it\n"
        ),
        "{text}"
    );
    assert_eq!(usage_error("error: x", &[]), "error: x\n");
}

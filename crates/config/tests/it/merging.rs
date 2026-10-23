//! Local overrides and `extends` merge mappings per key; alternative forms clear each other.

use devlanes_config::*;

/// Return a service's readiness check mapping.
fn check(service: &ServiceConfig) -> &ReadyCheck {
    match service.ready.as_ref().expect("ready") {
        ReadyConfig::Check(check) => check,
        ReadyConfig::Started(_) => panic!("expected a check"),
    }
}

/// Partial `stop`, `ready` and `run_script` mappings keep the keys they leave out.
#[test]
fn service_mappings_merge_per_key() {
    let base = "project: demo\ntemplates:\n  base:\n    stop: { signal: SIGINT, timeout: 10s }\n    ready: { http: { path: /health, status: 204 } }\nservices:\n  api:\n    extends: base\n    run_script: { file: apps/api/package.json, name: dev }\n    ready: { http: { status: 200 } }\n";
    let local = "services:\n  api:\n    stop: { timeout: 20s }\n    ready: { timeout: 5s }\n    run_script: { name: start }\n";
    for local in [None, Some(local)] {
        let config = parse_project(base, local).expect("merged");
        let service = &config.services["api"];
        let stop = service.stop.as_ref().expect("stop");
        assert_eq!(stop.signal.as_deref(), Some("SIGINT"));
        let http = check(service).http.as_ref().expect("http");
        assert_eq!(http.path, Some(Templated::Literal("/health".into())));
        assert_eq!(http.status, Some(Templated::Literal(200)));
        let script = service.run_script.as_ref().expect("script");
        let expected_name = if local.is_some() { "start" } else { "dev" };
        assert_eq!(
            script.file_and_name().expect("script"),
            ("apps/api/package.json", expected_name)
        );
        if local.is_some() {
            assert_eq!(
                stop.timeout,
                Some(Templated::Literal("20s".parse().expect("duration")))
            );
            assert_eq!(
                check(service).timeout,
                Some(Templated::Literal("5s".parse().expect("duration")))
            );
        }
    }
}

/// Setting one form of a command or readiness check drops the inherited alternatives.
#[test]
fn alternative_forms_clear_siblings() {
    let source = "project: demo\ntemplates:\n  base:\n    run: server\n    ready: { http: { path: /-/health }, timeout: 30s }\n    env: { KEEP: base, VALUE: base }\nservices:\n  api:\n    extends: base\n    run_script: { file: package.json, name: dev }\n    ready: { tcp: true }\n    env: { VALUE: service }\n";
    let config = parse_project(source, None).expect("service replaces template forms");
    let service = &config.services["api"];
    assert!(service.run.is_none());
    let ready = check(service);
    assert!(ready.http.is_none());
    assert!(matches!(ready.tcp, Some(TcpReady::Enabled(true))));
    assert!(ready.timeout.is_some(), "timeout is not an alternative");
    assert_eq!(
        service.env["KEEP"],
        Templated::Literal(Scalar::String("base".into()))
    );
    assert_eq!(
        service.env["VALUE"],
        Templated::Literal(Scalar::String("service".into()))
    );

    let local = "services:\n  api:\n    run: local-server\n    ready: { log: listening }\n    env: { VALUE: local, EXTRA: local }\n";
    let config = parse_project(source, Some(local)).expect("local replaces service forms");
    let service = &config.services["api"];
    assert!(service.run.is_some());
    assert!(service.run_script.is_none());
    let ready = check(service);
    assert!(ready.http.is_none());
    assert!(ready.tcp.is_none());
    assert_eq!(ready.log, Some(Templated::Literal("listening".into())));
    assert_eq!(
        service.env["VALUE"],
        Templated::Literal(Scalar::String("local".into()))
    );
    assert!(service.env.contains_key("EXTRA"));

    let config = parse_project(source, Some("services:\n  api:\n    ready: started\n"))
        .expect("started replaces the check");
    assert!(matches!(
        config.services["api"].ready,
        Some(ReadyConfig::Started(_))
    ));
}

/// Tasks, infra commands, containers and profiles merge per key too.
#[test]
fn other_mappings_merge_per_key() {
    let base = "project: demo\ninfra:\n  containers:\n    cache:\n      image: redis\n      ports: { main: 6379 }\n      ready: { tcp: true, timeout: 5s }\n  setup:\n    db: createdb demo\n    queue: { run: add-vhost, container: cache, ignore_errors: true }\ntasks:\n  seed:\n    run: seed\n    timeout: 1m\n    env: { A: base }\n  build:\n    steps: [seed]\nservices:\n  api: { run: server, tags: [api] }\nprofiles:\n  default: [api]\n";
    let local = "infra:\n  containers:\n    cache:\n      ready: { timeout: 30s }\n  setup:\n    db: { ignore_errors: true }\n    queue: { run: make vhost }\ntasks:\n  seed:\n    env: { B: local }\n  build:\n    run: make\nprofiles:\n  default:\n    env: { MODE: local }\n";
    let config = parse_project(base, Some(local)).expect("merged");

    let cache = &config.infra.containers["cache"];
    assert!(cache.image.is_some());
    let Some(ReadyConfig::Check(ready)) = &cache.ready else {
        panic!("container check")
    };
    assert!(matches!(ready.tcp, Some(TcpReady::Enabled(true))));

    let InfraCommand::Object(db) = &config.infra.setup["db"] else {
        panic!("shorthand merged into a mapping")
    };
    assert!(db.run.is_some(), "shorthand counts as run");
    assert_eq!(db.ignore_errors, Some(Templated::Literal(true)));
    let InfraCommand::Object(queue) = &config.infra.setup["queue"] else {
        panic!("queue mapping")
    };
    assert!(
        matches!(&queue.run, Some(Run::Shell(Templated::Literal(run))) if run == "make vhost"),
        "local run replaces the base run"
    );
    assert_eq!(queue.container.as_deref(), Some("cache"));
    assert_eq!(queue.ignore_errors, Some(Templated::Literal(true)));

    let seed = &config.tasks["seed"];
    assert!(seed.run.is_some());
    assert!(seed.timeout.is_some());
    assert_eq!(seed.env.len(), 2);
    let build = &config.tasks["build"];
    assert!(build.steps.is_none(), "run clears steps");

    let ProfileConfig::Object(profile) = &config.profiles["default"] else {
        panic!("profile mapping")
    };
    assert_eq!(profile.include, Some(vec!["api".to_owned()]));
    assert!(profile.env.contains_key("MODE"));
}

/// Preset option mappings merge per key through templates and local files; lists replace.
#[test]
fn preset_options_merge_per_key() {
    let base = "project: demo\ntemplates:\n  base:\n    preset: custom\n    custom: { nested: { keep: true, replace: old }, args: [old] }\nservices:\n  api:\n    extends: base\n    run_script: { file: package.json, name: dev }\n    custom: { nested: { replace: new }, args: [] }\n";
    let config = parse_project(base, Some("services:\n  api:\n    custom: { extra: 1 }\n"))
        .expect("merged options");
    assert_eq!(
        config.services["api"].preset_options["custom"],
        serde_json::json!({"nested":{"keep":true,"replace":"new"},"args":[],"extra":1})
    );
}

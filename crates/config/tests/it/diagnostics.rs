//! Unknown keys, wrong shapes and wrong types point at their line with a useful hint.

use devlanes_config::*;

/// Line of the first source line containing `needle`, one-based.
fn line_of(source: &str, needle: &str) -> usize {
    source
        .lines()
        .position(|line| line.contains(needle))
        .expect("needle in source")
        + 1
}

/// Parse a project that must fail and check the message and the line it points at.
fn assert_project_error(source: &str, needle: &str, expected: &str) {
    let error = parse_project(source, None).expect_err(source);
    assert!(
        error.message.contains(expected),
        "{source}\nexpected `{expected}` in: {}",
        error.message
    );
    assert_eq!(
        error.location.line,
        line_of(source, needle),
        "{source}: {}",
        error.message
    );
}

/// Typos outside `services` get the same did-you-mean hint as service keys.
#[test]
fn did_you_mean_outside_services() {
    for (source, needle, expected) in [
        (
            "project: demo\nservics:\n  api: { run: server }\n",
            "servics",
            "unknown key `servics`; did you mean `services`?",
        ),
        (
            "project: demo\ntasks:\n  migrate:\n    run: migrate\n    whne: lane.start\n",
            "whne",
            "unknown key `whne`; did you mean `when`?",
        ),
        (
            "project: demo\nlanes:\n  primay: main\n",
            "primay",
            "unknown key `primay`; did you mean `primary`?",
        ),
    ] {
        assert_project_error(source, needle, expected);
    }
    let source = "proxy:\n  htps_port: 8443\n";
    let error = parse_user(source).expect_err("user typo");
    assert!(
        error
            .message
            .contains("unknown key `htps_port`; did you mean `https_port`?"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 2);
}

/// The lenient user load drops only the keys with problems: an unknown key, a
/// value of the wrong type and a value validation refuses each become a located
/// error, and every other key keeps its value.
#[test]
fn lenient_user_config_keeps_valid_keys() {
    let source = "proxy:\n  htps_port: 8443\n  http_port: 8081\nstate:\n  max_size: 0B\nterminal: { progress: never }\nports:\n  range: [not, a, range]\n";
    let (config, issues) = parse_user_lenient("config.yaml", source);
    assert_eq!(config.proxy.http_port, 8081);
    assert_eq!(
        config.terminal.as_ref().map(TerminalConfig::progress),
        Some(TerminalProgress::Never)
    );
    assert_eq!(
        config.state.max_size.0,
        UserConfig::default().state.max_size.0
    );
    let found: Vec<(String, usize)> = issues
        .iter()
        .map(|issue| {
            let path = issue
                .message
                .split_once(':')
                .map(|(p, _)| p)
                .unwrap_or_default();
            (path.to_owned(), issue.location.line)
        })
        .collect();
    assert_eq!(
        found,
        [
            ("proxy.htps_port".to_owned(), 2),
            ("ports.range".to_owned(), 8),
            ("state.max_size".to_owned(), 5),
        ],
        "{issues:?}"
    );
    let (defaults, issues) = parse_user_lenient("config.yaml", "proxy: [unclosed\n");
    assert_eq!(
        defaults.proxy.http_port,
        UserConfig::default().proxy.http_port
    );
    assert_eq!(issues.len(), 1);
}

/// `terminal` is a mapping with the progress setting; an older config's
/// terminal app (alone or as `app:`) still loads, ignored with a warning.
#[test]
fn terminal_takes_progress_and_ignores_an_old_app() {
    let progress_only = parse_user("terminal: { progress: always }\n").expect("progress only");
    assert!(progress_only.warnings().is_empty());
    let terminal = progress_only.terminal.expect("terminal");
    assert_eq!(terminal.progress(), TerminalProgress::Always);
    let bare = parse_user("terminal: kitty\n").expect("old app name");
    assert_eq!(bare.warnings()[0].path, "terminal");
    assert!(bare.warnings()[0].message.contains("kitty is ignored"));
    let terminal = bare.terminal.expect("terminal");
    assert_eq!(terminal.progress(), TerminalProgress::Auto);
    let mapped = parse_user("terminal:\n  app: ghostty\n  progress: never\n").expect("old mapping");
    assert_eq!(mapped.warnings()[0].path, "terminal.app");
    let terminal = mapped.terminal.expect("terminal");
    assert_eq!(terminal.progress(), TerminalProgress::Never);
    let source = "terminal:\n  progress: never\n  progres: never\n";
    let error = parse_user(source).expect_err("typo");
    assert!(
        error
            .message
            .contains("unknown key `progres`; did you mean `progress`?"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 3);
}

#[test]
fn terminal_features_default_to_auto_and_parse_each_choice() {
    let config = parse_user(
        "terminal:\n  title: never\n  links: always\n  notifications: auto\n  marks: never\n",
    )
    .expect("features");
    let settings = config.terminal.expect("terminal").settings();
    assert_eq!(settings.title, TerminalFeature::Never);
    assert_eq!(settings.links, TerminalFeature::Always);
    assert_eq!(settings.notifications, TerminalFeature::Auto);
    assert_eq!(settings.marks, TerminalFeature::Never);
    assert_eq!(settings.progress, TerminalProgress::Auto);
    // An old app name keeps every feature on auto.
    let legacy = parse_user("terminal: kitty\n").expect("old app name");
    let settings = legacy.terminal.expect("terminal").settings();
    assert_eq!(settings.title, TerminalFeature::Auto);
    assert!(settings.app.is_none());
    let error = parse_user("terminal:\n  title: sometimes\n").expect_err("bad value");
    assert_eq!(error.location.line, 2, "{}", error.message);
}

/// Values with several YAML forms report the chosen form's own unknown keys.
#[test]
fn alternative_forms_keep_struct_errors() {
    for (source, needle, expected) in [
        (
            "project: demo\nservices:\n  api:\n    run: server\n    ready: { htp: { path: / } }\n",
            "htp",
            "unknown key `htp`; did you mean `http`?",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: server\n    hosts:\n      - { host: a.localhost, fallbak: true }\n",
            "fallbak",
            "unknown key `fallbak`; did you mean `fallback`?",
        ),
        (
            "project: demo\nservices:\n  api: { run: server }\nprofiles:\n  p: { includes: [api] }\n",
            "includes",
            "unknown key `includes`; did you mean `include`?",
        ),
        (
            "project: demo\ninfra:\n  setup:\n    db: { run: x, ignore_error: true }\n",
            "ignore_error",
            "unknown key `ignore_error`; did you mean `ignore_errors`?",
        ),
        (
            "project: demo\nservices:\n  api:\n    run_script: { file: package.json, nmae: dev }\n",
            "nmae",
            "unknown key `nmae`; did you mean `name`?",
        ),
        (
            "project: demo\nservices:\n  api:\n    run_script: { source: package.json, name: dev }\n",
            "source",
            "unknown key `source`; `source` is now `file`",
        ),
        (
            "project: demo\ntasks:\n  migrate:\n    run_script:\n      source: package.json\n      name: migrate\n",
            "source",
            "unknown key `source`; `source` is now `file`",
        ),
        (
            "project: demo\nservices:\n  api:\n    run_script: apps/api/package.json#dev\n",
            "run_script",
            "run_script takes a mapping with file and name, such as `{ file: apps/api/package.json, name: dev }`",
        ),
        (
            "project: demo\nglobal_env:\n  FOO: null\n",
            "FOO",
            "expected a string, boolean, or number",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: server\n    ready: start\n",
            "ready",
            "expected `started` or a readiness-check mapping",
        ),
    ] {
        assert_project_error(source, needle, expected);
    }
    for source in [
        "project: demo\nservices:\n  api:\n    run: server\n    ready: { htp: {} }\n",
        "project: demo\nglobal_env:\n  FOO: null\n",
        "project: demo\nlanes:\n  block_size: banana\n",
    ] {
        let message = parse_project(source, None).expect_err(source).message;
        for rust in [
            "ReadyConfig",
            "Scalar",
            "Templated",
            "untagged",
            "u16",
            "variant",
        ] {
            assert!(!message.contains(rust), "{message}");
        }
    }
}

/// Wrong literal and expression types name the YAML type and allow expressions.
#[test]
fn wrong_type_messages_use_yaml_words() {
    let message = parse_project("project: demo\nlanes:\n  block_size: banana\n", None)
        .expect_err("wrong type")
        .message;
    assert!(message.contains("whole number"), "{message}");
    assert!(message.contains("expression is also allowed"), "{message}");
    let error = Templated::<u16>::Expression("${{ 'x' }}".into())
        .evaluate(&ExpressionContext::default())
        .expect_err("string for a port");
    assert!(error.message.contains("whole number"), "{}", error.message);
    assert!(!error.message.contains("u16"), "{}", error.message);
}

/// `replicas` takes a plain number: an expression fails to load and says why, in YAML
/// words, while a literal still loads.
#[test]
fn replicas_rejects_expressions() {
    for value in ["'${{ lane.index + 1 }}'", "${{ 2 }}"] {
        let source =
            format!("project: demo\nservices:\n  api:\n    run: server\n    replicas: {value}\n");
        let message = parse_project(&source, None)
            .expect_err("expression replicas")
            .message;
        assert!(
            message.contains("not a `${{ ... }}` expression"),
            "{message}"
        );
        assert!(message.contains("whole number"), "{message}");
        assert!(!message.contains("u16"), "{message}");
    }
    let config = parse_project(
        "project: demo\nservices:\n  api:\n    run: server\n    replicas: 2\n",
        None,
    )
    .expect("literal replicas");
    assert_eq!(config.services["api"].replicas, Some(2));
    assert!(
        parse_project(
            "project: demo\nservices:\n  api:\n    run: server\n    replicas: 0\n",
            None
        )
        .is_err()
    );
}

/// `gui` is reserved only on services; elsewhere it is a plain unknown key.
#[test]
fn gui_is_reserved_only_on_services() {
    let message = parse_project(
        "project: demo\nservices:\n  api:\n    run: server\n    gui: {}\n",
        None,
    )
    .expect_err("service gui")
    .message;
    assert!(message.contains("`gui` is not supported yet"), "{message}");
    let message = parse_project(
        "project: demo\ntasks:\n  t:\n    run: x\n    gui: true\n",
        None,
    )
    .expect_err("task gui")
    .message;
    assert!(!message.contains("not supported yet"), "{message}");
    assert!(message.contains("unknown"), "{message}");
}

/// `on:` gets the lifecycle hint on tasks and on services.
#[test]
fn on_key_points_to_when() {
    for (source, needle) in [
        (
            "project: demo\ntasks:\n  t:\n    run: x\n    on: lane.start\n",
            "on:",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: server\n    on: lane.start\n",
            "on:",
        ),
    ] {
        assert_project_error(source, needle, "lifecycle hooks are tasks with `when`");
    }
}

/// The `set preset:` hint is only for a mapping that could be preset options.
#[test]
fn preset_hint_only_for_mappings() {
    let message = parse_project(
        "project: demo\nservices:\n  api:\n    run: server\n    custom: 42\n",
        None,
    )
    .expect_err("scalar unknown key")
    .message;
    assert!(!message.contains("set `preset"), "{message}");
    let message = parse_project_with::<super::TestExtensions>(
        "project: demo\nservices:\n  api:\n    run: server\n    other: {}\n",
        None,
    )
    .expect_err("unregistered mapping")
    .message;
    assert!(message.contains("unknown key `other`"), "{message}");
    assert!(!message.contains("set `preset"), "{message}");
}

/// Integration errors about a preset option point at that option's line.
#[test]
fn preset_option_errors_are_located() {
    let source = "project: demo\nservices:\n  web:\n    preset: vite\n    run_script: { file: package.json, name: dev }\n    vite:\n      argz: []\n";
    let config = parse_project(source, None).expect("options deferred");
    let error = validate_with_presets(&config, &super::registered_validator)
        .expect_err("unknown vite option");
    assert!(
        error.message.starts_with("services.web.vite.argz:"),
        "{}",
        error.message
    );
    assert_eq!(error.location.file, "devlanes.yaml");
    assert_eq!(error.location.line, 7);
}

/// User-config validation errors point at the field, not the top of the file.
#[test]
fn user_errors_are_located() {
    for (source, line, path) in [
        (
            "tls:\n  ca: devlanes\nports:\n  range: 30000-20000\n",
            4,
            "ports.range",
        ),
        (
            "proxy:\n  http_port: 8080\n  https_port: 8080\n",
            3,
            "proxy.https_port",
        ),
        ("state:\n  max_size: 0B\n", 2, "state.max_size"),
        (
            "tls:\n  ca: devlanes\nhostnames:\n  resolver: public\n",
            4,
            "hostnames.resolver",
        ),
        ("startup:\n  max: 2\n  initial: 4\n", 3, "startup.initial"),
    ] {
        let error = parse_user(source).expect_err(source);
        assert!(error.message.starts_with(path), "{}", error.message);
        assert_eq!(error.location.line, line, "{source}: {}", error.message);
    }
}

/// Deferred mobile trust targets load with a warning and never reach the trust adapters.
#[test]
fn deferred_trust_targets_warn() {
    let user = parse_user("tls:\n  trust: [system, ios-simulator, android-emulator]\n")
        .expect("deferred targets are ignored");
    assert_eq!(user.tls.trust, [TrustTarget::System]);
    // The opt-in Windows store is kept as written.
    let wsl = parse_user("tls:\n  trust: [system, windows]\n").expect("windows target");
    assert_eq!(wsl.tls.trust, [TrustTarget::System, TrustTarget::Windows]);
    assert!(wsl.warnings().is_empty());
    let paths: Vec<_> = user.warnings().iter().map(|w| w.path.as_str()).collect();
    assert_eq!(paths, ["tls.trust.1", "tls.trust.2"]);
    assert!(user.warnings()[0].message.contains("not supported yet"));
    assert!(parse_user("{}").expect("defaults").warnings().is_empty());
    assert!(
        parse_user("hostnames: { resolver: public }")
            .expect_err("public resolver stays fatal")
            .message
            .contains("not supported yet")
    );
}

/// Locations follow keys that themselves contain dots.
#[test]
fn dotted_keys_are_located() {
    let source = "project: demo\nglobal_env:\n  OTHER: x\n  A.B: '${{ services.missing.url }}'\n";
    assert_project_error(source, "A.B", "unknown service reference `missing`");
}

/// Hostnames that DNS cannot carry are refused at their `hosts:` line, literal or
/// templated; case and a trailing dot are accepted.
#[test]
fn invalid_hostnames_are_located() {
    for (source, needle, expected) in [
        (
            "project: demo\nservices:\n  api:\n    run: server\n    hosts: [api_v2.acme.localhost]\n",
            "hosts:",
            "services.api.hosts: `api_v2.acme.localhost`: `_` is not allowed",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: server\n    hosts: [\"api_v2.${{ lane.domain }}\"]\n",
            "hosts:",
            "services.api.hosts: `api_v2.${{ lane.domain }}`: `_` is not allowed",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: server\n    hosts: [{ host: \"api.*.localhost\" }]\n",
            "hosts:",
            "a wildcard is only allowed as a leading `*.` label",
        ),
        (
            "project: demo\ninfra:\n  containers:\n    db: { image: postgres }\n  hosts:\n    db: [db_admin.localhost]\n",
            "db_admin",
            "infra.hosts.db: `db_admin.localhost`",
        ),
    ] {
        assert_project_error(source, needle, expected);
    }
    parse_project(
        "project: demo\nservices:\n  api:\n    run: server\n    hosts: [API.acme.localhost., \"*.${{ lane.domain }}\"]\n",
        None,
    )
    .expect("case and a trailing dot are normalised later");
}

/// The project name must come from the shared base file.
#[test]
fn project_only_in_local_file_is_refused() {
    let error = parse_project(
        "services:\n  api: { run: server }\n",
        Some("project: demo\n"),
    )
    .expect_err("local-only project");
    assert!(error.message.starts_with("project:"), "{}", error.message);
    assert_eq!(error.location.file, "devlanes.yaml");
}

/// A service only the local file defines must be runnable; a near miss names the base service.
#[test]
fn local_only_service_without_run() {
    let base = "project: demo\nservices:\n  admin-web: { run: server }\n";
    let local = "services:\n  admin-wbe:\n    autostart: false\n";
    let error = parse_project(base, Some(local)).expect_err("typo in local service");
    assert!(
        error.message.contains("defined only in the local file"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains("did you mean `admin-web`?"),
        "{}",
        error.message
    );
    assert_eq!(error.location.file, "devlanes.local.yaml");
    // Positions are recorded for values, so a block mapping points at its first key.
    assert_eq!(error.location.line, 3);
}

/// The diagnostic carries the location once; messages do not repeat it.
#[test]
fn messages_do_not_repeat_the_location() {
    for source in [
        "project: demo\nservices:\n  api:\n    run: server\n    stop: { timeout: 10fortnights }\n",
        "project: demo\nlanes:\n  block_size: banana\n",
        "project: demo\nservics: {}\n",
        "project: demo\nservices: [\n",
    ] {
        let error = parse_project(source, None).expect_err(source);
        assert!(!error.message.contains(" at line "), "{}", error.message);
        assert!(error.location.line > 1, "{}", error.message);
    }
}

/// Unknown template and preset names suggest the closest declared one at the right field.
#[test]
fn template_and_preset_names_suggest() {
    assert_project_error(
        "project: demo\ntemplates:\n  base: { platform: node }\nservices:\n  api:\n    extends: bsae\n    run: server\n",
        "extends",
        "unknown template `bsae`; did you mean `base`?",
    );
    let error = parse_project_with::<super::TestExtensions>(
        "project: demo\nservices:\n  api:\n    run_script: { file: package.json, name: dev }\n    preset: custm\n",
        None,
    )
    .expect_err("typed registry");
    assert!(
        error
            .message
            .starts_with("services.api.preset: unknown preset `custm`; did you mean `custom`?"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 5);
    let source = "project: demo\nservices:\n  api:\n    run_script: { file: package.json, name: dev }\n    preset: vitee\n";
    let config = parse_project(source, None).expect("opaque presets load");
    let reject = |name: &str, _: Option<&serde_json::Value>| -> Result<(), String> {
        Err(format!("unknown preset `{name}`"))
    };
    let error = validate_with_registry(&config, &["vite", "next"], &reject, &|_| Vec::new())
        .expect_err("registry names");
    assert!(
        error
            .message
            .starts_with("services.api.preset: unknown preset `vitee`; did you mean `vite`?"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 5);
    let error = validate_with_presets(&config, &reject).expect_err("validator only");
    assert!(
        error
            .message
            .starts_with("services.api.preset: unknown preset `vitee`"),
        "{}",
        error.message
    );
}

/// `config check` reports a missing Compose file with its path and stays quiet otherwise.
#[test]
fn missing_compose_file_is_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("devlanes.yaml"),
        "project: demo\ninfra:\n  compose: [compose.yaml, compose.dev.yaml]\nservices:\n  api: { run: server }\n",
    )
    .expect("config");
    std::fs::write(dir.path().join("compose.yaml"), "services: {}\n").expect("compose");
    let error = check(dir.path()).expect_err("second file missing");
    assert!(
        error
            .message
            .starts_with("infra.compose.1: Compose file not found: "),
        "{}",
        error.message
    );
    assert!(
        error.message.ends_with("compose.dev.yaml"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 3);
    std::fs::write(dir.path().join("compose.dev.yaml"), "services: {}\n").expect("compose");
    assert!(check(dir.path()).expect("both present").warnings.is_empty());
}

/// A script runs in its file's directory, so `cwd` next to `run_script` is refused, also
/// when one of them comes from a template.
#[test]
fn cwd_with_run_script_is_refused() {
    let hint = "cwd cannot be combined with run_script, which runs in the script file's directory; remove cwd";
    for (source, needle, path) in [
        (
            "project: demo\nservices:\n  api:\n    run_script: { file: apps/api/package.json, name: dev }\n    cwd: apps/api\n",
            "cwd:",
            "services.api.cwd",
        ),
        (
            "project: demo\ntasks:\n  migrate:\n    run_script: { file: apps/api/package.json, name: migrate }\n    cwd: apps/api\n",
            "cwd:",
            "tasks.migrate.cwd",
        ),
        (
            "project: demo\ntemplates:\n  base:\n    run_script: { file: apps/api/package.json, name: dev }\nservices:\n  api:\n    extends: base\n    cwd: apps/api\n",
            "cwd: apps/api",
            "services.api.cwd",
        ),
    ] {
        assert_project_error(source, needle, &format!("{path}: {hint}"));
    }
    // The service has no `cwd` key of its own, so the error points at the service.
    let inherited = "project: demo\ntemplates:\n  base:\n    cwd: apps/api\nservices:\n  api:\n    extends: base\n    run_script: { file: apps/api/package.json, name: dev }\n";
    assert_project_error(
        inherited,
        "extends: base",
        &format!("services.api.cwd: {hint} (it may come from template `base`)"),
    );
    // A local `run_script` replaces the base `run` but keeps its `cwd`.
    let error = parse_project(
        "project: demo\ntasks:\n  migrate: { run: ./migrate, cwd: apps/api }\n",
        Some("tasks:\n  migrate: { run_script: { file: apps/api/package.json, name: migrate } }\n"),
    )
    .expect_err("merged task");
    assert!(
        error.message.starts_with("tasks.migrate.cwd: "),
        "{}",
        error.message
    );
}

/// A preset only wraps `run_script`; with `run` the error points at `run`, also when a
/// template brings the preset.
#[test]
fn preset_with_run_is_refused() {
    let hint = "`preset: vite` needs run_script, such as `run_script: { file: apps/web/package.json, name: dev }`, so the preset can place its flags; to keep this command, remove `preset` and add the preset's flags to `run` yourself";
    assert_project_error(
        "project: demo\nservices:\n  web:\n    preset: vite\n    run: pnpm exec vite\n",
        "run:",
        &format!("services.web.run: {hint}"),
    );
    assert_project_error(
        "project: demo\ntemplates:\n  web: { preset: vite }\nservices:\n  app:\n    extends: web\n    run: pnpm exec vite\n",
        "run:",
        "(`preset` or `run` may come from template `web`)",
    );
    // A local `run` replaces the inherited script and so trips the rule too.
    let error = parse_project(
        "project: demo\nservices:\n  web:\n    preset: vite\n    run_script: { file: web/package.json, name: dev }\n",
        Some("services:\n  web:\n    run: pnpm exec vite\n"),
    )
    .expect_err("local run");
    assert!(
        error.message.starts_with("services.web.run: "),
        "{}",
        error.message
    );
}

/// Infra commands take only `run`, written out in full.
#[test]
fn infra_commands_take_only_run() {
    assert_project_error(
        "project: demo\ninfra:\n  setup:\n    seed:\n      run_script: { file: Makefile, name: seed }\n",
        "run_script",
        "unknown key `run_script`; did you mean `run`?",
    );
    assert_project_error(
        "project: demo\ninfra:\n  teardown:\n    drop:\n      ignore_errors: true\n",
        "ignore_errors",
        "infra.teardown.drop: infra command requires run",
    );
}

/// The wrangler preset's own port flags in `args` fail the check at the preset block.
#[test]
fn wrangler_port_flags_in_args_are_refused() {
    let source = "project: demo\nservices:\n  api:\n    preset: wrangler\n    run_script: { file: package.json, name: dev }\n    wrangler:\n      args: [--port, '8787']\n";
    let config = parse_project(source, None).expect("options deferred");
    let error = validate_with_presets(&config, &super::registered_validator)
        .expect_err("port flag in args");
    assert!(
        error.message.starts_with("services.api.wrangler: ")
            && error.message.contains("`args` passes `--port 8787`"),
        "{}",
        error.message
    );
    // A block mapping's position is its first key.
    assert_eq!(error.location.line, line_of(source, "args:"));
}

/// `cwd` stays valid with `run`, including when `run` replaces an inherited `run_script`.
#[test]
fn cwd_with_run_is_accepted() {
    let config = parse_project(
        "project: demo\ntemplates:\n  base:\n    run_script: { file: apps/api/package.json, name: dev }\n    cwd: apps/api\nservices:\n  api:\n    extends: base\n    run: pnpm dev\n  web:\n    run_script: { file: apps/web/package.json, name: dev }\ntasks:\n  migrate: { run: ./migrate, cwd: apps/api }\ninfra:\n  setup:\n    seed: { run: make seed, cwd: infra }\n",
        None,
    )
    .expect("cwd with run");
    let api = &config.services["api"];
    assert!(api.run_script.is_none());
    assert_eq!(api.cwd, Some(Templated::Literal("apps/api".into())));
}

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
    let source = "proxy:\n  htps_port: 8443\n  http_port: 8081\nstate:\n  max_size: 0B\nterminal: kitty\nports:\n  range: [not, a, range]\n";
    let (config, issues) = parse_user_lenient("config.yaml", source);
    assert_eq!(config.proxy.http_port, 8081);
    assert!(matches!(
        config.terminal.as_ref().and_then(TerminalConfig::app),
        Some(Terminal::Kitty)
    ));
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

/// `terminal` is the app name alone or a mapping with the progress setting.
#[test]
fn terminal_takes_a_name_or_a_mapping() {
    let bare = parse_user("terminal: kitty\n").expect("name");
    let terminal = bare.terminal.expect("terminal");
    assert!(matches!(terminal.app(), Some(Terminal::Kitty)));
    assert_eq!(terminal.progress(), TerminalProgress::Auto);
    let mapped = parse_user("terminal:\n  app: ghostty\n  progress: never\n").expect("mapping");
    let terminal = mapped.terminal.expect("terminal");
    assert!(matches!(terminal.app(), Some(Terminal::Ghostty)));
    assert_eq!(terminal.progress(), TerminalProgress::Never);
    let progress_only = parse_user("terminal: { progress: always }\n").expect("progress only");
    let terminal = progress_only.terminal.expect("terminal");
    assert!(terminal.app().is_none());
    assert_eq!(terminal.progress(), TerminalProgress::Always);
    let source = "terminal:\n  app: ghostty\n  progres: never\n";
    let error = parse_user(source).expect_err("typo");
    assert!(
        error
            .message
            .contains("unknown key `progres`; did you mean `progress`?"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 3);
    assert!(parse_user("terminal: xterm\n").is_err());
}

/// `windows-terminal` is spelled with a dash like the launcher's name, and
/// only it and WezTerm have Windows versions.
#[test]
fn windows_terminal_parses_with_its_launcher_name() {
    let config = parse_user("terminal: windows-terminal\n").expect("windows-terminal");
    let app = config.terminal.and_then(|t| t.app()).expect("app");
    assert!(matches!(app, Terminal::WindowsTerminal));
    assert_eq!(app.as_str(), "windows-terminal");
    assert_eq!(
        serde_json::to_value(app).expect("serialize"),
        "windows-terminal"
    );
    assert!(parse_user("terminal: windowsterminal\n").is_err());
    let on_windows: Vec<&str> = [
        Terminal::Iterm2,
        Terminal::Ghostty,
        Terminal::Wezterm,
        Terminal::Kitty,
        Terminal::Terminal,
        Terminal::WindowsTerminal,
    ]
    .into_iter()
    .filter(|app| app.runs_on_windows())
    .map(Terminal::as_str)
    .collect();
    assert_eq!(on_windows, ["wezterm", "windows-terminal"]);
}

/// A terminal app without a Windows version warns on Windows (config check and
/// doctor show it) but never fails, since configs move between machines.
#[cfg(windows)]
#[test]
fn unix_only_terminals_warn_on_windows() {
    for (source, path) in [
        ("terminal: iterm2\n", "terminal"),
        ("terminal:\n  app: kitty\n", "terminal.app"),
    ] {
        let config = parse_user(source).expect(source);
        let paths: Vec<_> = config.warnings().iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, [path], "{source}");
        assert!(
            config.warnings()[0].message.contains("no Windows version"),
            "{}",
            config.warnings()[0].message
        );
        assert_eq!(config.platform_warnings().len(), 1);
    }
    for source in ["terminal: wezterm\n", "terminal: windows-terminal\n", "{}"] {
        assert!(parse_user(source).expect(source).warnings().is_empty());
    }
    let (lenient, _) = parse_user_lenient("config.yaml", "terminal: ghostty\n");
    assert_eq!(lenient.warnings().len(), 1);
}

/// Outside Windows every terminal app is accepted silently.
#[cfg(not(windows))]
#[test]
fn terminals_never_warn_outside_windows() {
    for source in ["terminal: iterm2\n", "terminal:\n  app: kitty\n"] {
        let config = parse_user(source).expect(source);
        assert!(config.warnings().is_empty(), "{source}");
        assert!(config.platform_warnings().is_empty(), "{source}");
    }
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
            "project: demo\nservices:\n  api:\n    run_script: { source: package.json, nmae: dev }\n",
            "nmae",
            "unknown key `nmae`; did you mean `name`?",
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
    let source = "project: demo\nservices:\n  web:\n    preset: vite\n    run: server\n    vite:\n      argz: []\n";
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
    let source = "project: demo\nglobal_env:\n  OTHER: x\n  A.B: '${{ svc.missing.url }}'\n";
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
        "project: demo\nservices:\n  api:\n    run: server\n    preset: custm\n",
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
    let source = "project: demo\nservices:\n  api:\n    run: server\n    preset: vitee\n";
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

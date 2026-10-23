mod dogfood;

mod artifacts;
mod diagnostics;
mod merging;
mod reference_docs;
mod references;
mod retention;
mod schema_docs;
mod service_keys;
mod tags;

use devlanes_config::*;
use serde_json::json;

const FULL: &str = include_str!("../../../../docs/examples/devlanes.yaml");
const MINIMAL: &str = include_str!("../../../../docs/examples/minimal.yaml");
const LOCAL: &str = include_str!("../../../../docs/examples/devlanes.local.yaml");
const USER: &str = include_str!("../../../../docs/examples/user-config.yaml");

/// Exercise the same runtime registration the composition root supplies.
fn registered_schema() -> serde_json::Value {
    project_schema_with_presets(&[
        (
            devlanes_preset_vite::NAME,
            devlanes_preset_vite::options_schema(),
        ),
        (
            devlanes_preset_next::NAME,
            devlanes_preset_next::options_schema(),
        ),
        (
            devlanes_preset_wrangler::NAME,
            devlanes_preset_wrangler::options_schema(),
        ),
        (
            devlanes_preset_tauri::NAME,
            devlanes_preset_tauri::options_schema(),
        ),
    ])
}

/// Delegate parsing to the actual presets without production dependencies from config.
fn registered_validator(name: &str, options: Option<&serde_json::Value>) -> Result<(), String> {
    match name {
        devlanes_preset_vite::NAME => devlanes_preset_vite::parse_options(options)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        devlanes_preset_next::NAME => devlanes_preset_next::parse_options(options)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        devlanes_preset_wrangler::NAME => devlanes_preset_wrangler::parse_options(options)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        devlanes_preset_tauri::NAME => devlanes_preset_tauri::parse_options(options)
            .map(|_| ())
            .map_err(|e| e.to_string()),
        _ => Err(format!("unregistered preset `{name}`")),
    }
}

/// Keep snapshots limited to sanitized public configuration values.
#[test]
fn reference_examples() {
    insta::assert_json_snapshot!("full", parse_project(FULL, None).expect("full reference"));
    insta::assert_json_snapshot!(
        "minimal",
        parse_project(MINIMAL, None).expect("minimal reference")
    );
    let base = "project: acme\ntemplates:\n  fastify-api:\n    platform: node\nservices:\n  user-web:\n    run: node web.mjs\n    tags: [essential]\n";
    insta::assert_json_snapshot!(
        "local",
        parse_project(base, Some(LOCAL)).expect("local with matching base")
    );
    parse_project(FULL, Some(LOCAL)).expect("the example pair merges");
    let user: UserConfig = serde_saphyr::from_str(USER).expect("typed user syntax");
    insta::assert_json_snapshot!("user", user);
    user.validate().expect("the user example is valid");
}

/// Local partial services cannot silently introduce a service with no executable.
#[test]
fn local_missing_service_is_rejected() {
    let local = "services:\n  admin-web:\n    autostart: false\n";
    let error = parse_project(FULL, Some(local)).expect_err("admin-web missing");
    assert!(error.message.contains("admin-web"));
    assert_eq!(error.location.file, "devlanes.local.yaml");
}

/// Later declarations must not sort ahead of earlier lifecycle tasks.
#[test]
fn task_order_is_preserved() {
    let config = parse_project(
        "project: demo\ntasks:\n  z: { run: z }\n  a: { run: a }\n",
        Some("tasks:\n  z: { timeout: 1m }\n  m: { run: m }\n"),
    )
    .expect("valid tasks");
    assert_eq!(
        config.tasks.keys().map(String::as_str).collect::<Vec<_>>(),
        ["z", "a", "m"]
    );
}

/// Local edits to templates must be visible to both base and local services.
#[test]
fn merge_before_extends() {
    let base = "project: demo\ntemplates:\n  base:\n    env: { A: old, KEEP: yes }\n    tags: [old]\nservices:\n  api: { extends: base, run: server }\n";
    let local = "templates:\n  base:\n    env: { A: new }\n    tags: []\nservices:\n  other: { extends: base, run: server }\n";
    let config = parse_project(base, Some(local)).expect("merged");
    for service in config.services.values() {
        assert_eq!(
            service.env["A"],
            Templated::Literal(Scalar::String("new".into()))
        );
        assert!(service.env.contains_key("KEEP"));
        assert_eq!(service.tags, Some(vec![]));
    }
}

/// Show source-labelled errors for all required broken configuration forms.
#[test]
fn broken_diagnostics() {
    let cases = [
        (
            "typo",
            "project: demo\nservices:\n  api:\n    run: server\n    autostrat: false\n",
        ),
        (
            "on",
            "project: demo\ntasks:\n  migrate:\n    run: migrate\n    on: lane.start\n",
        ),
        (
            "unknown_template",
            "project: demo\nservices:\n  api:\n    extends: missing\n    run: server\n",
        ),
        (
            "expression_cycle",
            "project: demo\nlanes:\n  vars:\n    a: '${{ lane.b }}'\n    b: '${{ lane.a }}'\n",
        ),
        (
            "dash",
            "project: demo\nservices:\n  crm-api: { run: server }\n  web:\n    run: server\n    env: { URL: '${{ services.crm-api.url }}' }\n",
        ),
        (
            "duration",
            "project: demo\nservices:\n  api:\n    run: server\n    stop: { timeout: 10fortnights }\n",
        ),
        (
            "wrong_type",
            "project: demo\nlanes:\n  block_size: banana\n",
        ),
    ];
    for (name, yaml) in cases {
        let error = parse_project(yaml, None).expect_err(name);
        let report = miette::GraphicalReportHandler::new()
            .with_width(90)
            .with_links(false)
            .with_theme(miette::GraphicalTheme::unicode_nocolor());
        let mut output = String::new();
        report
            .render_report(&mut output, &error)
            .expect("render diagnostic");
        insta::assert_snapshot!(name, output);
        assert!(error.location.line > 0);
    }
}

/// Scalar parsing rejects unknown units, overflow, and wrong YAML types.
#[test]
fn duration_size_and_templated_types() {
    assert_eq!(
        "10m"
            .parse::<ConfigDuration>()
            .expect("duration")
            .0
            .as_secs(),
        600
    );
    assert_eq!(
        "15s"
            .parse::<ConfigDuration>()
            .expect("duration")
            .0
            .as_secs(),
        15
    );
    assert_eq!("5GiB".parse::<ByteSize>().expect("size").0, 5 << 30);
    assert!("18446744073709551615d".parse::<ConfigDuration>().is_err());
    assert!("5meg".parse::<ByteSize>().is_err());
    let value: Templated<u16> = serde_saphyr::from_str("'${{ lane.port + 5 }}'").expect("template");
    let context = ExpressionContext {
        lane: json!({"port":20000}),
        ..Default::default()
    };
    assert_eq!(value.evaluate(&context).expect("number"), 20005);
    let bad: Templated<u16> = Templated::Expression("${{ 'not a number' }}".into());
    assert!(bad.evaluate(&context).is_err());
    let overflow: Templated<u16> = Templated::Expression("${{ 70000 }}".into());
    assert!(overflow.evaluate(&context).is_err());
    let wrong_constant = parse_project(
        "project: demo\nlanes:\n  block_size: \"${{ 'bad' }}\"\n",
        None,
    )
    .expect_err("constant wrong type");
    assert_eq!(wrong_constant.location.line, 3);
}

/// Native results retain types; interpolated results are strings and shell syntax survives.
#[test]
fn expression_evaluation() {
    let ctx = ExpressionContext {
        lane: json!({"index":2,"port":20000}),
        services: json!({"crm-api":{"port":20001}}),
        ..Default::default()
    };
    assert_eq!(
        evaluate("${{ lane.port + 5 }}", &ctx).expect("int"),
        json!(20005)
    );
    assert_eq!(
        evaluate("${{ lane.index == 2 }}", &ctx).expect("bool"),
        json!(true)
    );
    assert_eq!(
        evaluate("port=${{ lane.port }} ${SHELL_VAR}", &ctx).expect("mixed"),
        json!("port=20000 ${SHELL_VAR}")
    );
    assert_eq!(
        evaluate("${{ services.crm_api.port }}", &ctx).expect("alias"),
        json!(20001)
    );
    assert_eq!(
        evaluate("${{ services['crm-api'].port }}", &ctx).expect("bracket"),
        json!(20001)
    );
    assert_eq!(
        evaluate("${{ env.MISSING | default('fallback') }}", &ctx).expect("default"),
        json!("fallback")
    );
    assert!(evaluate("${{ env.MISSING }}", &ctx).is_err());
    assert!(evaluate("x=${{ env.MISSING }}", &ctx).is_err());
    assert_eq!(
        evaluate("${{ '}}' }}", &ctx).expect("quoted braces"),
        json!("}}")
    );
}

/// Cross-project fallback must tolerate a project that is not running.
#[test]
fn cross_project_default() {
    assert_eq!(
        evaluate(
            "${{ projects.absent.services.api.public_url | default('https://example.test') }}",
            &ExpressionContext::default()
        )
        .expect("missing project fallback"),
        json!("https://example.test")
    );
    assert_eq!(
        evaluate(
            "${{ projects['absent-project'].services.api.public_url | default('fallback') }}",
            &ExpressionContext::default()
        )
        .expect("missing dashed project"),
        json!("fallback")
    );
    let context = ExpressionContext {
        projects: json!({"other-project":{"services":{"api":{"url":"http://localhost:20000"}}}}),
        ..Default::default()
    };
    assert_eq!(
        evaluate("${{ projects.other_project.services.api.url }}", &context)
            .expect("project alias"),
        json!("http://localhost:20000")
    );
}

/// Invalid lifecycle, infra, and command references must fail before runtime.
#[test]
fn structural_validation() {
    for (yaml, expected) in [
        ("project: invalid@name", "project: expected"),
        (
            "project: demo\nservices: { api: { run: a, run_script: { file: package.json, name: b } } }",
            "services.api: exactly one of run, run_script",
        ),
        (
            "project: demo\ntasks: { a: { run: a, interactive: true, when: lane.start } }",
            "tasks.a.interactive: interactive tasks cannot have when",
        ),
        (
            "project: demo\ntasks: { a: { run: a, after: services } }",
            "tasks.a.after: after: services requires when",
        ),
        (
            "project: demo\ntasks: { a: { steps: [missing] } }",
            "tasks.a.steps: unknown step `missing`",
        ),
        (
            "project: demo\ntasks: { a: { steps: [b] }, b: { steps: [a] } }",
            "tasks: expression/reference cycle: a -> b -> a",
        ),
        (
            "project: demo\nprofiles: { bad: [missing] }",
            "profiles.bad: selector `missing` matches no services",
        ),
        (
            "project: demo\ninfra: { hidden: [missing] }",
            "infra: unknown container or Compose service `missing`",
        ),
        (
            "project: demo\nservices: { api: { run: a, ready: { tcp: true, log: ready } } }",
            "services.api.ready: ready requires exactly one of tcp, http, log, or cmd",
        ),
        (
            "project: demo\nservices: { api: { run: a, gui: {} } }",
            "`gui` is not supported yet",
        ),
        (
            "project: demo\nservices: { api: { run: a, wrangler: {} } }",
            "services.api.wrangler: unknown key `wrangler`; set `preset: wrangler`",
        ),
        (
            "project: demo\nservices: { api: { run: a, env: { PORT: '${{ self.port }}' } } }",
            "services.api.env.PORT: expression cycle: env.PORT determines self.port",
        ),
        (
            "project: demo\ntemplates: { a: { extends: b }, b: { extends: a } }",
            "templates.a.extends: template cycle: a -> b -> a",
        ),
        (
            "project: demo\nservices: { api: { tags: [x] } }",
            "services.api: service requires run or run_script",
        ),
    ] {
        let error = parse_project(yaml, None).expect_err(yaml);
        assert!(
            error.message.contains(expected),
            "{yaml}\nexpected `{expected}` in: {}",
            error.message
        );
    }
}

/// Standalone schema generation must include exact preset option keys.
#[test]
fn schemas_are_strict_and_include_extensions() {
    let schema = registered_schema();
    assert_eq!(schema["additionalProperties"], false);
    let defs = schema["$defs"].as_object().expect("definitions");
    let service = defs
        .iter()
        .find(|(name, _)| name.starts_with("ServiceConfig"))
        .expect("service schema")
        .1;
    assert!(service["properties"].get("wrangler").is_some());
    assert!(service["properties"].get("tauri").is_some());
    assert_eq!(service["additionalProperties"], false);
    assert!(user_schema()["properties"].get("ports").is_some());
}

/// Both modelines support all binding YAML forms, including partial local overlays.
#[test]
fn yaml_language_server_schemas() {
    let project = project_schema();
    let registered = registered_schema();
    let user = user_schema();
    assert_eq!(project["$id"], "https://devlanes.run/v1/schema.json");
    assert_eq!(user["$id"], "https://devlanes.run/v1/user-schema.json");
    let project_validator =
        jsonschema::validator_for(&registered).expect("valid registered project schema");
    let opaque_validator =
        jsonschema::validator_for(&project).expect("valid opaque project schema");
    let user_validator = jsonschema::validator_for(&user).expect("valid user schema");
    for (name, source) in [
        ("full", FULL),
        ("minimal", MINIMAL),
        ("local", LOCAL),
        ("user", USER),
    ] {
        let value: serde_json::Value = serde_saphyr::from_str(source).expect("YAML document");
        if name != "user" {
            assert!(opaque_validator.is_valid(&value));
        }
        let validator = if name == "user" {
            &user_validator
        } else {
            &project_validator
        };
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{name}: {errors:#?}");
    }
    let config = parse_project(FULL, None).expect("reference example");
    let Some(ProfileConfig::Object(profile)) = config.profiles.get("e2e") else {
        panic!("the reference example declares an e2e profile mapping")
    };
    assert_eq!(profile.remap_script("dev"), "start");
    assert_eq!(config.tasks["build"].when, Some(TaskWhen::Start));
}

/// The deferred public resolver fails validation; mobile trust targets only warn.
#[test]
fn user_validation() {
    assert!(
        parse_user("hostnames: { resolver: public, base_domain: dev.example.test }")
            .expect_err("deferred public")
            .message
            .contains("not supported yet")
    );
    assert_eq!(
        parse_user("tls: { trust: [ios-simulator] }")
            .expect("deferred trust is ignored")
            .warnings()
            .len(),
        1
    );
    assert!(parse_user("ports: { range: 32767-20000 }").is_err());
    assert_eq!(
        parse_user("{}")
            .expect("defaults")
            .ports
            .bounds()
            .expect("bounds"),
        (20000, 32767)
    );
}

/// Lane data folders default to `data` in the devlanes home; a set folder must be
/// absolute or start with `~/`.
#[test]
fn lane_data_root() {
    let home = std::path::Path::new("/h/.devlanes");
    let defaults = parse_user("{}").expect("defaults").lanes;
    assert_eq!(defaults.data_root(home), home.join("data"));
    let set = parse_user("lanes: { data_dir: /fast/devlanes }").expect("absolute");
    assert_eq!(
        set.lanes.data_root(home),
        std::path::Path::new("/fast/devlanes")
    );
    let tilde = parse_user("lanes: { data_dir: ~/lane-data }").expect("tilde");
    if let Some(user_home) = std::env::var_os("HOME") {
        assert_eq!(
            tilde.lanes.data_root(home),
            std::path::PathBuf::from(user_home).join("lane-data")
        );
    }
    let relative = parse_user("lanes: { data_dir: data }").expect_err("relative");
    assert!(
        relative.message.contains("lanes.data_dir"),
        "{}",
        relative.message
    );
}

/// `hostnames.tld` must be a special-use name whatever the strategy, so a TLD that
/// could belong to a real site fails at load, located at the key; a lenient load
/// drops only that key.
#[test]
fn user_tld_validation() {
    for tld in [
        "test",
        "Test.",
        "internal",
        "dev.internal",
        "home.arpa",
        "lab.home.arpa",
        "dev.localhost",
    ] {
        for resolver in ["resolver", "localhost"] {
            parse_user(&format!(
                "hostnames: {{ resolver: {resolver}, tld: '{tld}' }}"
            ))
            .unwrap_or_else(|error| panic!("{tld} with {resolver}: {}", error.message));
        }
    }
    for tld in [
        "dev",
        "local",
        "localhost",
        "com",
        "test.com",
        "contest",
        "a..test",
        "-x.test",
        "x_y.test",
        "123",
    ] {
        for resolver in ["resolver", "localhost"] {
            let source = format!("hostnames:\n  resolver: {resolver}\n  tld: '{tld}'\n");
            let error = parse_user(&source).expect_err(&source);
            assert!(
                error.message.starts_with("hostnames.tld: `"),
                "{source}: {}",
                error.message
            );
            assert_eq!(error.location.line, 3, "{source}: {}", error.message);
        }
    }
    assert!(
        parse_user("hostnames: { tld: dev }")
            .expect_err("public TLD")
            .message
            .contains("must be test, internal or home.arpa")
    );
    assert!(
        parse_user("hostnames: { resolver: resolver, tld: '' }")
            .expect_err("empty")
            .message
            .contains("required for the resolver strategy")
    );
    let (config, issues) = parse_user_lenient(
        "config.yaml",
        "hostnames:\n  tld: dev\n  hosts_file: never\n",
    );
    assert_eq!(config.hostnames.tld, None);
    assert!(matches!(
        config.hostnames.hosts_file,
        Some(HostsFile::Never)
    ));
    assert_eq!(issues.len(), 1, "{issues:?}");
}

/// Startup pacing parses typed percentages and derives windows from the core count.
#[test]
fn startup_settings() {
    let defaults = parse_user("{}").expect("defaults").startup;
    assert_eq!(defaults.mode, DEFAULT_STARTUP_MODE);
    assert_eq!(defaults.target_cpu, Percent(75.0));
    assert_eq!(defaults.settle_cpu, Percent(10.0));
    assert_eq!(defaults.slot_timeout.0.as_secs(), 20);
    assert_eq!(defaults.mode, StartupMode::Adaptive);
    assert_eq!(defaults.reserve_memory, None);
    assert_eq!(defaults.windows(16), (16, 16));
    assert_eq!(defaults.windows(4), (4, 4));
    assert_eq!(defaults.windows(1), (1, 1));

    let user = parse_user(USER).expect("user example");
    assert_eq!(user.startup.mode, StartupMode::Adaptive);
    assert_eq!(user.startup.windows(32), (2, 8));
    assert_eq!(user.startup.settle_cpu, Percent(10.0));
    assert_eq!(user.startup.reserve_memory, Some(ByteSize(2 << 30)));
    assert_eq!(
        parse_user("startup: { reserve_memory: 0B }")
            .expect("reserve off")
            .startup
            .reserve_memory,
        Some(ByteSize(0))
    );
    assert!(parse_user("startup: { reserve_memory: 10% }").is_err());

    let custom = parse_user("startup: { mode: all, target_cpu: 12.5%, max: 3, initial: 9 }");
    assert!(
        custom
            .expect_err("initial above max")
            .message
            .contains("startup.initial")
    );
    assert_eq!(
        parse_user("startup: { max: 3 }")
            .expect("only max")
            .startup
            .windows(64),
        (3, 3)
    );
    for (source, line) in [
        ("startup:\n  mode: fixed\n  target_cpu: 75\n", 3),
        ("startup:\n  settle_cpu: -5%\n", 2),
        ("startup:\n  max: 0\n", 2),
        ("startup:\n  mode: turbo\n", 2),
    ] {
        let error = parse_user(source).expect_err(source);
        assert_eq!(error.location.line, line, "{source}: {}", error.message);
    }
    assert!(parse_user("startup: { target_cpu: 150% }").is_err());
    assert!(parse_user("startup: { slot_timeout: 0s }").is_err());
    assert!("10".parse::<Percent>().is_err());
    assert_eq!("7.5 %".parse::<Percent>().expect("spaced"), Percent(7.5));
}

/// Diagnostics after merging should point into the local file, not a rewritten document.
#[test]
fn local_error_location() {
    let error = parse_project(
        "project: demo\nservices:\n  api: { run: server }\n",
        Some("services:\n  api:\n    run: other\n    run_script: { file: package.json, name: dev }\n"),
    )
    .expect_err("conflicting command");
    assert_eq!(error.location.file, "devlanes.local.yaml");
    assert_eq!(error.location.line, 3);
}

/// Nested mappings keep omitted keys while profile lists can be explicitly cleared.
#[test]
fn nested_map_and_empty_list_overrides() {
    let base = "project: demo\nservices:\n  api:\n    run: server\n    ready: { http: { path: /health, timeout: 1s } }\n    stop: { signal: SIGINT, timeout: 10s }\nprofiles:\n  default:\n    include: [api]\n    exclude: [api]\n";
    let local = "services:\n  api:\n    ready: { http: { timeout: 5s } }\n    stop: { timeout: 20s }\nprofiles:\n  default:\n    include: []\n    exclude: []\n";
    let config = parse_project(base, Some(local)).expect("nested override");
    let service = &config.services["api"];
    assert_eq!(
        service.stop.as_ref().expect("stop").signal.as_deref(),
        Some("SIGINT")
    );
    let ReadyConfig::Check(ready) = service.ready.as_ref().expect("ready") else {
        panic!("check")
    };
    assert_eq!(
        ready.http.as_ref().expect("http").path,
        Some(Templated::Literal("/health".into()))
    );
    let ProfileConfig::Object(profile) = &config.profiles["default"] else {
        panic!("profile")
    };
    assert_eq!(profile.include, Some(vec![]));
    assert_eq!(profile.exclude, Some(vec![]));
}

/// The caller can resolve lane/env graphs and then evaluate typed service fields.
#[test]
fn typed_runtime_evaluation() {
    let config = parse_project("project: demo\nlanes:\n  vars:\n    url: 'http://${{ lane.host }}'\n    host: localhost\nservices:\n  api:\n    run: 'server --port ${{ self.port }}'\n    autostart: '${{ lane.index == 1 }}'\n    env: { URL: '${{ lane.url }}' }\n",None).expect("config");
    let mut context = ExpressionContext {
        lane: json!({"index":1}),
        service: json!({"port":20001}),
        ..Default::default()
    };
    let vars = resolve_lane_vars(&config.lanes, &context).expect("lane vars");
    assert_eq!(vars["url"].to_string(), "http://localhost");
    for (name, value) in vars {
        context.lane[name] = serde_json::to_value(value).expect("scalar");
    }
    let service = evaluate_typed(&config.services["api"], &context).expect("typed service");
    assert_eq!(service.autostart, Some(Templated::Literal(true)));
    assert_eq!(
        service.env["URL"],
        Templated::Literal(Scalar::String("http://localhost".into()))
    );
    let env: Env = serde_saphyr::from_str("B: '${{ env.A }}-suffix'\nA: literal").expect("env");
    context.env = json!({"A":"outer"});
    assert_eq!(
        evaluate_env(&env, &context).expect("outer env")["B"].to_string(),
        "outer-suffix"
    );
}

/// `hosts` takes one hostname or route as well as a list, and validates either form.
#[test]
fn single_host_shorthand() {
    let config = parse_project(
        "project: demo\nservices:\n  api: { run: server, hosts: 'api.${{ lane.domain }}' }\n  web: { run: server, hosts: { host: web.localhost, fallback: true } }\n  docs: { run: server, hosts: [docs.localhost, { host: '*.docs.localhost' }] }\n",
        None,
    )
    .expect("config");
    let hosts = |name: &str| config.services[name].hosts.clone().expect("hosts");
    assert!(
        matches!(&hosts("api")[..], [HostConfig::Name(Templated::Expression(text))] if text == "api.${{ lane.domain }}")
    );
    assert!(matches!(&hosts("web")[..], [HostConfig::Route(route)] if route.fallback.is_some()));
    assert_eq!(hosts("docs").len(), 2);
    let context = ExpressionContext {
        lane: json!({"domain": "demo.localhost"}),
        ..Default::default()
    };
    let api = evaluate_typed(&config.services["api"], &context).expect("typed service");
    assert!(
        matches!(&api.hosts.expect("hosts")[..], [HostConfig::Name(Templated::Literal(host))] if host == "api.demo.localhost")
    );
    assert!(
        parse_project(
            "project: demo\nservices:\n  api: { run: server, hosts: bad_host }\n",
            None
        )
        .is_err()
    );
    assert!(
        parse_project(
            "project: demo\nservices:\n  api: { run: server, hosts: 3 }\n",
            None
        )
        .is_err()
    );
    assert!(parse_project("project: demo\nservices:\n  api: { run: server, hosts: { host: a.localhost, prot: x } }\n", None).is_err());
    let infra = parse_project(
        "project: demo\ninfra:\n  containers:\n    s3: { image: s3mock, ports: { http: 9090 } }\n  hosts: { s3: s3.localhost }\n",
        None,
    )
    .expect("infra config");
    assert_eq!(infra.infra.hosts["s3"].len(), 1);
}

/// Outer env references do not create sibling cycles; fixed port capacity remains checked.
#[test]
fn outer_port_reference_and_capacity() {
    assert!(parse_project("project: demo\nservices:\n  api:\n    run: server\n    env:\n      PORT: '${{ env.OTHER }}'\n      OTHER: '${{ self.port }}'\n",None).is_ok());
    assert!(parse_project("project: demo\nlanes: { block_size: 1 }\nservices:\n  api: { run: server, ready: { tcp: inspector } }\n",None).is_err());
    assert!(parse_project("project: demo\nlanes: { block_size: 5 }\nservices:\n  api: { run: server, env: { PORT: '${{ lane.port + 5 }}' } }\n",None).is_err());
}

/// Self-references and apparent sibling cycles read only the unchanged outer environment.
#[test]
fn env_overrides_read_outer_values() {
    let config = parse_project(
        "project: demo\nglobal_env:\n  SENTRY_DSN: ${{ env.SENTRY_DSN | default('') }}\nservices:\n  api:\n    run: server\n    env:\n      PORT: '${{ env.PORT }}'\n      A: '${{ env.B }}'\n      B: '${{ env.A }}'\n",
        None,
    ).expect("outer references are valid");
    let context = ExpressionContext {
        env: json!({"SENTRY_DSN":"outer-dsn","PORT":20000,"A":"outer-a","B":"outer-b"}),
        ..Default::default()
    };
    let resolved = evaluate_env(&config.global_env, &context).expect("existing DSN");
    assert_eq!(resolved["SENTRY_DSN"].to_string(), "outer-dsn");
    let missing =
        evaluate_env(&config.global_env, &ExpressionContext::default()).expect("default DSN");
    assert_eq!(missing["SENTRY_DSN"].to_string(), "");
    let service_env =
        evaluate_env(&config.services["api"].env, &context).expect("outer service env");
    assert_eq!(service_env["PORT"], Scalar::Integer(20000));
    assert_eq!(service_env["A"].to_string(), "outer-b");
    assert_eq!(service_env["B"].to_string(), "outer-a");
    assert_eq!(context.env["A"], "outer-a");
    assert_eq!(
        service_env.keys().map(String::as_str).collect::<Vec<_>>(),
        ["PORT", "A", "B"]
    );
}

/// Lane variables still resolve one another and reject cycles during loading and evaluation.
#[test]
fn lane_var_cycles_remain_errors() {
    let source = "project: demo\nlanes:\n  vars:\n    a: '${{ lane.b }}'\n    b: '${{ lane.a }}'\n";
    assert!(
        parse_project(source, None)
            .expect_err("lane cycle")
            .message
            .contains("cycle")
    );
    let lanes: LanesConfig =
        serde_saphyr::from_str("vars:\n  a: '${{ lane.b }}'\n  b: '${{ lane.a }}'\n")
            .expect("lane syntax");
    assert!(
        resolve_lane_vars(&lanes, &ExpressionContext::default())
            .expect_err("runtime lane cycle")
            .message
            .contains("cycle")
    );
}

/// Extension schemas must reject unknown nested keys and retain typed booleans/numbers.
#[test]
fn schema_parser_type_agreement() {
    let schema = registered_schema();
    let validator = jsonschema::validator_for(&schema).expect("schema");
    for value in [
        json!({"services":{"api":{"run":"server","autostart":true,"replicas":2}}}),
        json!({"services":{"api":{"run":"server","autostart":"${{ lane.index == 0 }}"}}}),
        json!({"services":{"api":{"preset":"wrangler","wrangler":{"vars":{"COUNT":3},"args":["--local"]}}}}),
    ] {
        assert!(validator.is_valid(&value), "schema rejected {value}");
    }
    for value in [
        json!({"unknown":true}),
        json!({"services":{"api":{"unknown":true}}}),
        json!({"services":{"api":{"wrangler":{"unknown":true}}}}),
        json!({"services":{"api":{"tauri":{"identifier":42}}}}),
        json!({"services":{"api":{"replicas":false}}}),
        json!({"services":{"api":{"replicas":"${{ lane.index + 1 }}"}}}),
    ] {
        assert!(!validator.is_valid(&value), "schema accepted {value}");
    }
}

/// Compose adapters supply resolved names without config spawning Docker subprocesses.
#[test]
fn compose_reference_validation() {
    let config = parse_project("project: demo\ninfra:\n  compose: compose.yaml\n  hidden: [postgres]\n  hosts: { postgres: [db.localhost] }\n",None).expect("requires resolved model");
    assert!(
        validate(&config).expect("report").warnings.is_empty(),
        "unresolved Compose names are not a warning"
    );
    assert!(validate_with_compose_services(&config, &Default::default()).is_err());
    let names = ["postgres".to_owned()].into_iter().collect();
    assert!(
        validate_with_compose_services(&config, &names)
            .expect("resolved model")
            .warnings
            .is_empty()
    );
}

/// File loading honors the sibling local file and produces reusable check output.
#[test]
fn load_and_check_files() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        dir.path().join("devlanes.yaml"),
        "project: demo\nservices:\n  api: { run: server }\n",
    )
    .expect("base file");
    std::fs::write(
        dir.path().join("devlanes.local.yaml"),
        "services:\n  api: { autostart: false }\n",
    )
    .expect("local file");
    let project = load_project(dir.path()).expect("loaded config");
    assert_eq!(
        project.services["api"].autostart,
        Some(Templated::Literal(false))
    );
    assert_eq!(check(dir.path()).expect("check").project, "demo");
}

#[derive(Debug, Clone, Default, schemars::JsonSchema)]
struct TestExtensions;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
struct TestOptions {
    flag: Templated<bool>,
}

impl Extensions for TestExtensions {
    type Options = TestOptions;
    /// Register a test-only integration without changing the config parser.
    fn preset_names() -> &'static [&'static str] {
        &["custom"]
    }
    /// Delegate strict option parsing to the integration's typed options.
    fn parse_preset_options(_: &str, value: serde_json::Value) -> Result<Self::Options, String> {
        serde_json::from_value(value).map_err(|e| e.to_string())
    }
    /// Contribute an inline test schema at the declared integration key.
    fn extend_schema(schema: &mut schemars::Schema) {
        if let Some(defs) = schema
            .as_object_mut()
            .and_then(|s| s.get_mut("$defs"))
            .and_then(|s| s.as_object_mut())
        {
            for (name, value) in defs {
                if name.starts_with("ServiceConfig") {
                    value["properties"]["custom"] = json!({"type":"object","additionalProperties":false,"properties":{"flag":{"anyOf":[{"type":"boolean"},{"type":"string"}]}},"required":["flag"]});
                }
            }
        }
    }
}

/// Extension registration controls parsing and schema without core/integration dependencies.
#[test]
fn custom_extension_seam() {
    let source = "project: demo\nservices:\n  api:\n    run_script: { file: package.json, name: dev }\n    custom: { flag: true }\n    preset: custom\n";
    let config = parse_project_with::<TestExtensions>(source, None).expect("custom preset");
    assert_eq!(
        config.services["api"].preset_options["custom"].flag,
        Templated::Literal(true)
    );
    let schema = schema_with::<TestExtensions>().to_value();
    let value: serde_json::Value = serde_saphyr::from_str(source).expect("YAML");
    assert!(
        jsonschema::validator_for(&schema)
            .expect("custom schema")
            .is_valid(&value)
    );
    assert!(parse_project(source, None).is_ok());
}

/// Profile remapping applies once to service script names and keeps the file.
#[test]
fn profile_script_remapping() {
    let source = "project: demo\ntasks:\n  build: { run: pnpm build, when: lane.start, profiles: [e2e] }\nservices:\n  api: { run_script: { file: apps/api/package.json, name: dev } }\nprofiles:\n  e2e:\n    include: [api]\n    run_scripts: { dev: start, start: production }\n    env: { NODE_ENV: production }\n";
    let local = "profiles:\n  e2e:\n    run_scripts: { watch: serve }\n";
    let config = parse_project(source, Some(local)).expect("e2e profile");
    let ProfileConfig::Object(profile) = &config.profiles["e2e"] else {
        panic!("object profile")
    };
    assert_eq!(profile.remap_script("dev"), "start");
    assert_eq!(profile.remap_script("watch"), "serve");
    assert_eq!(profile.remap_script("build"), "build");
    let script = config.services["api"].run_script.as_ref().expect("script");
    assert_eq!(
        script
            .file_and_name_for_profile(Some(profile))
            .expect("remapped"),
        ("apps/api/package.json", "start")
    );
    assert_eq!(
        script.file_and_name_for_profile(None).expect("unmapped"),
        ("apps/api/package.json", "dev")
    );
    let make = RunScript {
        file: "Makefile".into(),
        name: "dev".into(),
    };
    assert_eq!(
        make.file_and_name_for_profile(Some(profile))
            .expect("make remapped"),
        ("Makefile", "start")
    );
    assert_eq!(config.tasks["build"].when, Some(TaskWhen::Start));
    let validator = jsonschema::validator_for(&project_schema()).expect("schema");
    for remap in [
        json!({"":"start"}),
        json!({"dev":""}),
        json!({"dev":"   "}),
        json!({" ":"start"}),
    ] {
        let value = json!({"project":"demo","profiles":{"e2e":{"run_scripts":remap}}});
        assert!(!validator.is_valid(&value));
        assert!(
            parse_project(&serde_json::to_string(&value).expect("JSON is YAML"), None).is_err()
        );
    }
}

/// Integration-owned fields survive loading, merging, and evaluation as JSON objects.
#[test]
fn opaque_preset_options_and_registry_validation() {
    let source = "project: demo\nservices:\n  worker:\n    preset: wrangler\n    run_script: { file: package.json, name: dev }\n    wrangler:\n      isolate_registry: false\n      vars: { COUNT: '${{ lane.index }}' }\n  desktop:\n    preset: tauri\n    run_script: { file: package.json, name: dev }\n    tauri: { product_name: Demo, before_dev_command: '' }\n  web:\n    preset: vite\n    run_script: { file: package.json, name: dev }\n    vite: { args: [--open] }\n  next:\n    preset: next\n    run_script: { file: package.json, name: dev }\n    next: { args: [--turbo] }\n";
    let config = parse_project(source, None).expect("opaque options");
    assert_eq!(
        config.services["worker"].preset_options["wrangler"]["isolate_registry"],
        false
    );
    validate_with_presets(&config, &registered_validator).expect("integration-owned options");
    let value: serde_json::Value = serde_saphyr::from_str(source).expect("YAML");
    assert!(
        jsonschema::validator_for(&registered_schema())
            .expect("schema")
            .is_valid(&value)
    );
    let context = ExpressionContext {
        lane: json!({"index":3}),
        ..Default::default()
    };
    let options = evaluate_typed(
        &config.services["worker"].preset_options["wrangler"],
        &context,
    )
    .expect("evaluated JSON");
    assert_eq!(options["vars"]["COUNT"], 3);

    let custom = parse_project("project: demo\ntemplates:\n  base:\n    preset: custom\n    custom: { nested: { keep: true, replace: old }, args: [old] }\nservices:\n  api:\n    extends: base\n    run_script: { file: package.json, name: dev }\n    custom: { nested: { replace: new }, args: [] }\n",None).expect("unregistered preset before integration validation");
    assert_eq!(
        custom.services["api"].preset_options["custom"],
        json!({"nested":{"keep":true,"replace":"new"},"args":[]})
    );
    assert!(
        validate_with_presets(&custom, &registered_validator)
            .expect_err("unknown registry entry")
            .message
            .starts_with("services.api.preset: unregistered preset `custom`")
    );
    let invalid = parse_project(
        "project: demo\nservices:\n  api: { preset: vite, run_script: { file: package.json, name: dev }, vite: { unknown: true } }\n",
        None,
    )
    .expect("options deferred");
    let error = validate_with_presets(&invalid, &registered_validator)
        .expect_err("integration rejects unknown option");
    assert!(error.message.contains("services.api.vite"));
    assert!(error.message.contains("unknown"));

    let absent = parse_project(
        "project: demo\nservices:\n  api: { preset: custom, run_script: { file: package.json, name: dev } }\n",
        None,
    )
    .expect("no options");
    validate_with_presets(&absent, &|name, options| {
        assert_eq!(name, "custom");
        assert!(options.is_none());
        Ok(())
    })
    .expect("hook gets missing block");
}

/// Mapping shape and preset/key agreement fail at the original YAML location.
#[test]
fn preset_block_diagnostics() {
    for value in ["42", "[]", "null", "text"] {
        let source = format!(
            "project: demo\nservices:\n  api:\n    preset: custom\n    run_script: {{ file: package.json, name: dev }}\n    custom: {value}\n"
        );
        let error = parse_project(&source, None).expect_err("options must be mapping");
        assert!(error.message.contains("map"));
        assert_eq!(error.location.line, 6);
    }
    let source = "project: demo\nservices:\n  api:\n    preset: vite\n    run_script: { file: package.json, name: dev }\n    next: { args: [] }\n";
    let error = parse_project(source, None).expect_err("mismatched preset key");
    assert!(error.message.contains("services.api.next"));
    assert_eq!(error.location.line, 6);
    let source = "project: demo\nservices:\n  api:\n    custom: { nested: true }\n    run_script: { file: package.json, name: dev }\n    preset: custom\n";
    assert!(
        parse_project(source, None).is_ok(),
        "key order must not matter"
    );
}

/// Local definitions in injected schemas cannot collide or change the project schema ID.
#[test]
fn injected_schema_reference_isolation() {
    let first = schemars::json_schema!({"$id":"https://example.test/first", "type":"object", "additionalProperties":false, "properties":{"option":{"$ref":"#/$defs/Shared"}}, "$defs":{"Shared":{"type":"boolean"}}});
    let second = schemars::json_schema!({"type":"object", "additionalProperties":false, "properties":{"option":{"$ref":"#/$defs/Shared"}}, "$defs":{"Shared":{"type":"integer"}}});
    let schema = project_schema_with_presets(&[("first", first), ("second", second)]);
    assert_eq!(schema["$id"], "https://devlanes.run/v1/schema.json");
    let validator = jsonschema::validator_for(&schema).expect("isolated local refs");
    assert!(validator.is_valid(&json!({"services":{"a":{"preset":"first","first":{"option":true}},"b":{"preset":"second","second":{"option":2}}}})));
    for value in [
        json!({"services":{"a":{"preset":"unregistered"}}}),
        json!({"services":{"a":{"preset":"first","first":{"option":2}}}}),
        json!({"services":{"b":{"preset":"second","second":{"option":true}}}}),
        json!({"services":{"a":{"typo":{}}}}),
    ] {
        assert!(
            !validator.is_valid(&value),
            "invalid registered schema input: {value}"
        );
    }
    let opaque = project_schema_with_presets(&[]);
    assert_eq!(opaque, project_schema());
    let validator = jsonschema::validator_for(&opaque).expect("opaque schema");
    assert!(
        validator
            .is_valid(&json!({"services":{"a":{"preset":"custom","custom":{"arbitrary":true}}}}))
    );
    assert!(!validator.is_valid(&json!({"services":{"a":{"preset":"custom","custom":42}}})));
}

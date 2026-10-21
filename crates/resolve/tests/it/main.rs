//! Lane resolution tests: env layering, ports, selection, namespaces and
//! snapshots of the example configs.

mod script_notes;
mod support;

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use devlanes_config::{HostConfig, Scalar, Templated};
use devlanes_core::{
    contrib::{Contributor, DetectCx, ScriptRunner, SecretsProvider, UnitKind},
    env::EnvLayer,
    ports::{PortBlock, PortName},
};
use devlanes_resolve::{
    ReadySpec, ResolveError, ResolveInput, detect_platform, lane_layout, port_needs,
    profile_summaries, resolve_lane, select, select_profiles,
};
use serde_json::json;
use support::*;

/// Resolves `config` for `facts` with the fake script runner only.
async fn plain(yaml: &str, index: u32) -> devlanes_resolve::ResolvedLane {
    let config = parse(yaml);
    let facts = facts(&config.project, index, Some("main"));
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let infra = infra();
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(FakeRunner::new())];
    let client = client_env();
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.infra = &infra;
    input.script_runners = &runners;
    input.client_env = &client;
    let lane = resolve_lane(input).await.expect("resolves");
    assert!(lane.errors.is_empty(), "{:?}", lane.errors);
    lane
}

#[tokio::test]
/// Each variable's top layer wins, following decision 7 from client env to `--env`.
async fn env_precedence_table() {
    let config = parse(
        r#"
project: demo
global_env: { E: global, F: global, G: global, H: global }
services:
  api:
    run: server
    env: { F: service, G: service, H: service }
profiles:
  default:
    include: [api]
    env: { G: profile, H: profile }
"#,
    );
    let facts = facts("demo", 1, Some("x"));
    let contributor: Arc<dyn Contributor> = Arc::new(FakeContributor::env(
        "fake",
        0,
        vec![
            ("B", "contributor"),
            ("C", "contributor"),
            ("DEVLANES_LANE", "contributor"),
            ("E", "contributor"),
            ("H", "contributor"),
        ],
    ));
    let contributors = [contributor];
    let (block, offsets) = allocation(&config, 21_000, &contributors);
    let mut client = client_env();
    for key in ["A", "B", "C", "DEVLANES_LANE", "E", "F", "G", "H"] {
        client.insert(key.into(), "client".into());
    }
    let secrets = FakeSecrets {
        values: BTreeMap::from([(
            None,
            BTreeMap::from([("C".into(), "secret".into()), ("H".into(), "secret".into())]),
        )]),
        ..FakeSecrets::default()
    };
    let overrides = BTreeMap::from([("H".to_owned(), "override".to_owned())]);
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.client_env = &client;
    input.contributors = &contributors;
    input.secrets = Some(&secrets as &dyn SecretsProvider);
    input.overrides = &overrides;
    let lane = resolve_lane(input).await.expect("resolves");
    let env = &lane.services["api"].launch.env;
    let expected = [
        ("A", "client", EnvLayer::Client),
        ("B", "contributor", EnvLayer::Contributor),
        ("C", "secret", EnvLayer::Secrets),
        ("DEVLANES_LANE", "1", EnvLayer::Devlanes),
        ("E", "global", EnvLayer::Global),
        ("F", "service", EnvLayer::Service),
        ("G", "profile", EnvLayer::Profile),
        ("H", "override", EnvLayer::Override),
    ];
    for (key, value, layer) in expected {
        assert_eq!(env.values[key], value, "{key}");
        assert_eq!(env.sources[key], layer, "{key}");
    }
}

#[tokio::test]
/// Injected variables follow decision 7; tasks get no PORT or HOST.
async fn injected_variables() {
    let lane = plain(FULL, 2).await;
    let env = &lane.services["crm-api"].launch.env.values;
    assert_eq!(env["DEVLANES"], "1");
    assert_eq!(env["DEVLANES_PROJECT"], "acme");
    assert_eq!(env["DEVLANES_LANE"], "2");
    assert_eq!(env["DEVLANES_LANE_NAME"], "lane2");
    assert_eq!(env["DEVLANES_SERVICE"], "crm-api");
    assert_eq!(env["HOST"], "127.0.0.1");
    assert_eq!(env["PORT"], lane.services["crm-api"].port.to_string());
    assert_eq!(env["ACME_CRM_PORT"], env["PORT"]);
    assert_eq!(env["DEVLANES_PUBLIC_URL"], "https://crm.acme-2.localhost");
    let origins = &env["DEVLANES_ORIGINS"];
    assert!(origins.contains("https://crm.acme-2.localhost"));
    assert!(origins.contains(&format!("http://localhost:{}", env["PORT"])));
    assert!(!origins.contains('*'));
    let task = lane.tasks["build-deps"].launch.as_ref().expect("launch");
    assert!(!task.env.values.contains_key("PORT"));
    assert!(!task.env.values.contains_key("HOST"));
    assert_eq!(task.env.values["DEVLANES_LANE_NAME"], "lane2");
}

#[tokio::test]
/// A literal PORT is the service's port in every lane and allocates no offset;
/// an expression PORT reserves its block offset.
async fn port_overrides() {
    let config = parse(FULL);
    let needs = port_needs(&config, &[]).expect("needs");
    assert_eq!(needs.literal_ports.get("sms-stub"), Some(&3998));
    assert!(needs.derived_ports.contains("payments-api"));
    assert!(!needs.names.contains(&PortName::new("sms-stub", "default")));
    assert!(
        !needs
            .names
            .contains(&PortName::new("payments-api", "default"))
    );
    assert!(needs.reserved.contains(&5));
    assert!(needs.names.contains(&PortName::new("realtime", "debug")));
    assert!(
        needs
            .names
            .contains(&PortName::new("api-gateway", "inspector"))
    );
    assert!(needs.names.contains(&PortName::new("desktop.2", "default")));

    for index in [1, 2] {
        let lane = plain(FULL, index).await;
        let sms = &lane.services["sms-stub"];
        assert_eq!((sms.port, sms.literal_port), (3998, true));
        assert_eq!(sms.launch.env.values["PORT"], "3998");
        let payments = &lane.services["payments-api"];
        assert_eq!(payments.port, lane.port + 5);
        assert!(!payments.literal_port);
        assert_eq!(
            payments.launch.env.values["PORT"],
            payments.port.to_string()
        );
        assert_eq!(payments.launch.env.sources["PORT"], EnvLayer::Service);
        assert_eq!(
            payments.ready.check,
            ReadySpec::Http {
                port: payments.port,
                path: "/-/health".into(),
                status: None,
            }
        );
        // No allocated port may land on the reserved offset.
        for service in lane.services.values().filter(|s| !s.literal_port) {
            if service.name != "payments-api" {
                assert_ne!(service.port, lane.port + 5, "{}", service.unit);
            }
        }
    }
}

#[tokio::test]
/// Replicas get distinct ports and `replica.index` in their own expressions.
async fn replicas_have_distinct_ports() {
    let lane = plain(FULL, 1).await;
    let first = &lane.services["desktop.1"];
    let second = &lane.services["desktop.2"];
    assert_ne!(first.port, second.port);
    assert_eq!((first.replica, second.replica, second.replicas), (1, 2, 2));
    assert_eq!(lane.units_of("desktop").count(), 2);
}

#[test]
/// Profiles, globs, tags, exclusions and on-demand services.
fn selection() {
    let config = parse(FULL);
    let pick = |profile: Option<&str>, selectors: &[&str]| {
        let selectors: Vec<String> = selectors.iter().map(|s| (*s).to_owned()).collect();
        select(&config, profile, &selectors).map(|s| s.services)
    };
    assert_eq!(
        pick(None, &[]).expect("default"),
        [
            "catalog",
            "crm-api",
            "admin-api",
            "user-web",
            "queue-worker"
        ]
    );
    let everything = pick(Some("everything"), &[]).expect("everything");
    for absent in ["sms-stub", "desktop", "desktop-web", "ios-sim"] {
        assert!(!everything.iter().any(|s| s == absent), "{absent}");
    }
    assert!(everything.iter().any(|s| s == "pages-server"));
    assert_eq!(pick(Some("crm"), &[]).expect("crm"), ["catalog", "crm-api"]);
    assert_eq!(
        pick(Some("attach"), &[]).expect("tags select on-demand services"),
        ["desktop", "desktop-web"]
    );
    assert_eq!(pick(None, &["+mobile"]).expect("tag"), ["ios-sim"]);
    assert_eq!(pick(None, &["sms-*"]).expect("glob"), ["sms-stub"]);
    assert_eq!(
        pick(Some("e2e"), &["docs", "catalog"]).expect("selectors keep file order"),
        ["catalog", "docs"]
    );
    assert!(matches!(
        pick(None, &["nope"]),
        Err(ResolveError::NoMatch { .. })
    ));
    assert!(matches!(
        pick(Some("nope"), &[]),
        Err(ResolveError::UnknownProfile { .. })
    ));
    let e2e = select(&config, Some("e2e"), &[]).expect("e2e");
    assert_eq!(e2e.profile.as_deref(), Some("e2e"));
    assert_eq!(
        e2e.profile_options.expect("object form").run_scripts["dev"],
        "start"
    );

    let bare = parse("project: x\nservices:\n  a: { run: a }\n  b: { run: b, autostart: false }\n");
    let selection = select(&bare, None, &[]).expect("no profiles");
    assert_eq!(
        (selection.services, selection.profile),
        (vec!["a".to_owned()], None)
    );
}

/// Config with overlapping profiles for the multi-profile tests.
const PROFILES: &str = r#"
project: demo
services:
  api: { run_script: package.json#dev, tags: [api] }
  tester: { run_script: package.json#test }
  web: { run: web, tags: [web] }
tasks:
  only-a: { run: a, when: lane.start, profiles: [a] }
  only-b: { run: b, when: lane.start, profiles: [b] }
  only-default: { run: d, when: lane.start, profiles: [default] }
  always: { run: x, when: lane.start }
profiles:
  a:
    include: [api]
    infra: false
    env: { PX: a, PY: a }
    run_scripts: { dev: start, test: ci }
  b:
    include: [web, api]
    infra: false
    env: { PX: b }
    run_scripts: { dev: prod }
  c: [tester]
"#;

/// Resolves [`PROFILES`] in lane 1 with `profile` first, then `profiles`.
async fn with_profiles(profile: Option<&str>, profiles: &[&str]) -> devlanes_resolve::ResolvedLane {
    let config = parse(PROFILES);
    let facts = facts("demo", 1, Some("x"));
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(FakeRunner::new())];
    let profiles: Vec<String> = profiles.iter().map(|p| (*p).to_owned()).collect();
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.script_runners = &runners;
    input.profile = profile;
    input.profiles = &profiles;
    let lane = resolve_lane(input).await.expect("resolves");
    assert!(lane.errors.is_empty(), "{:?}", lane.errors);
    lane
}

#[test]
/// Several profiles select the union of their services in declaration order.
fn multi_profile_selection() {
    let config = parse(FULL);
    let selection = select_profiles(&config, &["web", "crm"], &[]).expect("union");
    assert_eq!(
        selection.services,
        ["catalog", "crm-api", "user-web", "docs"]
    );
    assert_eq!(selection.profiles, ["web", "crm"]);
    assert_eq!(selection.profile.as_deref(), Some("web"));
    let repeated = select_profiles(&config, &["crm", " ", "crm"], &[]).expect("dedup");
    assert_eq!(repeated.profiles, ["crm"]);
    assert!(matches!(
        select_profiles(&config, &["web", "nope"], &[]),
        Err(ResolveError::UnknownProfile { .. })
    ));
    let none: [&str; 0] = [];
    assert_eq!(
        select_profiles(&config, &none, &[])
            .expect("default")
            .profiles,
        ["default"]
    );

    let config = parse(PROFILES);
    let both = select_profiles(&config, &["a", "b"], &[]).expect("both");
    let options = both.profile_options.expect("merged");
    assert_eq!(
        options.env["PX"],
        Templated::Literal(Scalar::String("b".into()))
    );
    assert_eq!(
        options.env["PY"],
        Templated::Literal(Scalar::String("a".into()))
    );
    assert_eq!(options.run_scripts["dev"], "prod");
    assert_eq!(options.run_scripts["test"], "ci");
    assert!(matches!(options.infra, Some(Templated::Literal(false))));
    let mixed = select_profiles(&config, &["a", "c"], &[]).expect("mixed");
    assert_eq!(mixed.services, ["api", "tester"]);
    assert!(
        mixed
            .profile_options
            .expect("a has options")
            .infra
            .is_none()
    );
}

#[tokio::test]
/// Profile env applies in the order given; infra stays on unless every profile skips it.
async fn multi_profile_env_and_infra() {
    let ab = with_profiles(None, &["a", "b"]).await;
    let env = &ab.services["api"].launch.env;
    assert_eq!(
        (env.values["PX"].as_str(), env.values["PY"].as_str()),
        ("b", "a")
    );
    assert_eq!(env.sources["PX"], EnvLayer::Profile);
    assert!(!ab.infra_enabled);
    assert_eq!(ab.profiles, ["a", "b"]);

    let ba = with_profiles(None, &["b", "a"]).await;
    assert_eq!(ba.services["api"].launch.env.values["PX"], "a");
    // The legacy single profile counts as the first entry.
    let legacy = with_profiles(Some("b"), &["a"]).await;
    assert_eq!(legacy.profiles, ["b", "a"]);
    assert_eq!(legacy.profile.as_deref(), Some("b"));
    assert_eq!(legacy.services["api"].launch.env.values["PX"], "a");

    assert!(with_profiles(None, &["a", "c"]).await.infra_enabled);
    assert!(!with_profiles(None, &["a"]).await.infra_enabled);
    assert!(with_profiles(None, &[]).await.infra_enabled);
}

#[tokio::test]
/// `run_scripts` remaps merge in order; later profiles win per script name.
async fn multi_profile_run_scripts() {
    let ab = with_profiles(None, &["a", "b"]).await;
    assert_eq!(ab.services["api"].launch.args, ["run", "prod"]);
    assert_eq!(ab.services["tester"].launch.args, ["run", "ci"]);
    let ba = with_profiles(None, &["b", "a"]).await;
    assert_eq!(ba.services["api"].launch.args, ["run", "start"]);
}

#[tokio::test]
/// Tasks with `profiles:` run when any profile in effect matches.
async fn multi_profile_task_filter() {
    let active = |lane: &devlanes_resolve::ResolvedLane| -> Vec<String> {
        lane.tasks
            .values()
            .filter(|t| t.active)
            .map(|t| t.name.clone())
            .collect()
    };
    assert_eq!(
        active(&with_profiles(None, &["a"]).await),
        ["only-a", "always"]
    );
    assert_eq!(
        active(&with_profiles(None, &["b", "a"]).await),
        ["only-a", "only-b", "always"]
    );
    assert_eq!(
        active(&with_profiles(None, &[]).await),
        ["only-default", "always"]
    );
}

#[tokio::test]
/// Public URLs and origins carry the proxy's HTTPS port unless it is 443.
async fn public_https_port() {
    let config = parse(MINIMAL);
    let facts = facts("acme", 1, Some("x"));
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let infra = json!({"postgres": {"port": 15432, "ports": {"5432": 15432}}});
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(FakeRunner::new())];
    for (port, suffix) in [
        (None, ""),
        (Some(443), ""),
        (Some(8443), ":8443"),
        (Some(9443), ":9443"),
    ] {
        let mut input = ResolveInput::new(&config, &facts, block, &offsets);
        input.infra = &infra;
        input.script_runners = &runners;
        input.public_https_port = port;
        let layout = lane_layout(&input).expect("layout");
        let lane = resolve_lane(input).await.expect("resolves");
        assert!(lane.errors.is_empty(), "{:?}", lane.errors);
        let api_url = format!("https://api.acme-1.localhost{suffix}");
        let web_url = format!("https://acme-1.localhost{suffix}");
        assert_eq!(
            layout.services["api"].public_url.as_deref(),
            Some(api_url.as_str())
        );
        let api = &lane.services["api"];
        assert_eq!(api.public_url.as_deref(), Some(api_url.as_str()));
        assert_eq!(api.launch.env.values["DEVLANES_PUBLIC_URL"], api_url);
        // svc.api.public_url feeds web's env through an expression.
        assert_eq!(
            lane.services["web"].launch.env.values["VITE_API_URL"],
            api_url
        );
        assert_eq!(lane.lane_env()["DEVLANES_PUBLIC_URL_API"], api_url);
        assert_eq!(lane.lane_env()["DEVLANES_PUBLIC_URL_WEB"], web_url);
        assert_eq!(
            lane.svc_namespace()["api"]["public_url"],
            json!(api_url.as_str())
        );
        assert!(lane.origins.contains(&api_url) && lane.origins.contains(&web_url));
        let origins = &api.launch.env.values["DEVLANES_ORIGINS"];
        assert!(origins.split(',').any(|o| o == api_url), "{origins}");
        // Routes keep bare hostnames: the proxy matches on Host without the port.
        assert!(lane.routes.iter().any(|r| r.host == "api.acme-1.localhost"));
    }
    assert_eq!(
        devlanes_resolve::https_url("a.localhost", Some(443)),
        "https://a.localhost"
    );
    assert_eq!(
        devlanes_resolve::https_url("a.localhost", Some(8443)),
        "https://a.localhost:8443"
    );
}

#[tokio::test]
/// Services and tasks carry the core color: tag overrides, then categories, then hash.
async fn unit_colors() {
    let config = parse(
        r#"
project: demo
tags:
  essential: { color: bright-yellow, description: Always on }
  api: {}
services:
  catalog: { run: a, tags: [essential, api] }
  backend: { run: b, tags: [api] }
  web: { run: w, preset: vite }
  thing: { run: t }
  sms-stub: { run: s, replicas: 2 }
tasks:
  seed: { run: s }
"#,
    );
    assert_eq!(
        devlanes_resolve::tag_colors(&config),
        BTreeMap::from([("essential".to_owned(), "bright-yellow".to_owned())])
    );
    let facts = facts("demo", 1, None);
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let lane = resolve_lane(ResolveInput::new(&config, &facts, block, &offsets))
        .await
        .expect("resolves");
    let color = |unit: &str| lane.services[unit].color.as_str();
    assert_eq!(color("catalog"), "bright-yellow");
    assert_eq!(color("backend"), "blue");
    assert_eq!(color("web"), "green");
    assert_eq!(color("sms-stub.1"), "gray");
    assert_eq!(color("sms-stub.2"), "gray");
    let hashed = devlanes_core::color::unit_color(&[], "thing", None, &BTreeMap::new());
    assert_eq!(color("thing"), hashed);
    assert_ne!(hashed, "red");
    assert_eq!(
        lane.tasks["seed"].color,
        devlanes_core::color::unit_color(&[], "seed", None, &BTreeMap::new())
    );
    let layout = lane_layout(&ResolveInput::new(&config, &facts, block, &offsets)).expect("layout");
    assert_eq!(layout.services["catalog"].color, "bright-yellow");
}

#[test]
/// Profile summaries list selectors and literal `infra: false` for clients.
fn profile_summaries_for_clients() {
    let summaries = profile_summaries(&parse(FULL));
    let by_name = |name: &str| {
        summaries
            .iter()
            .find(|s| s.name == name)
            .expect("profile listed")
    };
    assert_eq!(summaries[0].name, "default");
    assert!(by_name("default").is_default);
    assert_eq!(by_name("default").include, ["+essential"]);
    assert!(by_name("staging-web").skips_infra);
    assert!(by_name("attach").skips_infra);
    assert!(!by_name("e2e").skips_infra);
    assert_eq!(by_name("everything").exclude, ["+desktop", "sms-stub"]);
    assert_eq!(
        serde_json::to_value(by_name("crm")).expect("json"),
        json!({"name": "crm", "include": ["crm-*", "catalog"], "exclude": [], "skips_infra": false, "is_default": false})
    );
}

#[tokio::test]
/// The default domain follows the lane index; lane vars see lane values.
async fn domain_and_lane_vars() {
    let main = plain(MINIMAL, 0).await;
    assert_eq!(main.domain, "acme.localhost");
    assert_eq!(main.services["web"].hosts, ["acme.localhost"]);
    let third = plain(MINIMAL, 3).await;
    assert_eq!(third.domain, "acme-3.localhost");
    assert_eq!(
        third.services["web"].launch.env.values["VITE_API_URL"],
        "https://api.acme-3.localhost"
    );
    let full = plain(FULL, 2).await;
    assert_eq!(full.domain, "acme-2.localhost");
    assert_eq!(full.vars["db_name"].to_string(), "acme_lane2");
    assert_eq!(full.vars["redis_db"].to_string(), "2");
    assert_eq!(
        full.setup["database"].launch.args[1],
        r#"psql "postgres://postgres:postgres@127.0.0.1:15432/postgres" -c "CREATE DATABASE acme_lane2""#
    );
}

#[tokio::test]
/// `projects.<name>` falls back through `default` and uses real values when present.
async fn projects_namespace_with_default() {
    let lane = plain(FULL, 1).await;
    let env = &lane.services["catalog"].launch.env.values;
    assert_eq!(
        env["REFEREE_URL"],
        "https://referee.staging.example.invalid"
    );
    assert_eq!(env["SENTRY_DSN"], "");

    let config = parse(FULL);
    let facts = facts("acme", 1, Some("feature"));
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let infra = infra();
    let projects = json!({"workflows": {"svc": {"referee": {"public_url": "https://referee.workflows-1.localhost"}}}});
    let client = BTreeMap::from([("SENTRY_DSN".to_owned(), "dsn".to_owned())]);
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.infra = &infra;
    input.projects = &projects;
    input.client_env = &client;
    let lane = resolve_lane(input).await.expect("resolves");
    let env = &lane.services["catalog"].launch.env.values;
    assert_eq!(env["REFEREE_URL"], "https://referee.workflows-1.localhost");
    assert_eq!(env["SENTRY_DSN"], "dsn");
}

#[tokio::test]
/// `lanes.main.svc` comes from lane 0's layout; without it only affected units fail.
async fn lanes_main_namespace() {
    let config = parse(FULL);
    let infra = infra();
    let (block0, offsets) = allocation(&config, 21_000, &[]);
    let main_facts = facts("acme", 0, Some("main"));
    let mut main_input = ResolveInput::new(&config, &main_facts, block0, &offsets);
    main_input.infra = &infra;
    let main = lane_layout(&main_input).expect("lane 0 layout");
    let namespace = main.svc_namespace();
    assert_eq!(
        namespace["crm-api"]["public_url"],
        json!("https://crm.acme.localhost")
    );

    let lane_facts = facts("acme", 1, Some("desktop-fix"));
    let block1 = PortBlock::new(22_000, block0.size).expect("block");
    let mut input = ResolveInput::new(&config, &lane_facts, block1, &offsets);
    input.infra = &infra;
    input.profile = Some("attach");
    input.lanes_main = Some(&namespace);
    let lane = resolve_lane(input).await.expect("resolves");
    assert!(!lane.infra_enabled);
    assert_eq!(
        lane.services["desktop.1"].launch.env.values["ACME_CRM_BASE_URL"],
        "https://crm.acme.localhost"
    );

    let mut input = ResolveInput::new(&config, &lane_facts, block1, &offsets);
    input.infra = &infra;
    input.profile = Some("attach");
    let lane = resolve_lane(input)
        .await
        .expect("lane-level resolution still works");
    assert!(matches!(
        lane.check_services(["desktop"]),
        Err(ResolveError::Expression { .. })
    ));

    // Lane 0 uses its own services for lanes.main.
    let mut input = ResolveInput::new(&config, &main_facts, block0, &offsets);
    input.infra = &infra;
    input.profile = Some("attach");
    let lane = resolve_lane(input).await.expect("resolves");
    assert_eq!(
        lane.services["desktop.2"].launch.env.values["ACME_CRM_BASE_URL"],
        "https://crm.acme.localhost"
    );
}

#[tokio::test]
/// The e2e profile runs `start` scripts instead of `dev`, keeping the source file.
async fn run_scripts_remap() {
    let config = parse(FULL);
    let facts = facts("acme", 1, Some("e2e"));
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let infra = infra();
    let runner = Arc::new(FakeRunner::new());
    let runners: [Arc<dyn ScriptRunner>; 1] = [runner.clone()];
    let client = client_env();
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.infra = &infra;
    input.profile = Some("e2e");
    input.script_runners = &runners;
    input.client_env = &client;
    let lane = resolve_lane(input).await.expect("resolves");
    let admin = &lane.services["admin-api"].launch;
    assert_eq!(admin.program, "pnpm");
    assert_eq!(admin.args, ["run", "start"]);
    assert_eq!(
        admin.cwd,
        std::path::Path::new("/work/acme-lanes/lane-1/apps/admin-api")
    );
    assert_eq!(admin.platform.as_deref(), Some("node"));
    assert_eq!(lane.services["extension"].launch.args, ["run", "start"]);
    // Tasks keep their script names.
    assert_eq!(
        lane.tasks["migrate"].launch.as_ref().expect("launch").args,
        ["run", "db-deploy"]
    );
    assert_eq!(admin.env.values["NODE_ENV"], "production");
    assert!(!lane.tasks["migrate"].active);
    assert!(lane.tasks["build"].active);
}

#[tokio::test]
/// `lane_env` carries one public URL per service, falling back to the internal URL.
async fn public_url_variables() {
    let lane = plain(FULL, 2).await;
    let env = lane.lane_env();
    assert_eq!(
        env["DEVLANES_PUBLIC_URL_CRM_API"],
        "https://crm.acme-2.localhost"
    );
    assert_eq!(
        env["DEVLANES_PUBLIC_URL_USER_WEB"],
        "https://app.acme-2.localhost"
    );
    assert_eq!(
        env["DEVLANES_PUBLIC_URL_PAGES_SERVER"],
        lane.services["pages-server"].url
    );
    assert_eq!(env["DEVLANES_LANE"], "2");
    assert!(!env.contains_key("PORT"));
}

#[tokio::test]
/// Contributor args go after a shell command and through the script runner.
async fn contributor_args_reach_commands() {
    let config = parse(
        r#"
project: demo
services:
  shell:
    preset: vite
    run: pnpm dev
  script:
    preset: vite
    run_script: web/package.json#dev
  argv:
    preset: vite
    run: [vite, "--mode", "dev x"]
"#,
    );
    let facts = facts("demo", 1, None);
    let flags: Arc<dyn Contributor> = Arc::new(FakeContributor {
        preset_ports: Some(("vite", "hmr")),
        port_flag_for: Some("vite"),
        ..FakeContributor::env("vite", 100, vec![])
    });
    let contributors = [flags];
    let (block, offsets) = allocation(&config, 21_000, &contributors);
    let runner = Arc::new(FakeRunner::new());
    let runners: [Arc<dyn ScriptRunner>; 1] = [runner.clone()];
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    input.script_runners = &runners;
    let lane = resolve_lane(input).await.expect("resolves");
    let port = |unit: &str| lane.services[unit].port.to_string();
    assert_eq!(
        lane.services["shell"].launch.args,
        [
            "-c".to_owned(),
            format!("pnpm dev --port {}", port("shell"))
        ]
    );
    assert_eq!(
        lane.services["argv"].launch.args,
        [
            "--mode".to_owned(),
            "dev x".into(),
            "--port".into(),
            port("argv")
        ]
    );
    assert_eq!(
        lane.services["script"].launch.args,
        [
            "run".to_owned(),
            "dev".into(),
            "--port".into(),
            port("script")
        ]
    );
    assert!(lane.services["script"].ports.contains_key("hmr"));
    // First call without args (for the script body), then with the preset flags.
    let calls = runner.calls.lock().expect("lock");
    assert_eq!(calls.len(), 2);
    assert!(calls[0].2.is_empty());
    assert_eq!(calls[1].2, ["--port".to_owned(), port("script")]);
}

#[tokio::test]
/// A locked vault fails the units that need it and names the profile.
async fn locked_secrets_fail_units() {
    let config = parse(FULL);
    let facts = facts("acme", 1, Some("x"));
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let infra = infra();
    let secrets = FakeSecrets {
        locked: BTreeSet::from([Some("admin-api".to_owned())]),
        values: BTreeMap::from([(
            Some("dev".to_owned()),
            BTreeMap::from([("STRIPE_KEY".into(), "sk_test".into())]),
        )]),
        remove: BTreeSet::from(["PATH".to_owned()]),
        ..FakeSecrets::default()
    };
    let client = client_env();
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.infra = &infra;
    input.client_env = &client;
    input.secrets = Some(&secrets as &dyn SecretsProvider);
    let lane = resolve_lane(input).await.expect("resolves");
    let error = lane
        .check_services(["catalog", "admin-api"])
        .expect_err("locked");
    assert!(matches!(
        &error,
        ResolveError::SecretsLocked { profile: Some(p), .. } if p == "admin-api"
    ));
    assert!(error.to_string().contains("devlanes secrets unlock"));
    let catalog = &lane.services["catalog"].launch.env;
    assert_eq!(catalog.values["STRIPE_KEY"], "sk_test");
    assert_eq!(catalog.sources["STRIPE_KEY"], EnvLayer::Secrets);
    assert!(
        !catalog.values.contains_key("PATH"),
        "fnox remove drops client vars"
    );
    // One provider call per (profile, explicit) pair.
    let calls = secrets.calls.lock().expect("lock");
    assert_eq!(
        calls
            .iter()
            .filter(|(p, _)| p.as_deref() == Some("dev"))
            .count(),
        1
    );
    assert!(calls.iter().all(|(_, explicit)| *explicit));
}

#[tokio::test]
/// Launch debug output never shows env values or arguments.
async fn debug_output_hides_values() {
    let config =
        parse("project: demo\nservices:\n  api:\n    run: server --token ${{ env.TOKEN }}\n");
    let facts = facts("demo", 1, None);
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let client = BTreeMap::from([("TOKEN".to_owned(), "s3cr3t-value".to_owned())]);
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.client_env = &client;
    let lane = resolve_lane(input).await.expect("resolves");
    let debug = format!("{lane:?}");
    assert!(!debug.contains("s3cr3t-value"));
    assert!(debug.contains("TOKEN"));
}

#[tokio::test]
/// Hosts that reference each other cannot be ordered and fail with the chain.
async fn identity_cycles_are_rejected() {
    let mut config = parse(
        "project: demo\nservices:\n  a: { run: a, hosts: [a.localhost] }\n  b: { run: b, hosts: [b.localhost] }\n",
    );
    config.services["a"].hosts = Some(vec![HostConfig::Name(Templated::Expression(
        "a${{ svc.b.port }}.localhost".into(),
    ))]);
    config.services["b"].hosts = Some(vec![HostConfig::Name(Templated::Expression(
        "b${{ svc.a.port }}.localhost".into(),
    ))]);
    let facts = facts("demo", 1, None);
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let error =
        lane_layout(&ResolveInput::new(&config, &facts, block, &offsets)).expect_err("cycle");
    assert_eq!(
        error,
        ResolveError::Cycle {
            chain: vec!["a".into(), "b".into(), "a".into()]
        }
    );
}

#[tokio::test]
/// Platform comes from files in the working directory when not configured.
async fn platform_inference() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("web")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("ml")).expect("mkdir");
    std::fs::write(dir.path().join("web/package.json"), "{}").expect("write");
    std::fs::write(dir.path().join("ml/pyproject.toml"), "").expect("write");
    let config = parse(
        "project: demo\nservices:\n  web: { run: dev, cwd: web }\n  ml: { run: dev, cwd: ml }\n  bare: { run: dev }\n  forced: { run: dev, cwd: web, platform: python }\n",
    );
    let facts = devlanes_resolve::LaneFacts::new("demo", dir.path(), 0, dir.path(), None);
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let contributors: [Arc<dyn Contributor>; 2] = [
        Arc::new(devlanes_platform_python::PythonContributor),
        Arc::new(devlanes_platform_node::NodeContributor),
    ];
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    let lane = resolve_lane(input).await.expect("resolves");
    let platform = |unit: &str| lane.services[unit].launch.platform.clone();
    assert_eq!(platform("web").as_deref(), Some("node"));
    assert_eq!(platform("ml").as_deref(), Some("python"));
    assert_eq!(platform("bare"), None);
    assert_eq!(platform("forced").as_deref(), Some("python"));

    // Without platform contributors nothing is inferred: resolve has no built-in rules.
    let lane = resolve_lane(ResolveInput::new(&config, &facts, block, &offsets))
        .await
        .expect("resolves");
    assert_eq!(lane.services["web"].launch.platform, None);
    assert_eq!(
        lane.services["forced"].launch.platform.as_deref(),
        Some("python")
    );
}

#[tokio::test]
/// Highest confidence wins; ties go to higher priority, then the smaller id,
/// whatever the registration order.
async fn detection_precedence() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("marker"), "").expect("write");
    let fake = |id, priority, platform, confidence| -> Arc<dyn Contributor> {
        Arc::new(FakeContributor::platform(
            id,
            priority,
            platform,
            vec![(Probe::Cwd("marker"), confidence)],
        ))
    };
    let cx = DetectCx {
        kind: UnitKind::Service,
        name: "api",
        lane_path: dir.path(),
        cwd: dir.path(),
        script: None,
    };
    let pick = |list: Vec<Arc<dyn Contributor>>| async move {
        let forward = detect_platform(&list, &cx).await;
        let reversed: Vec<_> = list.into_iter().rev().collect();
        assert_eq!(
            forward,
            detect_platform(&reversed, &cx).await,
            "order-independent"
        );
        forward
    };
    let confidence = pick(vec![
        fake("a-low", 90, "low", 60),
        fake("b-high", 10, "high", 70),
    ])
    .await;
    assert_eq!(confidence.as_deref(), Some("high"));
    let priority = pick(vec![
        fake("a", 50, "first", 60),
        fake("b", 60, "second", 60),
    ])
    .await;
    assert_eq!(priority.as_deref(), Some("second"));
    let id = pick(vec![fake("zeta", 50, "z", 60), fake("alpha", 50, "a", 60)]).await;
    assert_eq!(id.as_deref(), Some("a"));
    let env_only: Arc<dyn Contributor> = Arc::new(FakeContributor::env("env", 99, vec![]));
    assert_eq!(pick(vec![env_only]).await, None);

    // An explicit platform skips detection even when a contributor claims the unit.
    let config = parse(
        "project: demo\nservices:\n  api: { run: dev, platform: rust }\n  free: { run: dev }\n",
    );
    let facts = devlanes_resolve::LaneFacts::new("demo", dir.path(), 0, dir.path(), None);
    let (block, offsets) = allocation(&config, 21_000, &[]);
    let contributors = [fake("claims", 50, "node", 99)];
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    let lane = resolve_lane(input).await.expect("resolves");
    assert_eq!(
        lane.services["api"].launch.platform.as_deref(),
        Some("rust")
    );
    assert_eq!(
        lane.services["free"].launch.platform.as_deref(),
        Some("node")
    );
}

#[tokio::test]
/// The real platform crates together: bun beats node only with a bun marker,
/// deno beats node with a deno config, rust yields to node in mixed directories,
/// and `platform: bun|deno|rust` parse as explicit values.
async fn platform_crates_together() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let write = |path: &str, contents: &str| {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, contents).expect("write");
    };
    write("plain/package.json", "{}");
    write("bunlock/package.json", "{}");
    write("bunlock/bun.lock", "");
    write("pinned/package.json", r#"{"packageManager":"bun@1.3.2"}"#);
    write("denoapp/package.json", "{}");
    write("denoapp/deno.json", "{}");
    write("crate/Cargo.toml", "[package]\n");
    write("mixed/package.json", "{}");
    write("mixed/Cargo.toml", "[package]\n");
    let config = parse(
        r#"
project: demo
services:
  plain: { run: dev, cwd: plain }
  bunlock: { run: dev, cwd: bunlock }
  bunscript: { run_script: bunlock/package.json#dev }
  nodescript: { run_script: plain/package.json#dev }
  pinned: { run: dev, cwd: pinned }
  denoapp: { run: dev, cwd: denoapp }
  crate: { run: cargo run, cwd: crate }
  mixed: { run: dev, cwd: mixed }
  forced-node: { run: dev, cwd: bunlock, platform: node }
  forced-bun: { run: dev, platform: bun }
  forced-deno: { run: dev, platform: deno }
"#,
    );
    let facts = devlanes_resolve::LaneFacts::new("demo", root, 0, root, None);
    let contributors: [Arc<dyn Contributor>; 5] = [
        Arc::new(devlanes_platform_rust::RustContributor),
        Arc::new(devlanes_platform_deno::DenoContributor),
        Arc::new(devlanes_platform_bun::BunContributor),
        Arc::new(devlanes_platform_python::PythonContributor),
        Arc::new(devlanes_platform_node::NodeContributor),
    ];
    let (block, offsets) = allocation(&config, 21_000, &contributors);
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(FakeRunner::new())];
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    input.script_runners = &runners;
    let lane = resolve_lane(input).await.expect("resolves");
    assert!(lane.errors.is_empty(), "{:?}", lane.errors);
    let platform = |unit: &str| {
        lane.services[unit]
            .launch
            .platform
            .clone()
            .unwrap_or_default()
    };
    for (unit, expected) in [
        ("plain", "node"),
        ("bunlock", "bun"),
        ("bunscript", "bun"),
        ("nodescript", "node"),
        ("pinned", "bun"),
        ("denoapp", "deno"),
        ("crate", "rust"),
        ("mixed", "node"),
        ("forced-node", "node"),
        ("forced-bun", "bun"),
        ("forced-deno", "deno"),
    ] {
        assert_eq!(platform(unit), expected, "{unit}");
    }
    // Only the winning platform's contributor sets its variables.
    let bun = &lane.services["bunlock"].launch.env;
    assert_eq!(bun.values["FORCE_COLOR"], "1");
    assert!(!bun.values.contains_key("DENO_NO_UPDATE_CHECK"));
    let deno = &lane.services["denoapp"].launch.env;
    assert_eq!(deno.values["DENO_NO_UPDATE_CHECK"], "1");
    assert_eq!(
        lane.services["crate"].launch.env.values["CARGO_TERM_COLOR"],
        "always"
    );
}

#[tokio::test]
/// Snapshot of examples/minimal.yaml in lane 1 with fake integrations.
async fn snapshot_minimal() {
    let config = parse(MINIMAL);
    let facts = facts("acme", 1, Some("feature/login"));
    let node: Arc<dyn Contributor> = Arc::new(FakeContributor {
        env: vec![("FORCE_COLOR", "1")],
        ..FakeContributor::platform("fake-node", 50, "node", FakeContributor::node_detection())
    });
    let contributors = [node];
    let (block, offsets) = allocation(&config, 21_000, &contributors);
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(FakeRunner::new())];
    let infra = json!({"postgres": {"port": 15432, "ports": {"5432": 15432}}});
    let client = client_env();
    let ca = BTreeMap::from([(
        "DEVLANES_CA_CERT".to_owned(),
        "/home/dev/.devlanes/ca/ca.pem".to_owned(),
    )]);
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    input.script_runners = &runners;
    input.infra = &infra;
    input.client_env = &client;
    input.ca_env = &ca;
    let lane = resolve_lane(input).await.expect("resolves");
    assert!(lane.errors.is_empty(), "{:?}", lane.errors);
    insta::assert_json_snapshot!(lane_view(&lane));
}

#[tokio::test]
/// Snapshot of examples/devlanes.yaml in lane 2 with fake integrations and secrets.
async fn snapshot_reference() {
    let config = parse(FULL);
    let facts = facts("acme", 2, Some("feature/billing"));
    let mise: Arc<dyn Contributor> = Arc::new(FakeContributor::env(
        "fake-mise",
        10,
        vec![("PATH", "/mise/bin:/usr/bin:/bin")],
    ));
    let node: Arc<dyn Contributor> = Arc::new(FakeContributor {
        env: vec![("FORCE_COLOR", "1")],
        ..FakeContributor::platform("fake-node", 50, "node", FakeContributor::node_detection())
    });
    // Preset options are opaque to config, so the inspector port comes from the
    // wrangler contributor, as it does with the real WranglerPreset.
    let wrangler: Arc<dyn Contributor> = Arc::new(FakeContributor {
        preset_ports: Some(("wrangler", "inspector")),
        ..FakeContributor::env("fake-wrangler", 100, vec![])
    });
    let contributors = [node, mise, wrangler];
    let (block, offsets) = allocation(&config, 21_000, &contributors);
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(FakeRunner::new())];
    let infra = infra();
    let client = client_env();
    let secrets = FakeSecrets {
        values: BTreeMap::from([
            (
                Some("dev".to_owned()),
                BTreeMap::from([("ACME_STRIPE_KEY".into(), "sk_dev".into())]),
            ),
            (
                Some("admin-api".to_owned()),
                BTreeMap::from([("ADMIN_TOKEN".into(), "adm".into())]),
            ),
        ]),
        ..FakeSecrets::default()
    };
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    input.script_runners = &runners;
    input.infra = &infra;
    input.client_env = &client;
    input.secrets = Some(&secrets as &dyn SecretsProvider);
    let lane = resolve_lane(input).await.expect("resolves");
    assert!(lane.errors.is_empty(), "{:?}", lane.errors);
    insta::assert_json_snapshot!(lane_view(&lane));
}

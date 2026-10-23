//! Expression references, endpoint cycles and port-block checks.

use devlanes_config::*;
use serde_json::json;

/// Load a project that must fail and return the message.
fn error(source: &str) -> String {
    parse_project(source, None).expect_err(source).message
}

/// Only env.PORT and hosts determine an endpoint, so reading URLs elsewhere is never a cycle.
#[test]
fn endpoint_cycles_only_through_port_and_hosts() {
    let mutual = "project: demo\nservices:\n  api:\n    run: server\n    env: { WEB_URL: '${{ services.web.public_url }}', SELF: '${{ services.api.url }}' }\n    hosts: ['api.${{ lane.domain }}']\n  web:\n    run: server\n    env: { API_URL: '${{ services.api.public_url }}' }\n    hosts: ['web.${{ lane.domain }}']\n";
    parse_project(mutual, None).expect("services may read each other's URLs");
    let own_port_in_host = "project: demo\nservices:\n  api:\n    run: server\n    hosts: ['api-${{ services.api.port }}.localhost']\n";
    parse_project(own_port_in_host, None).expect("hosts do not determine the port");
    for reference in ["services.api_a.port", "services['api-a'].url"] {
        let source = format!(
            "project: demo\nservices:\n  api-a:\n    run: server\n    env: {{ PORT: '${{{{ services.api_b.port + 1 }}}}' }}\n  api-b:\n    run: server\n    env:\n      PORT: \"${{{{ {reference} }}}}\"\n"
        );
        let message = error(&source);
        assert!(message.contains("cycle"), "{reference}: {message}");
        assert!(message.starts_with("services.api-"), "{message}");
    }
    let message = error(
        "project: demo\nlanes:\n  vars:\n    host: 'x-${{ services.api.public_url }}'\nservices:\n  api:\n    run: server\n    hosts: ['${{ lane.host }}']\n",
    );
    assert!(message.contains("cycle"), "{message}");
}

/// A single-entry `hosts` determines `public_url` like a list entry does.
#[test]
fn single_host_public_url_cycles_are_rejected() {
    for source in [
        "project: demo\nservices:\n  a:\n    run: server\n    hosts: 'a.${{ services.b.public_url }}'\n  b:\n    run: server\n    hosts: 'b.${{ services.a.public_url }}'\n",
        "project: demo\nservices:\n  a:\n    run: server\n    hosts: 'a.${{ self.public_url }}'\n",
    ] {
        let message = error(source);
        assert!(message.contains("cycle"), "{source}: {message}");
        assert!(message.starts_with("services.a.hosts"), "{message}");
    }
}

/// Matrix rows are lane vars and get the same cycle check, located at the row.
#[test]
fn matrix_cycles_are_rejected() {
    let source = "project: demo\nlanes:\n  matrix:\n    - { a: '1' }\n    - { a: '${{ lane.b }}', b: '${{ lane.a }}' }\n";
    let error = parse_project(source, None).expect_err("matrix cycle");
    assert!(
        error.message.starts_with("lanes.matrix.1."),
        "{}",
        error.message
    );
    assert!(error.message.contains("cycle"), "{}", error.message);
    assert_eq!(error.location.line, 5);
}

/// References are checked everywhere, including unused templates and lanes.main.
#[test]
fn unknown_references_are_rejected() {
    for (source, expected) in [
        (
            "project: demo\ntemplates:\n  unused:\n    env: { X: '${{ services.missing.url }}' }\n",
            "templates.unused.env.X: unknown service reference `missing`",
        ),
        (
            "project: demo\nglobal_env:\n  DB: '${{ infra.postgres.port }}'\n",
            "global_env.DB: unknown container `postgres`",
        ),
        (
            "project: demo\nservices:\n  api: { run: server }\nprofiles:\n  remote:\n    include: [api]\n    env: { URL: '${{ lanes.main.services.ap.url }}' }\n",
            "profiles.remote.env.URL: unknown service reference `ap` in lanes.main",
        ),
    ] {
        let message = error(source);
        assert!(message.contains(expected), "{message}");
    }
    parse_project(
        "project: demo\ninfra:\n  containers:\n    object-store: { image: minio }\nglobal_env:\n  S3: '${{ infra.object_store.port }}'\n",
        None,
    )
    .expect("declared container");
    parse_project(
        "project: demo\ninfra:\n  compose: compose.yaml\nglobal_env:\n  DB: '${{ infra.postgres.port }}'\n",
        None,
    )
    .expect("Compose services are checked against the resolved model later");
}

/// Attributes are checked against what the runtime context defines, with a suggestion.
#[test]
fn unknown_attributes_are_rejected() {
    for (source, needle, expected) in [
        (
            "project: demo\nservices:\n  api:\n    run: server\n    hosts: [\"${{ lane.domian }}\"]\n",
            "domian",
            "services.api.hosts.0: `lane.domian` is not defined; did you mean `lane.domain`?",
        ),
        (
            "project: demo\nlanes:\n  vars: { db_name: x }\nglobal_env:\n  DB: '${{ lane.db_nmae }}'\n",
            "db_nmae",
            "`lane.db_nmae` is not defined; did you mean `lane.db_name`?",
        ),
        (
            "project: demo\nglobal_env:\n  P: '${{ project.primary_pth }}'\n",
            "primary_pth",
            "did you mean `project.primary_path`?",
        ),
        (
            "project: demo\nservices:\n  api: { run: server }\n  web:\n    run: server\n    env: { API: '${{ services.api.pubilc_url }}' }\n",
            "pubilc_url",
            "`services.api.pubilc_url` is not defined; did you mean `services.api.public_url`?",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: 'server ${{ self.prot }}'\n",
            "self.prot",
            "did you mean `self.port`?",
        ),
        (
            "project: demo\nservices:\n  api:\n    run: 'server ${{ replica.idx }}'\n",
            "replica.idx",
            "`replica.idx` is not defined",
        ),
        (
            "project: demo\nservices:\n  api: { run: server }\n  web:\n    run: server\n    env: { API: '${{ services.apl.url }}' }\n",
            "services.apl",
            "unknown service reference `apl`; did you mean `api`?",
        ),
        (
            "project: demo\ninfra:\n  containers:\n    cache: { image: redis, ports: { main: 6379 } }\nglobal_env:\n  R: '${{ infra.cache.ports.mian }}'\n",
            "mian",
            "did you mean `infra.cache.ports.main`?",
        ),
        (
            "project: demo\nglobal_env:\n  X: '${{ lnae.name }}'\n",
            "lnae",
            "unknown expression context `lnae`; did you mean `lane`?",
        ),
        (
            "project: demo\nglobal_env:\n  X: '${{ svc.api.url }}'\n",
            "svc",
            "unknown expression context `svc`; did you mean `services`?",
        ),
        (
            "project: demo\nglobal_env:\n  X: '${{ projects.other.svc.api.url }}'\n",
            "svc",
            "`projects.other.svc` is not defined; did you mean `projects.other.services`?",
        ),
    ] {
        let error = parse_project(source, None).expect_err(source);
        assert!(error.message.contains(expected), "{}", error.message);
        let line = source
            .lines()
            .position(|l| l.contains(needle))
            .expect("needle")
            + 1;
        assert_eq!(error.location.line, line, "{}", error.message);
    }
    parse_project(
        "project: demo\nlanes:\n  matrix:\n    - { db-name: a }\n    - { db-name: b, extra: c }\nservices:\n  api:\n    run: 'server ${{ lane.db_name }} ${{ lane[''db-name''] }} ${{ lane.label }} ${{ lane.path }} ${{ replica.count }}'\n    hosts: ['${{ lane.domain }}']\n    env: { A: '${{ self.ports.debug }}', B: '${{ lanes.main.services.api.public_url }}', C: '${{ env.ANY }}', D: '${{ projects.other.services.x.url | default(\"\") }}' }\n",
        None,
    )
    .expect("known attributes");
}

/// `services.my-api` is a subtraction in expressions; the error names the escape.
#[test]
fn dashed_names_explain_the_subtraction() {
    let message = error(
        "project: demo\nservices:\n  my-api: { run: server }\n  web:\n    run: server\n    env: { API: '${{ services.my-api.url }}' }\n",
    );
    assert!(
        message.contains(
            "`services.my-api.url` reads as `services.my - api.url`; write `services['my-api'].url` or `services.my_api.url`"
        ),
        "{message}"
    );
}

/// A literal `PORT` must be a port number.
#[test]
fn literal_port_must_be_a_port() {
    for value in ["abc", "0", "70000", "true"] {
        let source = format!(
            "project: demo\nservices:\n  api:\n    run: server\n    env: {{ PORT: {value} }}\n"
        );
        let message = error(&source);
        assert!(
            message.starts_with("services.api.env.PORT: must be a port number"),
            "{value}: {message}"
        );
    }
    for value in ["3000", "'3000'", "\"${{ lane.port + 1 }}\""] {
        let source = format!(
            "project: demo\nservices:\n  api:\n    run: server\n    env: {{ PORT: {value} }}\n"
        );
        parse_project(&source, None).unwrap_or_else(|e| panic!("{value}: {}", e.message));
    }
}

/// Fixed blocks count registered preset ports and bracketed `lane['port']` offsets.
#[test]
fn block_size_counts_preset_ports_and_offsets() {
    let source = "project: demo\nlanes: { block_size: 2 }\nservices:\n  api: { run_script: { file: package.json, name: dev }, preset: custom }\n";
    let config = parse_project(source, None).expect("no registry, no preset ports");
    let accept = |_: &str, _: Option<&serde_json::Value>| Ok(());
    validate_with_presets(&config, &accept).expect("without preset ports");
    let message = validate_with_preset_ports(&config, &accept, &|preset| {
        assert_eq!(preset, "custom");
        vec!["inspector".into(), "debug".into()]
    })
    .expect_err("preset ports exceed the block")
    .message;
    assert!(message.starts_with("lanes.block_size:"), "{message}");

    // Doubled quotes are YAML's escape inside the single-quoted value below.
    for expression in ["lane[''port''] + 5", "5 + lane.port", "lane.port+5"] {
        let source = format!(
            "project: demo\nlanes: {{ block_size: 5 }}\nservices:\n  api: {{ run: server, env: {{ PORT: '${{{{ {expression} }}}}' }} }}\n"
        );
        assert!(
            error(&source).contains("offset 5 exceeds the block"),
            "{expression}"
        );
    }
    parse_project(
        "project: demo\nlanes: { block_size: 5, vars: { port_offset: 9 } }\nservices:\n  api: { run: server, env: { PORT: '${{ lane.port + 4 }}', X: '${{ lane.port_offset + 9 }}' } }\n",
        None,
    )
    .expect("offsets inside the block and other variables");
}

/// A dashed key beside its underscore spelling keeps both and does not break evaluation.
#[test]
fn ambiguous_aliases_stay_literal() {
    let context = ExpressionContext {
        env: json!({"A-B":"dash","A_B":"underscore","C-D":"only"}),
        ..Default::default()
    };
    assert_eq!(
        evaluate("${{ env.A_B }}", &context).expect("literal"),
        "underscore"
    );
    assert_eq!(
        evaluate("${{ env['A-B'] }}", &context).expect("dash"),
        "dash"
    );
    assert_eq!(evaluate("${{ env.C_D }}", &context).expect("alias"), "only");
    let context = ExpressionContext {
        env: json!({"X-Y_Z":"one","X_Y-Z":"two"}),
        ..Default::default()
    };
    assert!(evaluate("${{ env.X_Y_Z }}", &context).is_err());
    assert_eq!(
        evaluate("${{ env['X-Y_Z'] }}", &context).expect("key"),
        "one"
    );
}

/// `${{ '${{' }}` produces a literal `${{`, alone or inside text.
#[test]
fn literal_expression_opener() {
    let context = ExpressionContext::default();
    assert_eq!(evaluate("${{ '${{' }}", &context).expect("alone"), "${{");
    assert_eq!(
        evaluate("echo ${{ '${{' }} x }}", &context).expect("in text"),
        "echo ${{ x }}"
    );
    let config = parse_project(
        "project: demo\nservices:\n  api:\n    run: server\n    env: { RAW: \"${{ '${{' }} lane.index }}\" }\n",
        None,
    )
    .expect("escaped opener");
    let env = evaluate_env(&config.services["api"].env, &context).expect("evaluated");
    assert_eq!(env["RAW"].to_string(), "${{ lane.index }}");
}

/// Schema strings for expressions, durations, sizes, percentages and port ranges match the
/// parsers, and preset options require the matching preset.
#[test]
fn schema_patterns_and_preset_agreement() {
    let project = jsonschema::validator_for(&super::registered_schema()).expect("schema");
    for value in [
        json!({"services":{"api":{"stop":{"timeout":"15s"}}}}),
        json!({"services":{"api":{"stop":{"timeout":"${{ lane.index }}s"}}}}),
        json!({"services":{"api":{"replicas":2}}}),
        json!({"services":{"api":{"preset":"vite","vite":{"args":[]}}}}),
        json!({"services":{"api":{"extends":"web","vite":{"args":[]}}}}),
        // An override or `extends` child may set one `run_script` key and keep the other.
        json!({"services":{"api":{"run_script":{"name":"dev"}}}}),
        json!({"templates":{"web":{"run_script":{"file":"package.json"}}}}),
    ] {
        assert!(project.is_valid(&value), "rejected {value}");
    }
    for value in [
        json!({"services":{"api":{"stop":{"timeout":"10fortnights"}}}}),
        json!({"services":{"api":{"replicas":"two"}}}),
        json!({"services":{"api":{"replicas":"${{ lane.index }}"}}}),
        json!({"services":{"api":{"preset":"next","vite":{"args":[]}}}}),
        json!({"services":{"api":{"run_script":{"source":"package.json","name":"dev"}}}}),
    ] {
        assert!(!project.is_valid(&value), "accepted {value}");
    }
    let user = jsonschema::validator_for(&user_schema()).expect("user schema");
    for value in [
        json!({"logs":{"retention":{"max_size":"5GiB"}}}),
        json!({"startup":{"target_cpu":"12.5%"}}),
        json!({"ports":{"range":"20000-32767"}}),
    ] {
        assert!(user.is_valid(&value), "rejected {value}");
    }
    for value in [
        json!({"logs":{"retention":{"max_size":"5 gigs"}}}),
        json!({"startup":{"target_cpu":"75"}}),
        json!({"ports":{"range":"lots"}}),
    ] {
        assert!(!user.is_valid(&value), "accepted {value}");
    }
}

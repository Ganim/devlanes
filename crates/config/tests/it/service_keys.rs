use devlanes_config::{parse_project, parse_project_with};

/// Service typos remain unknown-key errors even when their values resemble preset mappings.
#[test]
fn unknown_service_keys_have_suggestions_and_spans() {
    for value in ["", "{}", "{ tcp: true }", "false"] {
        let source =
            format!("project: acme\nservices:\n  api:\n    run: server\n    raedy: {value}\n");
        let error = parse_project(&source, None).expect_err("unknown service key");
        assert!(
            error
                .message
                .contains("unknown key `raedy`; did you mean `ready`?"),
            "{}",
            error.message
        );
        assert!(!error.message.contains("options require"));
        assert_eq!(error.location.file, "devlanes.yaml");
        assert_eq!(error.location.line, 5);
        assert!(error.location.column > 1);
    }
}

/// The selected preset is a suggestion candidate regardless of declaration order or inheritance.
#[test]
fn misspelled_preset_key_uses_effective_preset() {
    for source in [
        "project: acme\nservices:\n  api:\n    run_script: { file: package.json, name: dev }\n    wranglr: {}\n    preset: wrangler\n",
        "project: acme\ntemplates:\n  worker: { preset: wrangler }\nservices:\n  api:\n    extends: worker\n    run_script: { file: package.json, name: dev }\n    wranglr: {}\n",
    ] {
        let error = parse_project(source, None).expect_err("wrong preset key");
        assert!(
            error
                .message
                .contains("unknown key `wranglr`; did you mean `wrangler`?"),
            "{}",
            error.message
        );
        assert_eq!(
            error.location.line,
            source
                .lines()
                .position(|line| line.contains("wranglr:"))
                .expect("key line")
                + 1
        );
    }
    let source = "project: acme\ntemplates:\n  worker: { preset: wrangler }\nservices:\n  api: { extends: worker, run_script: { file: package.json, name: dev } }\n";
    let error = parse_project(source, Some("services:\n  api:\n    wranglr: {}\n"))
        .expect_err("local typo");
    assert!(error.message.contains("did you mean `wrangler`?"));
    assert_eq!(error.location.file, "devlanes.local.yaml");
    assert_eq!(error.location.line, 3);
    let config = parse_project(
        source,
        Some("services:\n  api:\n    wrangler: { isolate_registry: true }\n"),
    )
    .expect("inherited preset options");
    assert_eq!(
        config.services["api"].preset_options["wrangler"]["isolate_registry"],
        true
    );
}

/// An options block alone does not select an integration.
#[test]
fn options_without_preset_explain_selection() {
    let error = parse_project(
        "project: acme\nservices:\n  api:\n    run: server\n    wrangler: {}\n",
        None,
    )
    .expect_err("missing preset");
    assert!(
        error
            .message
            .contains("set `preset: wrangler` to use these options"),
        "{}",
        error.message
    );
    assert_eq!(error.location.line, 5);
    // Typed registrations must classify the key before invoking their option parser.
    let source = "project: acme\nservices:\n  api:\n    run: server\n    custom: {}\n";
    let error = parse_project_with::<super::TestExtensions>(source, None)
        .expect_err("registered preset missing");
    assert!(
        error
            .message
            .contains("set `preset: custom` to use these options"),
        "{}",
        error.message
    );
    let source = "project: acme\nservices:\n  api:\n    run_script: { file: package.json, name: dev }\n    custm: {}\n    preset: custom\n";
    let error = parse_project_with::<super::TestExtensions>(source, None)
        .expect_err("registered preset typo");
    assert!(
        error.message.contains("did you mean `custom`?"),
        "{}",
        error.message
    );
}

/// Deferred option parsing must retain duplicate-key rejection.
#[test]
fn duplicate_preset_option_is_rejected() {
    let source = "project: acme\nservices:\n  api:\n    run_script: { file: package.json, name: dev }\n    preset: wrangler\n    wrangler:\n      isolate_registry: true\n      isolate_registry: false\n";
    assert!(parse_project(source, None).is_err());
}

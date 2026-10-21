use devlanes_config::*;
use serde_json::json;

/// Local tag fields replace individually while undeclared service tags remain valid.
#[test]
fn tag_overrides_merge_per_field() {
    let source = "project: acme\ntags:\n  api: { color: cyan, description: Original }\n  web: { color: blue }\nservices:\n  api: { run: server, tags: [api, custom] }\n";
    let local = "tags:\n  api: { color: bright-magenta }\n  web: { description: Frontend }\n  worker: { description: Background jobs }\n";
    let config = parse_project(source, Some(local)).expect("merged tags");
    assert_eq!(config.tags["api"].color, Some(TagColor::BrightMagenta));
    assert_eq!(config.tags["api"].description.as_deref(), Some("Original"));
    assert_eq!(config.tags["web"].color, Some(TagColor::Blue));
    assert_eq!(config.tags["web"].description.as_deref(), Some("Frontend"));
    assert!(config.tags["worker"].color.is_none());
    assert!(!config.tags.contains_key("custom"));
    assert_eq!(
        config.services["api"].tags.as_deref(),
        Some(["api".into(), "custom".into()].as_slice())
    );
    assert!(
        parse_project("project: acme", None)
            .expect("optional tags")
            .tags
            .is_empty()
    );
}

/// Runtime parsing and the schema share the fixed terminal palette.
#[test]
fn tag_palette_matches_schema() {
    let validator = jsonschema::validator_for(&super::registered_schema()).expect("schema");
    for value in [
        "red",
        "green",
        "yellow",
        "blue",
        "magenta",
        "cyan",
        "white",
        "gray",
        "bright-red",
        "bright-green",
        "bright-yellow",
        "bright-blue",
        "bright-magenta",
        "bright-cyan",
    ] {
        let color: TagColor = value.parse().expect("palette color");
        assert_eq!(color.as_str(), value);
        assert_eq!(serde_json::to_value(color).expect("serialize"), value);
        assert!(validator.is_valid(&json!({"tags":{"api":{"color":value}}})));
    }
    for value in ["", "black", "bright-white", "bright-gray", "blu", "#ffffff"] {
        assert!(value.parse::<TagColor>().is_err());
        assert!(!validator.is_valid(&json!({"tags":{"api":{"color":value}}})));
    }
    assert!(!validator.is_valid(&json!({"tags":{"api":{"unknown":true}}})));
}

/// Invalid colors offer a suggestion at their original YAML location.
#[test]
fn tag_color_diagnostic() {
    let error = parse_project("project: acme\ntags:\n  api:\n    color: bleu\n", None)
        .expect_err("bad color");
    assert!(error.message.contains("did you mean `blue`?"));
    assert_eq!(error.location.line, 4);
    let error = parse_project(
        "project: acme\n",
        Some("tags:\n  api:\n    color: bright-cayn\n"),
    )
    .expect_err("bad local color");
    assert!(error.message.contains("did you mean `bright-cyan`?"));
    assert_eq!(error.location.file, "devlanes.local.yaml");
    assert_eq!(error.location.line, 3);
}

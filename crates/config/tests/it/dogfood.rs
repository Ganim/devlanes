//! The anonymized dogfood fixtures under `fixtures/dogfood-*` load, validate with the
//! registered presets and match the registered JSON Schema.

use devlanes_config::*;
use std::path::{Path, PathBuf};

/// Directory of a repository-level fixture.
fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Both fixtures pass the same checks as `devlanes config check`, plus the schema.
#[test]
fn dogfood_fixtures_pass_config_check() {
    let validator =
        jsonschema::validator_for(&super::registered_schema()).expect("registered schema");
    for (name, project) in [
        ("dogfood-monorepo", "acme"),
        ("dogfood-desktop", "acme-desktop"),
    ] {
        let dir = fixture(name);
        let config = load_project(&dir).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(config.project, project);
        let report = validate_with_presets(&config, &super::registered_validator)
            .unwrap_or_else(|e| panic!("{name}: {e:?}"));
        assert_eq!(report.project, project);
        assert!(
            report.warnings.is_empty(),
            "{name}: unexpected warnings {:?}",
            report.warnings
        );
        let source = std::fs::read_to_string(dir.join("devlanes.yaml")).expect("fixture source");
        let value: serde_json::Value = serde_saphyr::from_str(&source).expect("YAML");
        let errors: Vec<_> = validator
            .iter_errors(&value)
            .map(|e| e.to_string())
            .collect();
        assert!(errors.is_empty(), "{name}: {errors:#?}");
        insta::assert_json_snapshot!(format!("fixture_{}", name.replace('-', "_")), config);
    }
}

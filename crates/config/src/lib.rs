//! Typed, ordered project configuration and strict expression evaluation.
//!
//! Loading is synchronous; async callers should use their blocking worker pool.
//! Parsing does not read process environments or execute integration commands.
//!
//! ```
//! let project = devlanes_config::parse_project(
//!     "project: demo\nservices:\n  api: { run: server }\n", None,
//! )?;
//! assert_eq!(project.project, "demo");
//! # Ok::<(), devlanes_config::ConfigError>(())
//! ```

mod error;
mod expression;
mod extensions;
mod loader;
mod model;
mod scalar;
mod schema_docs;
mod service;
mod shape;
mod tags;
mod user;
mod validation;

pub use error::{ConfigError, SourceLocation};
pub use expression::{
    ExpressionContext, evaluate, evaluate_env, evaluate_typed, references, resolve_lane_vars,
};
pub use extensions::{Extensions, OpaqueExtensions};
pub use loader::{
    ConfigSources, check, load_project, load_project_with, parse_project, parse_project_with,
};
pub use model::*;
pub use scalar::{ByteSize, ConfigDuration, Percent, Scalar, Templated};
pub use service::{ServiceConfig, StopConfig};
pub use tags::{TagColor, TagConfig};
pub use user::*;
pub use validation::{
    CheckReport, ConfigWarning, PresetPorts, PresetValidator, validate,
    validate_with_compose_services, validate_with_preset_ports, validate_with_presets,
};

/// Generate the project JSON Schema, including the selected extension schemas.
pub fn schema() -> schemars::Schema {
    schema_with::<OpaqueExtensions>()
}

/// Generate a schema using the binary's registered integration types.
pub fn schema_with<E: Extensions>() -> schemars::Schema {
    let mut schema = schemars::schema_for!(ProjectConfig<E>);
    E::extend_schema(&mut schema);
    schema.insert("$id".into(), "https://devlanes.run/schema/v1.json".into());
    // The same modeline supports partial local overrides; loading still requires a base project.
    schema.remove("required");
    schema_docs::prepare(&mut schema, "devlanes");
    schema
}

/// Return the project/local-override schema for yaml-language-server.
pub fn project_schema() -> serde_json::Value {
    schema().to_value()
}

/// Inject integration-owned schemas without adding runtime dependencies on preset crates.
/// With a registry, unknown service keys and unregistered preset names are schema errors.
/// Without a registry, option mappings are opaque; loading checks preset/key agreement.
pub fn project_schema_with_presets(presets: &[(&str, schemars::Schema)]) -> serde_json::Value {
    let mut schema = project_schema();
    if presets.is_empty() {
        return schema;
    }
    let mut properties = serde_json::Map::new();
    if let Some(defs) = schema
        .get_mut("$defs")
        .and_then(serde_json::Value::as_object_mut)
    {
        for (index, (name, options)) in presets.iter().enumerate() {
            let mut key = format!("InjectedPresetOptions{index}");
            while defs.contains_key(&key) {
                key.push('_');
            }
            let reference = format!("#/$defs/{key}");
            let mut options = options.clone().to_value();
            if let Some(object) = options.as_object_mut() {
                object.remove("$id");
                object.remove("$schema");
            }
            rebase_schema_refs(&mut options, &reference);
            defs.insert(key, options);
            properties.insert(
                (*name).to_owned(),
                serde_json::json!({"type":"object","$ref":reference}),
            );
        }
        // An options key only works with its own preset. The preset may come from a template,
        // so the rule constrains `preset` when present instead of requiring it.
        let agreement: Vec<_> = presets
            .iter()
            .map(|(name, _)| {
                serde_json::json!({
                    "if": {"required": [name]},
                    "then": {"properties": {"preset": {
                        "const": name,
                        "description": format!("`{name}` options require `preset: {name}`."),
                        "examples": [name],
                    }}},
                })
            })
            .collect();
        for (name, definition) in defs {
            if name.starts_with("ServiceConfig") {
                definition["allOf"] = agreement.clone().into();
                definition["additionalProperties"] = false.into();
                if let Some(fields) = definition
                    .get_mut("properties")
                    .and_then(serde_json::Value::as_object_mut)
                {
                    fields.extend(properties.clone());
                    if let Some(preset) = fields.get_mut("preset") {
                        preset["enum"] = serde_json::json!(
                            presets.iter().map(|(name, _)| *name).collect::<Vec<_>>()
                        );
                        preset["examples"] = preset["enum"].clone();
                    }
                }
            }
        }
    }
    schema_docs::prepare_value(&mut schema, "devlanes");
    schema
}

/// Keep each injected schema's local definitions isolated from other preset schemas.
fn rebase_schema_refs(schema: &mut serde_json::Value, base: &str) {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get_mut("$ref")
                && let Some(suffix) = reference.as_str().and_then(|r| r.strip_prefix('#'))
            {
                *reference = format!("{base}{suffix}").into();
            }
            for value in map.values_mut() {
                rebase_schema_refs(value, base);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                rebase_schema_refs(value, base);
            }
        }
        _ => {}
    }
}

/// Generate the separate machine configuration schema.
pub fn user_schema() -> serde_json::Value {
    let mut schema = schemars::schema_for!(UserConfig);
    schema.insert(
        "$id".into(),
        "https://devlanes.run/schema/user-v1.json".into(),
    );
    schema_docs::prepare(&mut schema, "devlanes user config");
    schema.to_value()
}

/// Format a served schema with stable recursive key ordering and a trailing newline.
pub fn schema_artifact(mut schema: serde_json::Value) -> Result<String, serde_json::Error> {
    schema["$comment"] =
        "Generated by bins/devlanes/build.rs from crates/config; do not edit.".into();
    schema.sort_all_objects();
    let mut content = serde_json::to_string_pretty(&schema)?;
    content.push('\n');
    Ok(content)
}

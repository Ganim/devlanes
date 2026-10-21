use schemars::JsonSchema;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

/// Composition-root contract for integration-specific configuration and schema.
pub trait Extensions: Clone + std::fmt::Debug + Default + JsonSchema + 'static {
    type Options: Clone + std::fmt::Debug + Serialize + DeserializeOwned + JsonSchema;

    /// Return the integration keys this registration accepts.
    fn preset_names() -> &'static [&'static str];

    /// Allow runtime registration instead of requiring a compile-time preset list.
    fn accepts_unregistered_presets() -> bool {
        false
    }

    /// Parse an integration's options without accepting unrelated unknown fields.
    fn parse_preset_options(name: &str, value: Value) -> Result<Self::Options, String>;

    /// Merge object options using the same map-merge/scalar-replace contract as services.
    fn merge_preset_options(base: &mut Self::Options, overlay: Self::Options);

    /// Declare named ports before expressions are evaluated; presets without extras return none.
    fn required_ports(_preset: &str) -> &'static [&'static str] {
        &[]
    }

    /// Add option properties and constraints to the generated service schema.
    fn extend_schema(schema: &mut schemars::Schema);
}

/// Opaque preset options validated by integrations registered at the composition root.
/// The historical name remains available to callers using the default generic parameter.
#[derive(Debug, Clone, Default, JsonSchema)]
pub struct BuiltinExtensions;

impl Extensions for BuiltinExtensions {
    type Options = Value;

    /// Preset registration belongs to the binary, not the config crate.
    fn preset_names() -> &'static [&'static str] {
        &[]
    }

    /// Defer the registry lookup until integrations are available.
    fn accepts_unregistered_presets() -> bool {
        true
    }

    /// Preserve option values for integration-owned parsing after expression evaluation.
    fn parse_preset_options(name: &str, value: Value) -> Result<Value, String> {
        if !value.is_object() {
            return Err(format!("preset options `{name}` must be a mapping"));
        }
        Ok(value)
    }

    /// Recursively merge opaque option maps while replacing lists and scalars.
    fn merge_preset_options(base: &mut Value, overlay: Value) {
        match (base, overlay) {
            (Value::Object(base), Value::Object(overlay)) => {
                for (key, value) in overlay {
                    if let Some(base) = base.get_mut(&key) {
                        Self::merge_preset_options(base, value);
                    } else {
                        base.insert(key, value);
                    }
                }
            }
            (base, overlay) => *base = overlay,
        }
    }

    /// Unregistered option keys can only be mappings; loading enforces preset/key agreement.
    fn extend_schema(schema: &mut schemars::Schema) {
        if let Some(defs) = schema
            .as_object_mut()
            .and_then(|o| o.get_mut("$defs"))
            .and_then(Value::as_object_mut)
        {
            for (name, definition) in defs {
                if name.starts_with("ServiceConfig") {
                    definition["additionalProperties"] = serde_json::json!({
                        "type":"object",
                        "description":"Options for the preset selected by this service; validated by the registered integration.",
                        "examples":[{"args":["--open"]}]
                    });
                }
            }
        }
    }
}

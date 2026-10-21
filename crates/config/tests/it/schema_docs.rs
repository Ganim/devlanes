use serde_json::{Value, json};

/// Inspect schema nodes rather than instance values stored in examples and defaults.
fn walk(schema: &Value, path: &str, named: bool, root: &Value, failures: &mut Vec<String>) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if !path.is_empty() && object.contains_key("title") {
        failures.push(format!("{path}: nested title"));
    }
    let is_object = schema.get("type").is_some_and(|t| {
        t == "object"
            || t.as_array()
                .is_some_and(|types| types.contains(&json!("object")))
    });
    if named || is_object || object.contains_key("enum") || object.contains_key("const") {
        if object
            .get("description")
            .and_then(Value::as_str)
            .is_none_or(|text| text.trim().is_empty())
        {
            failures.push(format!("{path}: missing description"));
        }
        if object
            .get("examples")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
        {
            failures.push(format!("{path}: missing examples"));
        }
    }
    if let Some(examples) = object.get("examples").and_then(Value::as_array) {
        // References retain the complete document's definitions, including injected preset schemas.
        let wrapper = json!({"$schema":root["$schema"],"$defs":root["$defs"],"allOf":[schema]});
        match jsonschema::validator_for(&wrapper) {
            Ok(validator) => {
                for example in examples {
                    for error in validator.iter_errors(example) {
                        failures.push(format!("{path}: example {example}: {error}"));
                    }
                }
            }
            Err(error) => failures.push(format!("{path}: invalid subschema: {error}")),
        }
    }
    for keyword in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get(keyword).and_then(Value::as_object) {
            for (key, child) in children {
                walk(
                    child,
                    &format!("{path}/{keyword}/{key}"),
                    true,
                    root,
                    failures,
                );
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get(keyword).and_then(Value::as_array) {
            for (index, child) in children.iter().enumerate() {
                walk(
                    child,
                    &format!("{path}/{keyword}/{index}"),
                    false,
                    root,
                    failures,
                );
            }
        }
    }
    for keyword in [
        "items",
        "additionalProperties",
        "contains",
        "not",
        "if",
        "then",
        "else",
        "propertyNames",
        "unevaluatedProperties",
    ] {
        if let Some(child) = object.get(keyword) {
            walk(child, &format!("{path}/{keyword}"), false, root, failures);
        }
    }
}

/// Every editor-visible field and definition has valid examples, descriptions, and no type title.
#[test]
fn editor_schema_metadata_is_complete() {
    for (title, schema) in [
        ("devlanes", super::registered_schema()),
        ("devlanes", devlanes_config::project_schema()),
        ("devlanes user config", devlanes_config::user_schema()),
    ] {
        assert_eq!(schema["title"], title);
        let mut failures = Vec::new();
        walk(&schema, "", true, &schema, &mut failures);
        assert!(failures.is_empty(), "{title}:\n{}", failures.join("\n"));
    }
}

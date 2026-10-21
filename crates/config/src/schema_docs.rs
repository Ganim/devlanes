use serde_json::{Map, Value};

/// Remove editor headings and expose type annotations at their referencing properties.
pub(crate) fn prepare(schema: &mut schemars::Schema, title: &str) {
    let mut value = Value::Object(schema.ensure_object().clone());
    prepare_value(&mut value, title);
    if let Value::Object(object) = value {
        *schema.ensure_object() = object;
    }
}

/// Keep examples on their Rust declarations; this pass only copies existing annotations.
pub(crate) fn prepare_value(schema: &mut Value, title: &str) {
    let source = schema.clone();
    visit(schema, &source, true);
    if let Some(object) = schema.as_object_mut() {
        object.insert("title".into(), title.into());
    }
}

/// Resolve annotations through references and union wrappers without inventing example values.
fn annotation(node: &Value, root: &Value, key: &str, seen: &mut Vec<String>) -> Option<Value> {
    if let Some(value) = node.get(key) {
        return Some(value.clone());
    }
    if let Some(reference) = node.get("$ref").and_then(Value::as_str)
        && let Some(pointer) = reference.strip_prefix('#')
        && !seen.iter().any(|entry| entry == pointer)
        && let Some(target) = root.pointer(pointer)
    {
        seen.push(pointer.into());
        let value = annotation(target, root, key, seen);
        seen.pop();
        if value.is_some() {
            return value;
        }
    }
    let mut examples = Vec::new();
    for keyword in ["anyOf", "oneOf", "allOf"] {
        if let Some(branches) = node.get(keyword).and_then(Value::as_array) {
            for branch in branches {
                if let Some(value) = annotation(branch, root, key, seen) {
                    if key != "examples" {
                        return Some(value);
                    }
                    if let Value::Array(values) = value {
                        for value in values {
                            if !examples.contains(&value) {
                                examples.push(value);
                            }
                        }
                    }
                }
            }
        }
    }
    (!examples.is_empty()).then_some(Value::Array(examples))
}

/// Visit schema keywords only; examples and defaults contain instance data, not schemas.
/// Union branches (`inject` false) keep only their own annotations: copying the referenced
/// definition's generic text there would repeat it beside the field's own description.
fn visit(node: &mut Value, root: &Value, inject: bool) {
    let metadata: Vec<_> = ["description", "examples"]
        .into_iter()
        .filter(|key| inject && node.get(key).is_none())
        .filter_map(|key| annotation(node, root, key, &mut Vec::new()).map(|value| (key, value)))
        .collect();
    let Some(object) = node.as_object_mut() else {
        return;
    };
    object.remove("title");
    for (key, value) in metadata {
        object.insert(key.into(), value);
    }
    let examples = object
        .get("examples")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let description = object.get("description").cloned();
    let property_names: Vec<_> = object
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| properties.keys().cloned().collect())
        .unwrap_or_default();
    for keyword in [
        "properties",
        "$defs",
        "definitions",
        "patternProperties",
        "dependentSchemas",
    ] {
        if let Some(children) = object.get_mut(keyword).and_then(Value::as_object_mut) {
            for child in children.values_mut() {
                visit(child, root, true);
            }
        }
    }
    for keyword in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(children) = object.get_mut(keyword).and_then(Value::as_array_mut) {
            for child in children {
                visit(child, root, keyword == "prefixItems");
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
        if let Some(child) = object.get_mut(keyword) {
            visit(child, root, true);
            // Map/array members can reuse instances already written on their containing field.
            if matches!(keyword, "items" | "additionalProperties") {
                let samples = examples
                    .iter()
                    .flat_map(|example| match (keyword, example) {
                        ("items", Value::Array(values)) => values.clone(),
                        ("additionalProperties", Value::Object(values)) => values
                            .iter()
                            .filter(|(key, _)| !property_names.contains(key))
                            .map(|(_, value)| value.clone())
                            .collect(),
                        _ => Vec::new(),
                    })
                    .collect::<Vec<_>>();
                if let Some(child) = child.as_object_mut() {
                    inherit(
                        child,
                        "examples",
                        (!samples.is_empty()).then_some(Value::Array(samples)),
                    );
                    inherit(child, "description", description.clone());
                }
            }
        }
    }
}

/// Fill missing annotations without overwriting a field's own documentation.
fn inherit(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        object.entry(key).or_insert(value);
    }
}

use serde_json::{Map, Value};

/// Remove editor headings and expose type annotations at their referencing properties.
pub(crate) fn prepare(schema: &mut schemars::Schema) {
    let mut value = Value::Object(schema.ensure_object().clone());
    prepare_value(&mut value);
    if let Value::Object(object) = value {
        *schema.ensure_object() = object;
    }
}

/// Keep examples on their Rust declarations; this pass only copies existing annotations.
/// No title is kept, the root's included: yaml-language-server prints the nearest title as
/// a heading on every field hover.
pub(crate) fn prepare_value(schema: &mut Value) {
    let source = schema.clone();
    visit(schema, &source, true);
    for_hover(schema);
    as_markdown(schema);
}

/// Joins wrapped doc-comment lines and repeats each text as Markdown. Editors escape a plain
/// `description` (backticks show literally) and turn each line break into a paragraph;
/// `markdownDescription` is rendered as written and preferred when present.
fn as_markdown(node: &mut Value) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    for (plain, markdown) in [
        ("description", "markdownDescription"),
        ("enumDescriptions", "markdownEnumDescriptions"),
    ] {
        let text = match object.get_mut(plain) {
            Some(Value::String(text)) => {
                *text = unwrap_lines(text);
                Value::String(text.clone())
            }
            Some(Value::Array(texts)) => {
                for text in texts.iter_mut() {
                    if let Value::String(line) = text {
                        *line = unwrap_lines(line);
                    }
                }
                Value::Array(texts.clone())
            }
            _ => continue,
        };
        object.insert(markdown.into(), text);
    }
    // Examples, defaults and constants are instance data, never schemas.
    for (key, child) in object.iter_mut() {
        if matches!(
            key.as_str(),
            "examples" | "default" | "const" | "enum" | "enumDescriptions"
        ) {
            continue;
        }
        match child {
            Value::Object(_) => as_markdown(child),
            Value::Array(items) => items.iter_mut().for_each(as_markdown),
            _ => {}
        }
    }
}

/// Joins the lines of each paragraph; blank lines still separate paragraphs.
fn unwrap_lines(text: &str) -> String {
    text.split("\n\n")
        .map(|paragraph| paragraph.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Shapes the schema for yaml-language-server hovers, which show the first description among
/// the schemas a value matches, innermost union branch first, and every matched schema's
/// examples. Without this, a branch such as the literal side of a `${{ }}` expression wins
/// over the field's own text and examples repeat once per level.
fn for_hover(node: &mut Value) {
    let Some(object) = node.as_object_mut() else {
        return;
    };
    enumerate_constants(object);
    for keyword in ["anyOf", "oneOf"] {
        for branch in object
            .get_mut(keyword)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let Some(branch) = branch.as_object_mut() else {
                continue;
            };
            if branch.contains_key("$ref") {
                // A referenced definition's own text would be merged into the branch;
                // an empty description is skipped by the hover, so the field's text wins.
                // Only annotations go here: a constraint beside `$ref` makes the editor
                // wrap it in `allOf`, which brings the definition's text back.
                branch.insert("description".into(), "".into());
                branch.insert("examples".into(), Value::Array(Vec::new()));
                continue;
            }
            branch.remove("description");
            branch.remove("examples");
        }
    }
    // Examples, defaults and constants are instance data, never schemas.
    let schemas = object.iter_mut().filter(|(key, _)| {
        !matches!(
            key.as_str(),
            "examples" | "default" | "const" | "enum" | "enumDescriptions"
        )
    });
    for (_, child) in schemas {
        match child {
            Value::Object(_) => for_hover(child),
            Value::Array(items) => items.iter_mut().for_each(for_hover),
            _ => {}
        }
    }
}

/// Turns a union of documented string constants (a unit enum with variant docs) into an
/// `enum` with `enumDescriptions`, which editors list as allowed values with their meaning.
fn enumerate_constants(object: &mut Map<String, Value>) {
    let Some(branches) = object.get("oneOf").and_then(Value::as_array) else {
        return;
    };
    let mut values = Vec::new();
    let mut descriptions = Vec::new();
    for branch in branches {
        let Some(Value::String(value)) = branch.get("const") else {
            return;
        };
        values.push(Value::String(value.clone()));
        descriptions.push(branch.get("description").cloned().unwrap_or_default());
    }
    object.remove("oneOf");
    object.insert("type".into(), "string".into());
    object.insert("enum".into(), Value::Array(values));
    object.insert("enumDescriptions".into(), Value::Array(descriptions));
}

/// Resolve annotations through references and union wrappers without inventing example values.
/// The empty placeholders `for_hover` puts on `$ref` branches are skipped, so a second pass
/// does not copy them onto a field that has no text of its own.
fn annotation(node: &Value, root: &Value, key: &str, seen: &mut Vec<String>) -> Option<Value> {
    if let Some(value) = node.get(key).filter(|value| !is_placeholder(value)) {
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

/// An empty description or examples list, as `for_hover` leaves on `$ref` branches.
fn is_placeholder(value: &Value) -> bool {
    match value {
        Value::String(text) => text.is_empty(),
        Value::Array(values) => values.is_empty(),
        _ => false,
    }
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
        let Some(child) = object.get_mut(keyword) else {
            continue;
        };
        if !matches!(keyword, "items" | "additionalProperties") {
            visit(child, root, true);
            continue;
        }
        // Map/array members can reuse instances already written on their containing field.
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
        let samples = (!samples.is_empty()).then_some(Value::Array(samples));
        // A member that is only a `${{ }}` wrapper (`env.PORT`, `vars.db_name`) is described
        // by its field; the wrapper's generic text would say nothing about it.
        let wraps_expression = child
            .get("$ref")
            .and_then(Value::as_str)
            .is_some_and(|reference| reference.starts_with("#/$defs/Templated_"));
        if wraps_expression && let Some(child) = child.as_object_mut() {
            inherit(child, "examples", samples.clone());
            inherit(child, "description", description.clone());
        }
        visit(child, root, true);
        if let Some(child) = child.as_object_mut() {
            inherit(child, "examples", samples);
            inherit(child, "description", description.clone());
        }
    }
}

/// Fill missing annotations without overwriting a field's own documentation.
fn inherit(object: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(value) = value {
        object.entry(key).or_insert(value);
    }
}

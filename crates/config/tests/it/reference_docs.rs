//! `docs/reference/config.md`, generated from the committed `schema.json` and
//! `user-schema.json`. The test fails when the file is stale; `DEVLANES_BLESS=1`
//! rewrites it.

use std::path::Path;

use serde_json::Value;

/// Map keys the user chooses appear as this placeholder in field paths.
const KEY: &str = "<name>";

/// One documented field.
struct Row {
    path: String,
    kind: String,
    description: String,
    example: String,
}

#[test]
fn config_reference_is_current() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR"));
    let project = read_schema(&directory.join("schema.json"));
    let user = read_schema(&directory.join("user-schema.json"));
    let generated = render(&project, &user);
    check_or_bless(
        &directory.join("../../docs/reference/config.md"),
        &generated,
        "cargo nextest run -p devlanes-config -E 'test(config_reference_is_current)'",
    );
}

/// Parses a committed schema artifact.
fn read_schema(path: &Path) -> Value {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// Compares `generated` with the committed file, or writes it under `DEVLANES_BLESS=1`.
fn check_or_bless(path: &Path, generated: &str, command: &str) {
    if std::env::var("DEVLANES_BLESS").as_deref() == Ok("1") {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create docs/reference");
        }
        std::fs::write(path, generated).expect("write the reference");
        return;
    }
    let committed = std::fs::read_to_string(path).unwrap_or_default();
    assert!(
        committed == generated,
        "{} is stale; regenerate it with DEVLANES_BLESS=1 {command}",
        path.display()
    );
}

/// The whole page: project config first, then the per-user config.
fn render(project: &Value, user: &Value) -> String {
    let mut page = String::from(
        "# Config reference\n\n\
         <!-- Generated from crates/config/schema.json and crates/config/user-schema.json by \
         crates/config/tests/it/reference_docs.rs. Edit the doc comments in crates/config, \
         then run DEVLANES_BLESS=1 cargo nextest run -p devlanes-config. -->\n\n\
         Every field of `devlanes.yaml` and of the per-user `config.yaml`, generated from the \
         JSON Schemas devlanes publishes. In a path, `<name>` stands for a key you choose \
         (a service, task or profile name) and `[]` for an item of a list. A type marked \
         *templated* also accepts a `${{ ... }}` expression, evaluated for each lane. A \
         *duration* is a number with `ms`, `s`, `m`, `h` or `d`, and a *scalar* is a \
         string, number or boolean. \
         [examples/devlanes.yaml](../examples/devlanes.yaml) shows the fields in context.\n\n",
    );
    page.push_str("## Project config\n\n");
    page.push_str(&format!(
        "`devlanes.yaml` at the repository root (or a subdirectory, in a monorepo), with \
         optional overrides in `devlanes.local.yaml` next to it. Schema: `{}`.\n\n",
        text(&project["$id"])
    ));
    sections(&mut page, project);
    page.push_str("## User config\n\n");
    page.push_str(&format!(
        "`config.yaml` in the devlanes home (`~/.devlanes`, or `$DEVLANES_HOME`), for \
         settings of this machine. Schema: `{}`.\n\n",
        text(&user["$id"])
    ));
    sections(&mut page, user);
    page
}

/// One `###` section with a table per top-level key of `schema`.
fn sections(page: &mut String, schema: &Value) {
    let Some(properties) = schema["properties"].as_object() else {
        return;
    };
    for (key, property) in properties {
        let mut rows = vec![row(key, property, schema)];
        let mut stack = Vec::new();
        children(property, key, schema, &mut rows, &mut stack);
        let mut seen = std::collections::BTreeSet::new();
        rows.retain(|row| seen.insert(row.path.clone()));
        page.push_str(&format!("### {key}\n\n"));
        page.push_str("| Path | Type | Description | Example |\n| --- | --- | --- | --- |\n");
        for row in rows {
            page.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                row.path,
                cell(&row.kind),
                cell(&row.description),
                row.example
            ));
        }
        page.push('\n');
    }
}

/// The documented row for the field at `path`.
fn row(path: &str, node: &Value, root: &Value) -> Row {
    Row {
        path: path.to_owned(),
        kind: kind(node, root, &mut Vec::new()),
        description: annotation(node, root, "description")
            .as_ref()
            .map(text)
            .unwrap_or_default()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        example: annotation(node, root, "examples")
            .and_then(|examples| examples.as_array().and_then(|e| e.first()).cloned())
            .map(|example| code(&example))
            .unwrap_or_default(),
    }
}

/// Rows for the fields nested under `node`: object properties, map values
/// (`<name>`), list items (`[]`) and every branch of a union.
fn children(node: &Value, path: &str, root: &Value, rows: &mut Vec<Row>, stack: &mut Vec<String>) {
    if let Some(reference) = node["$ref"].as_str() {
        if stack.iter().any(|seen| seen == reference) {
            return;
        }
        stack.push(reference.to_owned());
        if let Some(target) = resolve(reference, root) {
            children(target, path, root, rows, stack);
        }
        stack.pop();
        return;
    }
    if let Some(properties) = node["properties"].as_object() {
        for (key, property) in properties {
            let child = format!("{path}.{key}");
            rows.push(row(&child, property, root));
            children(property, &child, root, rows, stack);
        }
    }
    if node["additionalProperties"].is_object() {
        children(
            &node["additionalProperties"],
            &format!("{path}.{KEY}"),
            root,
            rows,
            stack,
        );
    }
    if node["items"].is_object() {
        children(&node["items"], &format!("{path}[]"), root, rows, stack);
    }
    for keyword in ["anyOf", "oneOf"] {
        for branch in node[keyword].as_array().into_iter().flatten() {
            children(branch, path, root, rows, stack);
        }
    }
}

/// The node a local `#/$defs/...` reference points at.
fn resolve<'a>(reference: &str, root: &'a Value) -> Option<&'a Value> {
    root.pointer(reference.strip_prefix('#')?)
}

/// `key` on the node or, when absent, on the definition it references.
fn annotation(node: &Value, root: &Value, key: &str) -> Option<Value> {
    if let Some(value) = node.get(key) {
        return Some(value.clone());
    }
    node["$ref"]
        .as_str()
        .and_then(|reference| resolve(reference, root))
        .and_then(|target| target.get(key).cloned())
}

/// A readable type: `string`, `integer`, `list of string`, `map of object`,
/// `` `auto` or `never` ``, with `(templated)` where an expression is accepted.
fn kind(node: &Value, root: &Value, stack: &mut Vec<String>) -> String {
    if let Some(reference) = node["$ref"].as_str() {
        let name = reference.rsplit('/').next().unwrap_or(reference);
        match name {
            "ConfigDuration" => return "duration".into(),
            "Scalar" => return "scalar".into(),
            _ => {}
        }
        if stack.iter().any(|seen| seen == reference) {
            return "object".into();
        }
        let Some(target) = resolve(reference, root) else {
            return "any".into();
        };
        stack.push(reference.to_owned());
        let found = if name.starts_with("Templated_") {
            // The first branch is the literal; the second is the expression form.
            format!("{} (templated)", kind(&target["anyOf"][0], root, stack))
        } else {
            kind(target, root, stack)
        };
        stack.pop();
        return found;
    }
    if let Some(constant) = node.get("const") {
        return code(constant);
    }
    for keyword in ["anyOf", "oneOf"] {
        if let Some(branches) = node[keyword].as_array() {
            let mut kinds: Vec<String> = Vec::new();
            for branch in branches {
                if branch["type"] == "null" {
                    continue;
                }
                let found = kind(branch, root, stack);
                if !kinds.contains(&found) {
                    kinds.push(found);
                }
            }
            return kinds.join(" or ");
        }
    }
    if let Some(values) = node["enum"].as_array() {
        return values.iter().map(code).collect::<Vec<_>>().join(" or ");
    }
    let types: Vec<&str> = match &node["type"] {
        Value::String(single) => vec![single.as_str()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    let names: Vec<String> = types
        .into_iter()
        .filter(|name| *name != "null")
        .map(|name| match name {
            "array" => format!("list of {}", kind_or_any(&node["items"], root, stack)),
            "object" if node["additionalProperties"].is_object() => format!(
                "map of {}",
                kind(&node["additionalProperties"], root, stack)
            ),
            other => other.to_owned(),
        })
        .collect();
    if names.is_empty() {
        "any".into()
    } else {
        names.join(" or ")
    }
}

/// [`kind`] of an optional schema.
fn kind_or_any(node: &Value, root: &Value, stack: &mut Vec<String>) -> String {
    if node.is_object() {
        kind(node, root, stack)
    } else {
        "any".into()
    }
}

/// A string as is; anything else as compact JSON.
fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// An inline code span safe inside a table cell; an empty string shows as `""`.
fn code(value: &Value) -> String {
    let text = match value {
        Value::String(empty) if empty.is_empty() => "\"\"".to_owned(),
        _ => text(value).replace('\n', " ").replace('|', "\\|"),
    };
    if text.contains('`') {
        format!("`` {text} ``")
    } else {
        format!("`{text}`")
    }
}

/// Table cell text: one line, with pipes escaped.
fn cell(text: &str) -> String {
    text.replace('\n', " ").replace('|', "\\|")
}

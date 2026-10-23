use crate::ConfigError;
use indexmap::IndexMap;
use minijinja::{Environment, UndefinedBehavior, syntax::SyntaxConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;

/// Runtime-owned values. No process environment is read implicitly.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExpressionContext {
    pub project: Value,
    pub lane: Value,
    pub lanes: Value,
    #[serde(rename = "self")]
    pub service: Value,
    pub services: Value,
    pub infra: Value,
    pub env: Value,
    pub projects: Value,
    pub replica: Value,
}

impl Default for ExpressionContext {
    /// Provide empty namespaces so missing leaf values remain strictly undefined.
    fn default() -> Self {
        Self {
            project: serde_json::json!({}),
            lane: serde_json::json!({}),
            lanes: serde_json::json!({}),
            service: serde_json::json!({}),
            services: serde_json::json!({}),
            infra: serde_json::json!({}),
            env: serde_json::json!({}),
            projects: serde_json::json!({}),
            replica: serde_json::json!({}),
        }
    }
}

/// Construct a strict expression-only template environment.
fn environment() -> Result<Environment<'static>, ConfigError> {
    let mut env = Environment::new();
    env.set_undefined_behavior(UndefinedBehavior::Strict);
    let syntax = SyntaxConfig::builder()
        .variable_delimiters("${{", "}}")
        .block_delimiters("\u{1}block", "endblock\u{1}")
        .comment_delimiters("\u{1}comment", "endcomment\u{1}")
        .build()
        .map_err(|e| ConfigError::plain(e.to_string()))?;
    env.set_syntax(syntax);
    Ok(env)
}

/// A MiniJinja error without its `(in <string>:1)` suffix, which names no real file.
fn describe(error: &minijinja::Error) -> String {
    match error.detail() {
        Some(detail) => format!("{}: {detail}", error.kind()),
        None => error.kind().to_string(),
    }
}

/// Extract expression bodies without confusing braces inside quoted strings.
fn parts(text: &str) -> Result<Vec<(usize, usize, &str)>, ConfigError> {
    let mut parts = Vec::new();
    let mut cursor = 0;
    while let Some(start) = text[cursor..].find("${{").map(|i| i + cursor) {
        let mut quote = None;
        let mut escaped = false;
        let mut depth = 0_usize;
        let mut end = None;
        let body = start + 3;
        let mut iter = text[body..].char_indices().peekable();
        while let Some((offset, c)) = iter.next() {
            if escaped {
                escaped = false;
                continue;
            }
            if let Some(q) = quote {
                if c == '\\' {
                    escaped = true;
                } else if c == q {
                    quote = None;
                }
                continue;
            }
            match c {
                '\'' | '"' => quote = Some(c),
                '(' | '[' | '{' => depth += 1,
                '}' if depth == 0 && iter.peek().is_some_and(|(_, c)| *c == '}') => {
                    end = Some(body + offset);
                    break;
                }
                ')' | ']' | '}' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        let end = end.ok_or_else(|| ConfigError::plain("unclosed `${{` expression"))?;
        parts.push((start, end + 2, &text[body..end]));
        cursor = end + 2;
    }
    Ok(parts)
}

/// Whether any mapping key in the tree contains a dash and so needs an underscore alias.
fn has_dashed_keys(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key.contains('-') || has_dashed_keys(value)),
        Value::Array(values) => values.iter().any(has_dashed_keys),
        _ => false,
    }
}

/// Add underscore aliases recursively. An alias is added only when exactly one dashed key
/// maps to it and no key already has that name, so `A-B` beside `A_B` keeps both literal.
fn aliases(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for value in map.values_mut() {
                aliases(value);
            }
            let mut extra: IndexMap<String, Option<Value>> = IndexMap::new();
            for (key, value) in map.iter().filter(|(key, _)| key.contains('-')) {
                extra
                    .entry(key.replace('-', "_"))
                    .and_modify(|slot| *slot = None)
                    .or_insert_with(|| Some(value.clone()));
            }
            for (key, value) in extra {
                if let Some(value) = value
                    && !map.contains_key(&key)
                {
                    map.insert(key, value);
                }
            }
        }
        Value::Array(values) => {
            for value in values {
                aliases(value);
            }
        }
        _ => {}
    }
}

/// Evaluate a scalar, preserving a single expression's native JSON type.
/// Mixed text always produces a string. Missing values fail unless filtered with `default`.
/// Write `${{ '${{' }}` for a literal `${{`.
pub fn evaluate(text: &str, context: &ExpressionContext) -> Result<Value, ConfigError> {
    let chunks = parts(text)?;
    if chunks.is_empty() {
        return Ok(Value::String(text.into()));
    }
    let projects: Vec<_> = references(text)?
        .into_iter()
        .filter(|r| r.starts_with("projects."))
        .collect();
    let namespaces = [
        &context.project,
        &context.lane,
        &context.lanes,
        &context.service,
        &context.services,
        &context.infra,
        &context.env,
        &context.projects,
        &context.replica,
    ];
    // Copy the context only when aliases or project placeholders must be added to it.
    let owned = if projects.is_empty() && !namespaces.into_iter().any(has_dashed_keys) {
        None
    } else {
        let mut value =
            serde_json::to_value(context).map_err(|e| ConfigError::plain(e.to_string()))?;
        aliases(&mut value);
        for reference in projects {
            let components: Vec<_> = reference.split('.').collect();
            let mut node = &mut value;
            for component in components.iter().take(components.len().saturating_sub(1)) {
                if let Some(map) = node.as_object_mut() {
                    node = map
                        .entry(*component)
                        .or_insert_with(|| serde_json::json!({}));
                } else {
                    break;
                }
            }
        }
        Some(value)
    };
    let context = match &owned {
        Some(value) => minijinja::Value::from(minijinja::value::Serde(value)),
        None => minijinja::Value::from(minijinja::value::Serde(context)),
    };
    let env = environment()?;
    if chunks.len() == 1 && chunks[0].0 == 0 && chunks[0].1 == text.len() {
        let expression = env
            .compile_expression(chunks[0].2)
            .map_err(|e| ConfigError::plain(format!("invalid expression: {}", describe(&e))))?;
        let value = expression
            .eval(&context)
            .map_err(|e| ConfigError::plain(format!("expression failed: {}", describe(&e))))?;
        if value.is_undefined() {
            return Err(ConfigError::plain(
                "expression resolved to an undefined value",
            ));
        }
        return serde_json::to_value(value).map_err(|e| ConfigError::plain(e.to_string()));
    }
    env.render_str(text, context)
        .map(Value::String)
        .map_err(|e| ConfigError::plain(format!("expression failed: {}", describe(&e))))
}

/// Offsets added to `lane.port` in literal sums such as `lane.port + 5`, `lane['port'] + 5`
/// or `5 + lane.port`, which must stay inside the lane's port block.
pub(crate) fn lane_port_offsets(text: &str) -> Result<Vec<u64>, ConfigError> {
    let mut offsets = Vec::new();
    for (_, _, body) in parts(text)? {
        let (normalized, _) = normalize_static_subscripts(body);
        let compact: String = normalized.chars().filter(|c| !c.is_whitespace()).collect();
        let digits = |s: &str| -> Option<u64> {
            let number: String = s.chars().take_while(char::is_ascii_digit).collect();
            number.parse().ok()
        };
        for (index, _) in compact.match_indices("lane.port") {
            let after = &compact[index + "lane.port".len()..];
            let identifier = |c: char| c.is_alphanumeric() || c == '_' || c == '.';
            // `lane.ports`, `lane.port_name` and `other.lane.port` are other variables.
            if after.starts_with(identifier) || compact[..index].ends_with(identifier) {
                continue;
            }
            if let Some(offset) = after.strip_prefix('+').and_then(digits) {
                offsets.push(offset);
            }
            let before = &compact[..index];
            if let Some(head) = before.strip_suffix('+') {
                let start = head
                    .rfind(|c: char| !c.is_ascii_digit())
                    .map_or(0, |i| i + 1);
                if let Ok(offset) = head[start..].parse() {
                    offsets.push(offset);
                }
            }
        }
    }
    Ok(offsets)
}

/// Return static dotted references using MiniJinja's own expression analysis.
pub fn references(text: &str) -> Result<BTreeSet<String>, ConfigError> {
    let env = environment()?;
    let mut references = BTreeSet::new();
    for (_, _, body) in parts(text)? {
        let expression = env
            .compile_expression(body)
            .map_err(|e| ConfigError::plain(format!("invalid expression: {}", describe(&e))))?;
        references.extend(expression.undeclared_variables(true));
        let (normalized, original_keys) = normalize_static_subscripts(body);
        if normalized != body {
            let expression = env
                .compile_expression(&normalized)
                .map_err(|e| ConfigError::plain(format!("invalid expression: {}", describe(&e))))?;
            for reference in expression.undeclared_variables(true) {
                let original = reference
                    .split('.')
                    .map(|component| {
                        original_keys
                            .iter()
                            .find(|(alias, _)| alias == component)
                            .map_or(component, |(_, key)| key.as_str())
                    })
                    .collect::<Vec<_>>()
                    .join(".");
                references.insert(reference);
                references.insert(original);
            }
        }
    }
    Ok(references)
}

/// Evaluate every templated scalar in a typed service, task, host, or integration value.
/// Call separately per service/replica after allocating ports and assembling its context.
pub fn evaluate_typed<T: Serialize + serde::de::DeserializeOwned>(
    value: &T,
    context: &ExpressionContext,
) -> Result<T, ConfigError> {
    let mut value = serde_json::to_value(value)
        .map_err(|_| ConfigError::plain("cannot serialize typed configuration"))?;
    evaluate_tree(&mut value, context, "")?;
    serde_path_to_error::deserialize(value).map_err(|error| {
        ConfigError::plain(format!(
            "{}: expression result has the wrong type: {}",
            error.path(),
            crate::scalar::plain_type_names(&error.inner().to_string())
        ))
    })
}

/// Check fully constant expressions at load time; runtime-dependent values remain templated.
pub(crate) fn validate_constants<T: Serialize + serde::de::DeserializeOwned>(
    value: &T,
) -> Result<(), ConfigError> {
    /// Evaluate only expressions whose reference set is empty.
    fn visit(value: &mut Value) -> Result<(), ConfigError> {
        match value {
            Value::String(text) if text.contains("${{") && references(text)?.is_empty() => {
                *value = evaluate(text, &ExpressionContext::default())?
            }
            Value::Object(map) => {
                for value in map.values_mut() {
                    visit(value)?;
                }
            }
            Value::Array(values) => {
                for value in values {
                    visit(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut encoded = serde_json::to_value(value)
        .map_err(|_| ConfigError::plain("cannot serialize typed configuration"))?;
    visit(&mut encoded)?;
    let _: T = serde_path_to_error::deserialize(encoded).map_err(|error| {
        ConfigError::plain(format!(
            "{}: constant expression has the wrong type: {}",
            error.path(),
            crate::scalar::plain_type_names(&error.inner().to_string())
        ))
    })?;
    Ok(())
}

/// Traverse evaluated data without including resolved secrets in error messages.
fn evaluate_tree(
    value: &mut Value,
    context: &ExpressionContext,
    path: &str,
) -> Result<(), ConfigError> {
    match value {
        Value::String(text) if text.contains("${{") => {
            *value = evaluate(text, context)
                .map_err(|e| ConfigError::plain(format!("{path}: {}", e.message)))?;
        }
        Value::Object(map) => {
            for (key, value) in map {
                evaluate_tree(value, context, &format!("{path}.{key}"))?;
            }
        }
        Value::Array(values) => {
            for (index, value) in values.iter_mut().enumerate() {
                evaluate_tree(value, context, &format!("{path}.{index}"))?;
            }
        }
        _ => {}
    }
    Ok(())
}

/// Evaluate each override against the caller's outer environment, never sibling overrides.
pub fn evaluate_env(
    values: &crate::Env,
    context: &ExpressionContext,
) -> Result<indexmap::IndexMap<String, crate::Scalar>, ConfigError> {
    values
        .iter()
        .map(|(name, value)| value.evaluate(context).map(|value| (name.clone(), value)))
        .collect()
}

/// Resolve lane vars or the current matrix row using lane.index supplied by the caller.
pub fn resolve_lane_vars(
    config: &crate::LanesConfig,
    context: &ExpressionContext,
) -> Result<indexmap::IndexMap<String, crate::Scalar>, ConfigError> {
    let values = if let Some(matrix) = &config.matrix {
        let index = context
            .lane
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or_else(|| {
                ConfigError::plain("lane.index must be a nonnegative integer for matrix resolution")
            })?;
        matrix
            .get(index)
            .ok_or_else(|| ConfigError::plain("lane.index exceeds the configured matrix"))?
    } else {
        &config.vars
    };
    evaluate_lane_variables(values, context)
}

/// Resolve configured keys before consumers, preserving declaration order in the result.
fn evaluate_lane_variables(
    values: &crate::Env,
    context: &ExpressionContext,
) -> Result<indexmap::IndexMap<String, crate::Scalar>, ConfigError> {
    let mut pending: indexmap::IndexMap<String, BTreeSet<String>> = indexmap::IndexMap::new();
    for (name, value) in values {
        let refs = match value {
            crate::Templated::Expression(text) => references(text)?,
            _ => BTreeSet::new(),
        };
        let dependencies = refs
            .iter()
            .filter_map(|r| r.strip_prefix("lane."))
            .filter_map(|r| {
                values
                    .keys()
                    .find(|name| name.as_str() == r || name.replace('-', "_") == r)
                    .cloned()
            })
            .collect();
        pending.insert(name.clone(), dependencies);
    }
    let graph = pending
        .iter()
        .map(|(name, refs)| (name.clone(), refs.iter().cloned().collect()))
        .collect();
    check_cycles(&graph)?;
    let mut context = context.clone();
    let mut done = indexmap::IndexMap::new();
    while !pending.is_empty() {
        let next = pending
            .iter()
            .find(|(_, deps)| deps.iter().all(|name| done.contains_key(name)))
            .map(|(name, _)| name.clone())
            .ok_or_else(|| ConfigError::plain("unresolvable variable dependency cycle"))?;
        let value = values[&next].evaluate(&context)?;
        let map = context
            .lane
            .as_object_mut()
            .ok_or_else(|| ConfigError::plain("lane context must be an object"))?;
        map.insert(
            next.clone(),
            serde_json::to_value(&value)
                .map_err(|_| ConfigError::plain("cannot serialize resolved scalar"))?,
        );
        pending.shift_remove(&next);
        done.insert(next, value);
    }
    Ok(values
        .keys()
        .filter_map(|name| done.shift_remove(name).map(|value| (name.clone(), value)))
        .collect())
}

/// Normalize literal bracket lookups for MiniJinja's attribute-only dependency analysis.
fn normalize_static_subscripts(body: &str) -> (String, Vec<(String, String)>) {
    let mut output = String::new();
    let mut original_keys = Vec::new();
    let mut chars = body.char_indices().peekable();
    let mut quote = None;
    let mut escaped = false;
    while let Some((index, c)) = chars.next() {
        if escaped {
            output.push(c);
            escaped = false;
            continue;
        }
        if let Some(q) = quote {
            output.push(c);
            if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '['
            && output
                .trim_end()
                .chars()
                .last()
                .is_some_and(|c| c.is_alphanumeric() || matches!(c, '_' | ']' | ')'))
        {
            let rest = body[index + 1..].trim_start();
            if let Some(q @ ('\'' | '"')) = rest.chars().next()
                && let Some(end) = rest[1..].find(q)
            {
                let key = &rest[1..end + 1];
                let after = rest[end + 2..].trim_start();
                if after.starts_with(']')
                    && !key.is_empty()
                    && key
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                    && !key.starts_with(|c: char| c.is_ascii_digit())
                {
                    let consumed = body.len() - after.len() + 1;
                    let alias = key.replace('-', "_");
                    original_keys.push((alias.clone(), key.to_owned()));
                    output.push('.');
                    output.push_str(&alias);
                    while chars.peek().is_some_and(|(i, _)| *i < consumed) {
                        chars.next();
                    }
                    continue;
                }
            }
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
        }
        output.push(c);
    }
    (output, original_keys)
}

/// Resolve a dependency graph with a readable cycle chain.
pub(crate) fn check_cycles(
    graph: &indexmap::IndexMap<String, Vec<String>>,
) -> Result<(), ConfigError> {
    /// Walk only configured nodes; external runtime references do not form local edges.
    fn visit(
        node: &str,
        graph: &indexmap::IndexMap<String, Vec<String>>,
        active: &mut Vec<String>,
        done: &mut BTreeSet<String>,
    ) -> Result<(), ConfigError> {
        if done.contains(node) {
            return Ok(());
        }
        if let Some(start) = active.iter().position(|n| n == node) {
            let mut cycle = active[start..].to_vec();
            cycle.push(node.into());
            return Err(ConfigError::plain(format!(
                "expression/reference cycle: {}",
                cycle.join(" -> ")
            )));
        }
        active.push(node.into());
        if let Some(edges) = graph.get(node) {
            for edge in edges {
                if graph.contains_key(edge) {
                    visit(edge, graph, active, done)?;
                }
            }
        }
        active.pop();
        done.insert(node.into());
        Ok(())
    }
    let mut done = BTreeSet::new();
    for node in graph.keys() {
        visit(node, graph, &mut Vec::new(), &mut done)?;
    }
    Ok(())
}

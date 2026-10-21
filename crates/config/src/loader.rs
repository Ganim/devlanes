use crate::{
    BuiltinExtensions, ConfigError, Env, Extensions, FnoxConfig, InfraCommand, InfraConfig,
    LanesConfig, ProfileConfig, ProjectConfig, TaskConfig, service::PartialService,
};
use indexmap::IndexMap;
use serde::{Deserialize, de::DeserializeOwned};
use serde_saphyr::Spanned;
use std::{collections::HashSet, path::Path};

/// Parsed before defaults, so an omitted local field never erases a base field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, bound = "E: Extensions")]
struct PartialProject<E: Extensions> {
    project: Option<Spanned<String>>,
    #[serde(default)]
    tags: IndexMap<String, crate::TagConfig>,
    lanes: Option<Spanned<LanesConfig>>,
    infra: Option<Spanned<InfraConfig>>,
    fnox: Option<Spanned<FnoxConfig>>,
    global_env: Option<Spanned<Env>>,
    #[serde(default)]
    templates: IndexMap<String, PartialService<E>>,
    #[serde(default)]
    services: IndexMap<String, PartialService<E>>,
    #[serde(default)]
    tasks: IndexMap<String, TaskConfig>,
    #[serde(default)]
    profiles: IndexMap<String, ProfileConfig>,
}

/// Apply scalar/list replacement without introducing absent defaults.
macro_rules! replace {
    ($base:ident, $overlay:ident, $($field:ident),* $(,)?) => {
        $(if $overlay.$field.is_some() { $base.$field = $overlay.$field; })*
    };
}

/// Merge map entries in place to preserve the original declaration order.
fn merge_map<T>(
    base: &mut IndexMap<String, T>,
    overlay: IndexMap<String, T>,
    merge: impl Fn(&mut T, T),
) {
    for (key, value) in overlay {
        if let Some(base) = base.get_mut(&key) {
            merge(base, value);
        } else {
            base.insert(key, value);
        }
    }
}

/// Combine typed source-bearing root fields without a YAML-tree round trip.
fn merge_spanned<T>(
    base: &mut Option<Spanned<T>>,
    overlay: Option<Spanned<T>>,
    merge: impl FnOnce(&mut T, T),
) {
    if let Some(overlay) = overlay {
        if let Some(base) = base {
            merge(&mut base.value, overlay.value);
        } else {
            *base = Some(overlay);
        }
    }
}

/// Merge nested lane maps; matrix lists replace as a whole.
fn merge_lanes(base: &mut LanesConfig, overlay: LanesConfig) {
    replace!(
        base,
        overlay,
        primary,
        new_worktrees,
        removed_worktrees,
        worktree_dir,
        block_size,
        max,
        hosts_file,
        matrix
    );
    base.vars.extend(overlay.vars);
}

/// Retain unspecified task options across local overlays.
fn merge_task(base: &mut TaskConfig, overlay: TaskConfig) {
    replace!(
        base,
        overlay,
        run,
        run_script,
        cwd,
        when,
        after,
        profiles,
        steps,
        confirm,
        interactive,
        timeout,
        pty
    );
    base.env.extend(overlay.env);
    merge_option(&mut base.fnox, overlay.fnox, |base, overlay| {
        replace!(base, overlay, profile);
    });
}

/// Merge optional typed objects rather than replacing an entire object.
pub(crate) fn merge_option<T>(
    base: &mut Option<T>,
    overlay: Option<T>,
    merge: impl FnOnce(&mut T, T),
) {
    if let Some(overlay) = overlay {
        if let Some(base) = base {
            merge(base, overlay);
        } else {
            *base = Some(overlay);
        }
    }
}

/// Merge named infra command objects, replacing shorthand commands wholesale.
fn merge_command(base: &mut InfraCommand, overlay: InfraCommand) {
    match (base, overlay) {
        (InfraCommand::Object(base), InfraCommand::Object(overlay)) => {
            replace!(
                base,
                overlay,
                run,
                run_script,
                container,
                ignore_errors,
                cwd,
                timeout
            );
            base.env.extend(overlay.env);
        }
        (base, overlay) => *base = overlay,
    }
}

/// Merge infra maps including individual container declarations.
fn merge_infra(base: &mut InfraConfig, overlay: InfraConfig) {
    replace!(
        base,
        overlay,
        compose,
        project_name,
        exclude,
        per_lane,
        profiles,
        hidden
    );
    base.env.extend(overlay.env);
    base.hosts.extend(overlay.hosts);
    merge_map(&mut base.setup, overlay.setup, merge_command);
    merge_map(&mut base.teardown, overlay.teardown, merge_command);
    merge_map(&mut base.containers, overlay.containers, |base, overlay| {
        replace!(base, overlay, image, command, volumes, per_lane);
        merge_option(&mut base.ready, overlay.ready, merge_ready);
        base.ports.extend(overlay.ports);
        base.env.extend(overlay.env);
    });
}

/// Preserve HTTP readiness fields when an override changes only its timeout.
pub(crate) fn merge_ready(base: &mut crate::ReadyConfig, overlay: crate::ReadyConfig) {
    match (base, overlay) {
        (crate::ReadyConfig::Check(base), crate::ReadyConfig::Check(overlay)) => {
            replace!(base, overlay, tcp, log, cmd, timeout);
            merge_option(&mut base.http, overlay.http, |base, overlay| {
                replace!(base, overlay, path, status, timeout);
            });
        }
        (base, overlay) => *base = overlay,
    }
}

impl<E: Extensions> PartialProject<E> {
    /// Apply local settings before any template is expanded.
    fn merge(&mut self, overlay: Self) {
        replace!(self, overlay, project);
        merge_spanned(&mut self.lanes, overlay.lanes, merge_lanes);
        merge_spanned(&mut self.infra, overlay.infra, merge_infra);
        merge_spanned(&mut self.fnox, overlay.fnox, |base, overlay| {
            replace!(base, overlay, profile);
        });
        merge_spanned(&mut self.global_env, overlay.global_env, |base, overlay| {
            base.extend(overlay)
        });
        merge_map(
            &mut self.templates,
            overlay.templates,
            PartialService::merge,
        );
        merge_map(&mut self.services, overlay.services, PartialService::merge);
        merge_map(&mut self.tasks, overlay.tasks, merge_task);
        merge_map(&mut self.tags, overlay.tags, |base, overlay| {
            replace!(base, overlay, color, description);
        });
        merge_map(
            &mut self.profiles,
            overlay.profiles,
            |base, overlay| match (base, overlay) {
                (ProfileConfig::Object(base), ProfileConfig::Object(overlay)) => {
                    replace!(base, overlay, include, exclude, infra);
                    base.env.extend(overlay.env);
                    base.run_scripts.extend(overlay.run_scripts);
                }
                (base, overlay) => *base = overlay,
            },
        );
    }

    /// Expand all templates, including unused ones so hidden cycles cannot survive checks.
    fn finish(self) -> Result<ProjectConfig<E>, ConfigError> {
        let mut templates = IndexMap::new();
        for (name, value) in &self.templates {
            templates.insert(
                name.clone(),
                resolve(value, &self.templates, &mut vec![name.clone()])
                    .map_err(|e| {
                        ConfigError::plain(format!("templates.{name}.extends: {}", e.message))
                    })?
                    .finish(),
            );
        }
        let mut services = IndexMap::new();
        for (name, value) in self.services {
            services.insert(
                name.clone(),
                resolve(&value, &self.templates, &mut Vec::new())
                    .map_err(|e| {
                        ConfigError::plain(format!("services.{name}.extends: {}", e.message))
                    })?
                    .finish(),
            );
        }
        Ok(ProjectConfig {
            project: self
                .project
                .ok_or_else(|| ConfigError::plain("project: missing required project name"))?
                .value,
            lanes: self.lanes.map(|v| v.value).unwrap_or_default(),
            infra: self.infra.map(|v| v.value).unwrap_or_default(),
            fnox: self.fnox.map(|v| v.value),
            global_env: self.global_env.map(|v| v.value).unwrap_or_default(),
            templates,
            services,
            tasks: self.tasks,
            profiles: self.profiles,
            tags: self.tags,
        })
    }
}

/// Follow typed template references with an explicit recursion stack.
fn resolve<E: Extensions>(
    service: &PartialService<E>,
    templates: &IndexMap<String, PartialService<E>>,
    stack: &mut Vec<String>,
) -> Result<PartialService<E>, ConfigError> {
    let Some(parent) = &service.extends else {
        return Ok(service.clone());
    };
    if stack.contains(&parent.value) {
        stack.push(parent.value.clone());
        return Err(ConfigError::plain(format!(
            "template cycle: {}",
            stack.join(" -> ")
        )));
    }
    let template = templates
        .get(&parent.value)
        .ok_or_else(|| ConfigError::plain(format!("unknown template `{}`", parent.value)))?;
    stack.push(parent.value.clone());
    let mut resolved = resolve(template, templates, stack)?;
    stack.pop();
    resolved.merge(service.clone());
    Ok(resolved)
}

/// Decode with serde-saphyr, preserving its precise syntax/type error position.
pub(crate) fn parse_yaml<T: DeserializeOwned>(file: &str, source: &str) -> Result<T, ConfigError> {
    serde_saphyr::from_str(source).map_err(|error| {
        let offset = error.location().map_or(0, |l| {
            line_offset(source, l.line() as usize, l.column() as usize)
        });
        // Miette owns source rendering; nesting serde-saphyr's snippet would duplicate it.
        let mut message = error.without_snippet().to_string();
        if message.contains("unknown field `gui`") {
            message = "`gui` is not supported yet".into();
        }
        if message.contains("unknown field `on`") {
            message = "unknown key `on`; use `when` for lifecycle hooks".into();
        }
        if let Some((_, unknown)) = message.split_once("unknown field `")
            && let Some((key, expected)) = unknown.split_once('`')
        {
            let candidates: Vec<_> = expected
                .split('`')
                .enumerate()
                .filter_map(|(i, s)| (i % 2 == 1).then_some(s))
                .collect();
            if let Some(suggestion) = candidates.into_iter().max_by(|a, b| {
                strsim::jaro_winkler(key, a).total_cmp(&strsim::jaro_winkler(key, b))
            }) && strsim::jaro_winkler(key, suggestion) > 0.75
            {
                message = format!("unknown key `{key}`; did you mean `{suggestion}`?");
            }
        }
        ConfigError::at(file, source, offset, message)
    })
}

/// Translate character-based YAML positions into miette byte spans.
fn line_offset(source: &str, line: usize, column: usize) -> usize {
    let start = source
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    start
        + source
            .get(start..)
            .and_then(|s| s.lines().next())
            .map_or(0, |s| {
                s.char_indices()
                    .nth(column.saturating_sub(1))
                    .map_or(s.len(), |(i, _)| i)
            })
}

/// Track source positions independently from typed merge data.
#[derive(Debug, Default)]
struct SourceNode {
    children: IndexMap<String, Spanned<SourceNode>>,
}

impl<'de> Deserialize<'de> for SourceNode {
    /// Capture every mapping value's parser span without interpreting its content.
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct NodeVisitor;
        impl<'de> serde::de::Visitor<'de> for NodeVisitor {
            type Value = SourceNode;
            /// Describe all legal YAML values to serde.
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a YAML value")
            }
            /// Capture map descendants for validation diagnostics.
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut children = IndexMap::new();
                while let Some((key, value)) = map.next_entry()? {
                    children.insert(key, value);
                }
                Ok(SourceNode { children })
            }
            /// Preserve list indices in diagnostic paths.
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> Result<Self::Value, A::Error> {
                let mut children = IndexMap::new();
                while let Some(value) = seq.next_element()? {
                    children.insert(children.len().to_string(), value);
                }
                Ok(SourceNode { children })
            }
            /// Scalars carry their spans in the outer wrapper.
            fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<Self::Value, E> {
                Ok(SourceNode::default())
            }
            /// Ignore scalar contents when capturing metadata.
            fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<Self::Value, E> {
                Ok(SourceNode::default())
            }
            /// Ignore scalar contents when capturing metadata.
            fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<Self::Value, E> {
                Ok(SourceNode::default())
            }
            /// Ignore scalar contents when capturing metadata.
            fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<Self::Value, E> {
                Ok(SourceNode::default())
            }
            /// Ignore scalar contents when capturing metadata.
            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<Self::Value, E> {
                Ok(SourceNode::default())
            }
            /// Null values need no descendant positions.
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(SourceNode::default())
            }
        }
        d.deserialize_any(NodeVisitor)
    }
}

/// Find the deepest source node matching a validation path.
fn source_position(source: &str, path: &str) -> Option<(usize, usize)> {
    let node: SourceNode = serde_saphyr::from_str(source).ok()?;
    let mut children = &node.children;
    let mut found = None;
    for (depth, component) in path.split('.').enumerate() {
        let Some(child) = children.get(component) else {
            break;
        };
        found = Some((
            depth + 1,
            line_offset(
                source,
                child.referenced.line() as usize,
                child.referenced.column() as usize,
            ),
        ));
        children = &child.value.children;
    }
    found
}

/// Attribute semantic failures to the most specific source, preferring local overrides.
fn locate(
    error: ConfigError,
    file: &str,
    source: &str,
    local: Option<(&str, &str)>,
) -> ConfigError {
    let path = error
        .message
        .split_once(':')
        .map_or(error.message.as_str(), |(p, _)| p);
    let base = source_position(source, path);
    if let Some((name, text)) = local
        && let Some((depth, offset)) = source_position(text, path)
        && base.is_none_or(|(d, _)| depth >= d)
    {
        return ConfigError::at(name, text, offset, error.message);
    }
    ConfigError::at(file, source, base.map_or(0, |(_, o)| o), error.message)
}

/// Parse and validate an in-memory project plus an optional local overlay.
pub fn parse_project(source: &str, local: Option<&str>) -> Result<ProjectConfig, ConfigError> {
    parse_project_with::<BuiltinExtensions>(source, local)
}

/// Parse a project with the binary's registered integration schemas.
pub fn parse_project_with<E: Extensions>(
    source: &str,
    local: Option<&str>,
) -> Result<ProjectConfig<E>, ConfigError> {
    parse_named(
        "devlanes.yaml",
        source,
        local.map(|s| ("devlanes.local.yaml", s)),
    )
}

/// Apply named sources so all returned diagnostics identify their original file.
fn parse_named<E: Extensions>(
    file: &str,
    source: &str,
    local: Option<(&str, &str)>,
) -> Result<ProjectConfig<E>, ConfigError> {
    let mut partial: PartialProject<E> = parse_yaml(file, source)?;
    if let Some((name, local)) = local {
        partial.merge(parse_yaml(name, local)?);
    }
    let config = partial
        .finish()
        .map_err(|e| locate(e, file, source, local))?;
    crate::validate(&config).map_err(|e| locate(e, file, source, local))?;
    Ok(config)
}

/// Load a project file (or a directory containing it) and its sibling local override.
pub fn load_project(path: impl AsRef<Path>) -> Result<ProjectConfig, ConfigError> {
    load_project_with::<BuiltinExtensions>(path)
}

/// Load with custom integration parsing; no external commands are executed.
pub fn load_project_with<E: Extensions>(
    path: impl AsRef<Path>,
) -> Result<ProjectConfig<E>, ConfigError> {
    let path = path.as_ref();
    let file = if path.is_dir() {
        path.join("devlanes.yaml")
    } else {
        path.to_owned()
    };
    let source = std::fs::read_to_string(&file).map_err(|e| ConfigError::io(&file, e))?;
    let local_path = file.with_file_name("devlanes.local.yaml");
    let local = match std::fs::read_to_string(&local_path) {
        Ok(value) => Some(value),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(ConfigError::io(&local_path, error)),
    };
    parse_named(
        &file.display().to_string(),
        &source,
        local
            .as_deref()
            .map(|s| (local_path.to_str().unwrap_or("devlanes.local.yaml"), s)),
    )
}

/// Load and return warnings for `config check`; validation failures are errors.
pub fn check(path: impl AsRef<Path>) -> Result<crate::CheckReport, ConfigError> {
    crate::validate(&load_project(path)?)
}

/// Determine all transitively referenced templates for unused-template warnings.
pub(crate) fn used_templates<E: Extensions>(config: &ProjectConfig<E>) -> HashSet<String> {
    let mut used = HashSet::new();
    for service in config.services.values() {
        let mut next = service.extends.as_ref();
        while let Some(name) = next {
            if !used.insert(name.clone()) {
                break;
            }
            next = config
                .templates
                .get(name)
                .and_then(|template| template.extends.as_ref());
        }
    }
    used
}

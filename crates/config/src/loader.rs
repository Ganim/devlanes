use crate::{
    CheckReport, ComposeFiles, ConfigError, Env, Extensions, FnoxConfig, InfraCommand,
    InfraCommandOptions, InfraConfig, LanesConfig, OpaqueExtensions, ProfileConfig, ProfileOptions,
    ProjectConfig, TaskConfig, service::PartialService,
};
use indexmap::IndexMap;
use serde::{Deserialize, de::DeserializeOwned};
use serde_saphyr::Spanned;
use std::{collections::HashSet, path::Path};

/// Hint for `on:`, which users borrow from CI syntax for task hooks and service lifecycles.
pub(crate) const ON_HINT: &str = "unknown key `on`; lifecycle hooks are tasks with `when`";

/// Original YAML documents kept in memory so later integration checks can point at a line.
#[derive(Clone, Default)]
pub struct ConfigSources {
    base: Option<(String, String)>,
    local: Option<(String, String)>,
}

impl std::fmt::Debug for ConfigSources {
    /// Hide source contents, which may contain credentials, from debug output.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConfigSources { .. }")
    }
}

impl ConfigSources {
    /// Attach an error whose message starts with a field path to that field's YAML position.
    pub fn locate(&self, error: ConfigError) -> ConfigError {
        match &self.base {
            Some((file, source)) => locate(
                error,
                file,
                source,
                self.local.as_ref().map(|(f, s)| (f.as_str(), s.as_str())),
            ),
            None => error,
        }
    }

    /// Source position of the deepest YAML node on a dotted field path such as
    /// `services.api.hosts.0`, preferring the local file when it sets that field.
    /// `None` when the config was not parsed from YAML or no part of the path matches.
    pub fn position(&self, path: &str) -> Option<crate::SourceLocation> {
        let (file, source) = self.base.as_ref()?;
        let error = locate(
            ConfigError::plain(format!("{path}:")),
            file,
            source,
            self.local.as_ref().map(|(f, s)| (f.as_str(), s.as_str())),
        );
        let local = self
            .local
            .as_ref()
            .is_some_and(|(name, _)| *name == error.location.file);
        let matched = local || source_position(source, path).is_some();
        matched.then_some(error.location)
    }
}

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

/// Merge a local task per key; `run`, `run_script` and `steps` are alternatives, so setting
/// one clears the others.
fn merge_task(base: &mut TaskConfig, mut overlay: TaskConfig) {
    if overlay.run.is_some() || overlay.steps.is_some() {
        base.run_script = None;
    }
    if overlay.run_script.is_some() || overlay.steps.is_some() {
        base.run = None;
    }
    if overlay.run.is_some() || overlay.run_script.is_some() {
        base.steps = None;
    }
    merge_option(
        &mut base.run_script,
        overlay.run_script.take(),
        merge_script,
    );
    replace!(
        base,
        overlay,
        run,
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

/// Read command shorthand as the mapping `{ run: ... }` it abbreviates.
fn command_options(command: InfraCommand) -> InfraCommandOptions {
    match command {
        InfraCommand::Object(options) => *options,
        InfraCommand::Command(run) => InfraCommandOptions {
            run: Some(run),
            ..Default::default()
        },
    }
}

/// Merge named infra commands per key; shorthand counts as `run`, which clears `run_script`.
fn merge_command(base: &mut InfraCommand, overlay: InfraCommand) {
    if matches!(
        (&*base, &overlay),
        (InfraCommand::Command(_), InfraCommand::Command(_))
    ) {
        *base = overlay;
        return;
    }
    let mut merged = command_options(std::mem::replace(
        base,
        InfraCommand::Object(Box::default()),
    ));
    let mut overlay = command_options(overlay);
    if overlay.run.is_some() {
        merged.run_script = None;
    }
    if overlay.run_script.is_some() {
        merged.run = None;
    }
    merge_option(
        &mut merged.run_script,
        overlay.run_script.take(),
        merge_script,
    );
    replace!(merged, overlay, run, container, ignore_errors, cwd, timeout);
    merged.env.extend(overlay.env);
    *base = InfraCommand::Object(Box::new(merged));
}

/// Read a selector list as the `{ include: [...] }` mapping it abbreviates.
fn profile_options(profile: ProfileConfig) -> ProfileOptions {
    match profile {
        ProfileConfig::Object(options) => options,
        ProfileConfig::Selectors(include) => ProfileOptions {
            include: Some(include),
            ..Default::default()
        },
    }
}

/// Merge profiles per key; a selector list replaces only `include`.
fn merge_profile(base: &mut ProfileConfig, overlay: ProfileConfig) {
    if matches!(
        (&*base, &overlay),
        (ProfileConfig::Selectors(_), ProfileConfig::Selectors(_))
    ) {
        *base = overlay;
        return;
    }
    let mut merged = profile_options(std::mem::replace(
        base,
        ProfileConfig::Selectors(Vec::new()),
    ));
    let overlay = profile_options(overlay);
    replace!(merged, overlay, include, exclude, infra);
    merged.env.extend(overlay.env);
    merged.run_scripts.extend(overlay.run_scripts);
    *base = ProfileConfig::Object(merged);
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

/// Merge readiness checks per key. `started` and the check mapping are alternatives, and so
/// are `tcp`, `http`, `log` and `cmd`: setting one clears the others.
pub(crate) fn merge_ready(base: &mut crate::ReadyConfig, overlay: crate::ReadyConfig) {
    match (base, overlay) {
        (crate::ReadyConfig::Check(base), crate::ReadyConfig::Check(overlay)) => {
            let probes = [
                overlay.tcp.is_some(),
                overlay.http.is_some(),
                overlay.log.is_some(),
                overlay.cmd.is_some(),
            ];
            if probes.contains(&true) {
                if !probes[0] {
                    base.tcp = None;
                }
                if !probes[1] {
                    base.http = None;
                }
                if !probes[2] {
                    base.log = None;
                }
                if !probes[3] {
                    base.cmd = None;
                }
            }
            replace!(base, overlay, tcp, log, cmd, timeout);
            merge_option(&mut base.http, overlay.http, |base, overlay| {
                replace!(base, overlay, path, status, timeout);
            });
        }
        (base, overlay) => *base = overlay,
    }
}

/// Merge `{ source, name }` per key; the `file#name` shorthand replaces the whole selection.
pub(crate) fn merge_script(base: &mut crate::RunScript, overlay: crate::RunScript) {
    match (base, overlay) {
        (crate::RunScript::Object(base), crate::RunScript::Object(overlay)) => {
            if !overlay.source.is_empty() {
                base.source = overlay.source;
            }
            if !overlay.name.is_empty() {
                base.name = overlay.name;
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Merge opaque preset mappings per key, recursively; lists and scalars replace.
pub(crate) fn merge_json(base: &mut serde_json::Value, overlay: serde_json::Value) {
    match (base, overlay) {
        (serde_json::Value::Object(base), serde_json::Value::Object(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(base) => merge_json(base, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

impl<E: Extensions> PartialProject<E> {
    /// Apply local settings before any template is expanded. Mappings merge per key at every
    /// level; lists and scalars replace.
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
        merge_map(&mut self.profiles, overlay.profiles, merge_profile);
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
                    .finish(&format!("templates.{name}"))?,
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
                    .finish(&format!("services.{name}"))?,
            );
        }
        Ok(ProjectConfig {
            sources: ConfigSources::default(),
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
    let template = templates.get(&parent.value).ok_or_else(|| {
        ConfigError::plain(format!(
            "unknown template `{}`{}",
            parent.value,
            did_you_mean(&parent.value, templates.keys().map(String::as_str))
        ))
    })?;
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
        // Miette owns source rendering and the location, so drop serde-saphyr's own copies.
        let mut message = strip_location(&error.without_snippet().to_string()).to_owned();
        if let Some((_, unknown)) = message.split_once("unknown field `")
            && let Some((key, expected)) = unknown.split_once('`')
            && let Some(hint) = unknown_key_hint(key, expected)
        {
            message = hint;
        }
        ConfigError::at(file, source, offset, message)
    })
}

/// Remove a trailing ` at line N, column M`, which the diagnostic already shows.
fn strip_location(message: &str) -> &str {
    message
        .rfind(" at line ")
        .filter(|&start| {
            let tail = &message[start + " at line ".len()..];
            tail.split_once(", column ").is_some_and(|(line, column)| {
                !line.is_empty()
                    && !column.is_empty()
                    && line.bytes().all(|b| b.is_ascii_digit())
                    && column.bytes().all(|b| b.is_ascii_digit())
            })
        })
        .map_or(message, |start| &message[..start])
}

/// The candidate closest to `key`, when one is close enough to be a typo.
pub(crate) fn closest<'a>(
    key: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<&'a str> {
    candidates
        .into_iter()
        .filter(|candidate| *candidate != key)
        .max_by(|a, b| strsim::jaro_winkler(key, a).total_cmp(&strsim::jaro_winkler(key, b)))
        .filter(|candidate| strsim::jaro_winkler(key, candidate) > 0.75)
}

/// `; did you mean `x`?` for the candidate closest to `key`, or nothing when none is close.
pub(crate) fn did_you_mean<'a>(key: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    closest(key, candidates)
        .map(|candidate| format!("; did you mean `{candidate}`?"))
        .unwrap_or_default()
}

/// Name the closest expected key for an unknown-field error. serde-saphyr lists the expected
/// names after "expected" or "expected one of", with or without backticks.
fn unknown_key_hint(key: &str, expected: &str) -> Option<String> {
    if key == "on" {
        return Some(ON_HINT.into());
    }
    let expected = expected.split(" at line").next().unwrap_or(expected);
    let expected = expected
        .trim_start_matches(',')
        .trim_start()
        .trim_start_matches("expected ")
        .trim_start_matches("one of ");
    expected
        .split(", ")
        .map(|name| name.trim().trim_matches('`'))
        .filter(|name| !name.is_empty() && !name.contains(' '))
        .max_by(|a, b| strsim::jaro_winkler(key, a).total_cmp(&strsim::jaro_winkler(key, b)))
        .filter(|name| strsim::jaro_winkler(key, name) > 0.75)
        .map(|name| format!("unknown key `{key}`; did you mean `{name}`?"))
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
    let mut remaining = path;
    let mut depth = 0;
    while !remaining.is_empty() {
        // Keys may contain dots, so match the longest key that ends at a path separator.
        let Some((key, child)) = children
            .iter()
            .filter(|(key, _)| {
                remaining
                    .strip_prefix(key.as_str())
                    .is_some_and(|tail| tail.is_empty() || tail.starts_with('.'))
            })
            .max_by_key(|(key, _)| key.len())
        else {
            break;
        };
        depth += key.len();
        found = Some((
            depth,
            line_offset(
                source,
                child.referenced.line() as usize,
                child.referenced.column() as usize,
            ),
        ));
        remaining = remaining[key.len()..].trim_start_matches('.');
        children = &child.value.children;
    }
    found
}

/// Attribute semantic failures to the most specific source, preferring local overrides.
pub(crate) fn locate(
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
    parse_project_with::<OpaqueExtensions>(source, local)
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
    .map(|(config, _)| config)
}

/// Apply named sources so all returned diagnostics identify their original file.
fn parse_named<E: Extensions>(
    file: &str,
    source: &str,
    local: Option<(&str, &str)>,
) -> Result<(ProjectConfig<E>, CheckReport), ConfigError> {
    let mut partial: PartialProject<E> = parse_yaml(file, source)?;
    if partial.project.is_none() {
        // A project name set only in the local file would differ between checkouts.
        return Err(ConfigError::at(
            file,
            source,
            0,
            format!("project: {file} must set the project name"),
        ));
    }
    let base_services: Vec<String> = partial.services.keys().cloned().collect();
    let mut local_services = Vec::new();
    if let Some((name, local)) = local {
        let overlay: PartialProject<E> = parse_yaml(name, local)?;
        local_services.extend(
            overlay
                .services
                .keys()
                .filter(|service| !base_services.contains(service))
                .cloned(),
        );
        partial.merge(overlay);
    }
    let mut config = partial
        .finish()
        .map_err(|e| locate(e, file, source, local))?;
    config.sources = ConfigSources {
        base: Some((file.into(), source.into())),
        local: local.map(|(f, s)| (f.into(), s.into())),
    };
    for name in &local_services {
        let service = &config.services[name];
        if service.run.is_none() && service.run_script.is_none() {
            let suggestion = base_services
                .iter()
                .max_by(|a, b| {
                    strsim::jaro_winkler(name, a).total_cmp(&strsim::jaro_winkler(name, b))
                })
                .filter(|candidate| strsim::jaro_winkler(name, candidate) > 0.75)
                .map(|candidate| format!("; did you mean `{candidate}`?"))
                .unwrap_or_default();
            return Err(config.sources.locate(ConfigError::plain(format!(
                "services.{name}: service is defined only in the local file and needs run or run_script{suggestion}"
            ))));
        }
    }
    let report = crate::validate(&config).map_err(|e| locate(e, file, source, local))?;
    Ok((config, report))
}

/// Load a project file (or a directory containing it) and its sibling local override.
pub fn load_project(path: impl AsRef<Path>) -> Result<ProjectConfig, ConfigError> {
    load_project_with::<OpaqueExtensions>(path)
}

/// Load with custom integration parsing; no external commands are executed.
pub fn load_project_with<E: Extensions>(
    path: impl AsRef<Path>,
) -> Result<ProjectConfig<E>, ConfigError> {
    load_named(path.as_ref()).map(|(config, _)| config)
}

/// Read the base file and its sibling local file, returning the validation report as well.
fn load_named<E: Extensions>(path: &Path) -> Result<(ProjectConfig<E>, CheckReport), ConfigError> {
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
    let (config, report) = parse_named::<E>(
        &file.display().to_string(),
        &source,
        local
            .as_deref()
            .map(|s| (local_path.to_str().unwrap_or("devlanes.local.yaml"), s)),
    )?;
    check_compose_files(&config, file.parent().unwrap_or(Path::new(".")))?;
    Ok((config, report))
}

/// Fails, located at the entry, when an `infra.compose` file does not exist
/// relative to `dir`, the directory of `devlanes.yaml`. Blocking: reads the
/// filesystem.
pub fn check_compose_files<E: Extensions>(
    config: &ProjectConfig<E>,
    dir: &Path,
) -> Result<(), ConfigError> {
    let compose: Vec<&String> = match &config.infra.compose {
        Some(ComposeFiles::File(name)) => vec![name],
        Some(ComposeFiles::Files(names)) => names.iter().collect(),
        None => Vec::new(),
    };
    for (index, name) in compose.into_iter().enumerate() {
        let path = dir.join(name);
        if !path.is_file() {
            let field = match &config.infra.compose {
                Some(ComposeFiles::Files(_)) => format!("infra.compose.{index}"),
                _ => "infra.compose".to_owned(),
            };
            return Err(config.sources.locate(ConfigError::plain(format!(
                "{field}: Compose file not found: {}",
                path.display()
            ))));
        }
    }
    Ok(())
}

/// Load and return warnings for `config check`; validation failures are errors.
pub fn check(path: impl AsRef<Path>) -> Result<CheckReport, ConfigError> {
    load_named::<OpaqueExtensions>(path.as_ref()).map(|(_, report)| report)
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

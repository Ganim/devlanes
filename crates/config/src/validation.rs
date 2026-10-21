use crate::{
    ConfigError, Extensions, InfraCommand, ProfileConfig, ProjectConfig, ReadyConfig, Run,
    RunScript, TaskWhen, Templated,
};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};

#[derive(Debug, Clone, Serialize)]
pub struct ConfigWarning {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    pub project: String,
    pub warnings: Vec<ConfigWarning>,
}

/// Validate project structure and references that do not require a resolved Compose model.
pub fn validate<E: Extensions>(config: &ProjectConfig<E>) -> Result<CheckReport, ConfigError> {
    validate_inner(config, None)
}

/// Integration-owned parser callback; the optional object is absent when no block was given.
pub type PresetValidator<'a> = dyn Fn(&str, Option<&serde_json::Value>) -> Result<(), String> + 'a;

/// Validate each service's opaque preset options using the composition root's registry.
/// Callers may validate evaluated services when an integration requires concrete scalar types.
pub fn validate_with_presets(
    config: &ProjectConfig,
    validator: &PresetValidator<'_>,
) -> Result<CheckReport, ConfigError> {
    let report = validate(config)?;
    for (name, service) in &config.services {
        if let Some(preset) = &service.preset {
            validator(preset, service.preset_options.get(preset))
                .map_err(|message| fail(&format!("services.{name}.{preset}"), message))?;
        }
    }
    Ok(report)
}

/// Validate infra names against the integration's fully resolved Compose service names.
pub fn validate_with_compose_services<E: Extensions>(
    config: &ProjectConfig<E>,
    names: &HashSet<String>,
) -> Result<CheckReport, ConfigError> {
    validate_inner(config, Some(names))
}

/// Add a precise configuration path to a semantic error.
fn fail(path: &str, message: impl Into<String>) -> ConfigError {
    ConfigError::plain(format!("{path}: {}", message.into()))
}

/// Check executable declarations uniformly for services, tasks, and infra commands.
fn command(
    path: &str,
    run: &Option<Run>,
    script: &Option<RunScript>,
    steps: bool,
) -> Result<(), ConfigError> {
    if usize::from(run.is_some()) + usize::from(script.is_some()) + usize::from(steps) != 1 {
        return Err(fail(
            path,
            "exactly one of run, run_script, or task steps is required",
        ));
    }
    if let Some(Run::Argv(argv)) = run
        && argv.is_empty()
    {
        return Err(fail(path, "run argv cannot be empty"));
    }
    if let Some(Run::Shell(Templated::Literal(shell))) = run
        && shell.trim().is_empty()
    {
        return Err(fail(path, "run cannot be empty"));
    }
    if let Some(script) = script {
        script
            .source_and_name()
            .map_err(|e| fail(path, e.message))?;
    }
    Ok(())
}

/// Ensure readiness is a single mechanism rather than an ambiguous combination.
fn readiness(path: &str, ready: &ReadyConfig) -> Result<(), ConfigError> {
    if let ReadyConfig::Check(check) = ready {
        if usize::from(check.tcp.is_some())
            + usize::from(check.http.is_some())
            + usize::from(check.log.is_some())
            + usize::from(check.cmd.is_some())
            != 1
        {
            return Err(fail(
                path,
                "ready requires exactly one of tcp, http, log, or cmd",
            ));
        }
        if matches!(check.tcp, Some(crate::TcpReady::Enabled(false))) {
            return Err(fail(path, "tcp: false is invalid; use ready: started"));
        }
        if let Some(http) = &check.http
            && let Some(Templated::Literal(status)) = http.status
            && !(100..=599).contains(&status)
        {
            return Err(fail(
                path,
                "HTTP readiness status must be between 100 and 599",
            ));
        }
    }
    Ok(())
}

/// Match profile selectors with the same literal, tag, and glob syntax used at start.
fn selector<E: Extensions>(
    config: &ProjectConfig<E>,
    path: &str,
    selector: &str,
) -> Result<(), ConfigError> {
    let matched = if let Some(tag) = selector.strip_prefix('+') {
        config.services.values().any(|s| {
            s.tags
                .as_ref()
                .is_some_and(|tags| tags.iter().any(|t| t == tag))
        })
    } else {
        let glob = globset::Glob::new(selector)
            .map_err(|e| fail(path, format!("invalid selector `{selector}`: {e}")))?
            .compile_matcher();
        config.services.keys().any(|name| glob.is_match(name))
    };
    if !matched {
        return Err(fail(
            path,
            format!("selector `{selector}` matches no services"),
        ));
    }
    Ok(())
}

/// Run structural checks before expression reference analysis.
fn validate_inner<E: Extensions>(
    config: &ProjectConfig<E>,
    compose: Option<&HashSet<String>>,
) -> Result<CheckReport, ConfigError> {
    let name = &config.project;
    if name.is_empty()
        || !name.as_bytes()[0].is_ascii_lowercase() && !name.as_bytes()[0].is_ascii_digit()
        || !name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
    {
        return Err(fail("project", "expected [a-z0-9][a-z0-9._-]* (no @ or :)"));
    }
    if config.lanes.matrix.is_some() && !config.lanes.vars.is_empty() {
        return Err(fail(
            "lanes.matrix",
            "matrix and vars are mutually exclusive",
        ));
    }
    if config.lanes.matrix.as_ref().is_some_and(Vec::is_empty) {
        return Err(fail("lanes.matrix", "must contain at least one lane"));
    }
    if matches!(config.lanes.max, Some(Templated::Literal(0))) {
        return Err(fail("lanes.max", "must be positive"));
    }
    if matches!(config.lanes.block_size, Some(Templated::Literal(0))) {
        return Err(fail("lanes.block_size", "must be positive"));
    }
    let mut aliases = HashSet::new();
    for (name, template) in &config.templates {
        for key in template.preset_options.keys() {
            if template.preset.as_ref() != Some(key) {
                return Err(fail(
                    &format!("templates.{name}.{key}"),
                    format!("unknown key `{key}`; options require preset: {key}"),
                ));
            }
        }
    }
    for name in config.services.keys() {
        if !aliases.insert(name.replace('-', "_")) {
            return Err(fail(
                &format!("services.{name}"),
                "ambiguous underscore alias",
            ));
        }
    }
    for (name, service) in &config.services {
        let path = format!("services.{name}");
        command(&path, &service.run, &service.run_script, false)?;
        if let Some(ready) = &service.ready {
            readiness(&format!("{path}.ready"), ready)?;
        }
        if let Some(preset) = &service.preset
            && !E::accepts_unregistered_presets()
            && !E::preset_names().contains(&preset.as_str())
        {
            return Err(fail(
                &format!("{path}.preset"),
                format!("unknown preset `{preset}`"),
            ));
        }
        for option in service.preset_options.keys() {
            if service.preset.as_ref() != Some(option) {
                return Err(fail(
                    &format!("{path}.{option}"),
                    format!("options require preset: {option}"),
                ));
            }
        }
        if matches!(service.replicas, Some(Templated::Literal(0))) {
            return Err(fail(&format!("{path}.replicas"), "must be positive"));
        }
        if let Some(stop) = &service.stop
            && let Some(signal) = &stop.signal
            && ![
                "SIGTERM", "SIGINT", "SIGHUP", "SIGQUIT", "SIGKILL", "SIGUSR1", "SIGUSR2",
            ]
            .contains(&signal.as_str())
        {
            return Err(fail(
                &format!("{path}.stop.signal"),
                format!("unsupported stop signal `{signal}`"),
            ));
        }
    }
    let mut task_graph = IndexMap::new();
    for (name, task) in &config.tasks {
        let path = format!("tasks.{name}");
        command(&path, &task.run, &task.run_script, task.steps.is_some())?;
        if task.when.is_some()
            && matches!(
                task.interactive,
                Some(Templated::Literal(true)) | Some(Templated::Expression(_))
            )
        {
            return Err(fail(
                &format!("{path}.interactive"),
                "interactive tasks cannot have when",
            ));
        }
        if task.after.is_some() && !matches!(task.when, Some(TaskWhen::Create | TaskWhen::Start)) {
            return Err(fail(
                &format!("{path}.after"),
                "after: services requires when: lane.create or lane.start",
            ));
        }
        if let Some(profiles) = &task.profiles {
            if task.when.is_none() {
                return Err(fail(
                    &format!("{path}.profiles"),
                    "profiles requires a when hook",
                ));
            }
            for profile in profiles {
                if profile != "default" && !config.profiles.contains_key(profile) {
                    return Err(fail(
                        &format!("{path}.profiles"),
                        format!("unknown profile `{profile}`"),
                    ));
                }
            }
        }
        let mut edges = Vec::new();
        if let Some(steps) = &task.steps {
            if steps.is_empty() {
                return Err(fail(&format!("{path}.steps"), "steps cannot be empty"));
            }
            for step in steps {
                let exists = if let Some(name) = step.strip_prefix("infra.setup.") {
                    config.infra.setup.contains_key(name)
                } else if let Some(name) = step.strip_prefix("infra.teardown.") {
                    config.infra.teardown.contains_key(name)
                } else {
                    edges.push(step.clone());
                    config.tasks.contains_key(step)
                };
                if !exists {
                    return Err(fail(
                        &format!("{path}.steps"),
                        format!("unknown step `{step}`"),
                    ));
                }
            }
        }
        task_graph.insert(name.clone(), edges);
    }
    crate::expression::check_cycles(&task_graph).map_err(|e| fail("tasks", e.message))?;
    for (name, profile) in &config.profiles {
        let path = format!("profiles.{name}");
        match profile {
            ProfileConfig::Selectors(values) => {
                for value in values {
                    selector(config, &path, value)?;
                }
            }
            ProfileConfig::Object(options) => {
                for (from, to) in &options.run_scripts {
                    if from.trim().is_empty() || to.trim().is_empty() {
                        return Err(fail(
                            &format!("{path}.run_scripts"),
                            "script remapping keys and values must be non-empty script names",
                        ));
                    }
                }
                for value in options
                    .include
                    .iter()
                    .flatten()
                    .chain(options.exclude.iter().flatten())
                {
                    selector(config, &path, value)?;
                }
            }
        }
    }
    let mut report = CheckReport {
        project: config.project.clone(),
        warnings: Vec::new(),
    };
    let used = crate::loader::used_templates(config);
    for name in config.templates.keys().filter(|name| !used.contains(*name)) {
        report.warnings.push(ConfigWarning {
            path: format!("templates.{name}"),
            message: "unused template".into(),
        });
    }
    let known: HashSet<_> = config
        .infra
        .containers
        .keys()
        .cloned()
        .chain(compose.into_iter().flatten().cloned())
        .collect();
    let can_validate_infra = config.infra.compose.is_none() || compose.is_some();
    if !can_validate_infra {
        report.warnings.push(ConfigWarning {
            path: "infra.compose".into(),
            message:
                "Compose service references require validation against the resolved Compose model"
                    .into(),
        });
    }
    let infra_names = config
        .infra
        .hosts
        .keys()
        .chain(config.infra.hidden.iter().flatten())
        .chain(config.infra.per_lane.iter().flatten())
        .chain(config.infra.exclude.iter().flatten());
    for name in infra_names {
        if can_validate_infra && !known.contains(name) {
            return Err(fail(
                "infra",
                format!("unknown container or Compose service `{name}`"),
            ));
        }
    }
    for (name, container) in &config.infra.containers {
        if container.image.is_none() {
            return Err(fail(
                &format!("infra.containers.{name}.image"),
                "image is required",
            ));
        }
        for (port_name, port) in &container.ports {
            if matches!(port, Templated::Literal(0)) {
                return Err(fail(
                    &format!("infra.containers.{name}.ports.{port_name}"),
                    "container port must be positive",
                ));
            }
        }
        if let Some(ready) = &container.ready {
            readiness(&format!("infra.containers.{name}.ready"), ready)?;
        }
    }
    for (phase, entries) in [
        ("setup", &config.infra.setup),
        ("teardown", &config.infra.teardown),
    ] {
        for (name, entry) in entries {
            let path = format!("infra.{phase}.{name}");
            match entry {
                InfraCommand::Object(options) => {
                    command(&path, &options.run, &options.run_script, false)?;
                    if let Some(name) = &options.container
                        && can_validate_infra
                        && !known.contains(name)
                    {
                        return Err(fail(&path, format!("unknown container `{name}`")));
                    }
                }
                InfraCommand::Command(run) => command(&path, &Some(run.clone()), &None, false)?,
            }
        }
    }
    expression_graph(config)?;
    crate::expression::validate_constants(config)?;
    validate_port_capacity(config)?;
    Ok(report)
}

/// Count distinct allocated names, including contributor ports, before accepting a fixed block.
fn validate_port_capacity<E: Extensions>(config: &ProjectConfig<E>) -> Result<(), ConfigError> {
    let Some(Templated::Literal(capacity)) = config.lanes.block_size else {
        return Ok(());
    };
    let mut ports = BTreeSet::new();
    for (name, service) in &config.services {
        if !service.env.contains_key("PORT") {
            ports.insert((name.clone(), "default".to_owned()));
        }
        if let Some(preset) = &service.preset {
            for port in E::required_ports(preset) {
                ports.insert((name.clone(), (*port).to_owned()));
            }
        }
        if let Some(hosts) = &service.hosts {
            for host in hosts {
                if let crate::HostConfig::Route(route) = host
                    && let Some(port) = &route.port
                {
                    ports.insert((name.clone(), port.clone()));
                }
            }
        }
        if let Some(ReadyConfig::Check(ready)) = &service.ready
            && let Some(crate::TcpReady::Port(port)) = &ready.tcp
        {
            ports.insert((name.clone(), port.clone()));
        }
        let mut texts = Vec::new();
        expressions(
            &serde_json::to_value(service).map_err(|e| fail("services", e.to_string()))?,
            "",
            &mut texts,
        );
        for (_, text) in texts {
            for reference in crate::references(&text)? {
                if let Some(port) = reference
                    .strip_prefix("self.ports.")
                    .and_then(|s| s.split('.').next())
                {
                    ports.insert((name.clone(), port.to_owned()));
                }
            }
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            for tail in compact.split("lane.port+").skip(1) {
                let number: String = tail.chars().take_while(char::is_ascii_digit).collect();
                if let Ok(offset) = number.parse::<u64>()
                    && offset >= u64::from(capacity)
                {
                    return Err(fail(
                        "lanes.block_size",
                        format!("explicit port offset {offset} exceeds the block"),
                    ));
                }
            }
        }
    }
    for (name, container) in &config.infra.containers {
        if matches!(container.per_lane, Some(Templated::Literal(true))) {
            for port in container.ports.keys() {
                ports.insert((format!("infra.{name}"), port.clone()));
            }
        }
    }
    if ports.len() > usize::from(capacity) {
        return Err(fail(
            "lanes.block_size",
            format!(
                "project needs at least {} allocated ports but block_size is {capacity}",
                ports.len()
            ),
        ));
    }
    Ok(())
}

/// Collect templates from serialized typed fields, excluding unused template declarations.
fn expressions(value: &serde_json::Value, path: &str, result: &mut Vec<(String, String)>) {
    match value {
        serde_json::Value::String(text) if text.contains("${{") => {
            result.push((path.into(), text.clone()))
        }
        serde_json::Value::Object(map) => {
            for (key, value) in map {
                expressions(value, &format!("{path}.{key}"), result);
            }
        }
        serde_json::Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                expressions(value, &format!("{path}.{index}"), result);
            }
        }
        _ => {}
    }
}

/// Resolve aliases to declaration names for reference validation.
fn service_name<'a, E: Extensions>(config: &'a ProjectConfig<E>, alias: &str) -> Option<&'a str> {
    config
        .services
        .keys()
        .find(|name| name.as_str() == alias || name.replace('-', "_") == alias)
        .map(String::as_str)
}

/// Build dependency edges from MiniJinja's static references, including PORT derivation.
fn expression_graph<E: Extensions>(config: &ProjectConfig<E>) -> Result<(), ConfigError> {
    let value = serde_json::to_value(config).map_err(|e| ConfigError::plain(e.to_string()))?;
    let mut all = Vec::new();
    if let Some(map) = value.as_object() {
        for (key, value) in map {
            if key != "templates" {
                expressions(value, key, &mut all);
            }
        }
    }
    let mut graph: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut origins = IndexMap::new();
    for name in config.lanes.vars.keys() {
        graph.insert(format!("lane.{}", name.replace('-', "_")), Vec::new());
    }
    for name in config.services.keys() {
        graph.insert(format!("svc.{name}"), Vec::new());
    }
    for (path, text) in all {
        let refs = crate::references(&text).map_err(|e| fail(&path, e.message))?;
        let mut edges = BTreeSet::new();
        let current_service = path
            .strip_prefix("services.")
            .and_then(|p| p.split('.').next());
        for reference in &refs {
            let mut components = reference.split('.');
            let root = components.next().unwrap_or_default();
            if ![
                "project", "lane", "lanes", "self", "svc", "infra", "env", "projects", "replica",
            ]
            .contains(&root)
            {
                return Err(fail(
                    &path,
                    format!(
                        "unknown expression context `{root}`; use underscores or brackets for dashed names"
                    ),
                ));
            }
            if root == "svc"
                && let Some(alias) = components.next()
            {
                let name = service_name(config, alias)
                    .ok_or_else(|| fail(&path, format!("unknown service reference `{alias}`")))?;
                edges.insert(format!("svc.{name}"));
            }
            if root == "lane"
                && let Some(name) = components.next()
            {
                edges.insert(format!("lane.{name}"));
            }
            if path.ends_with(".env.PORT")
                && (reference == "self.port"
                    || reference.starts_with("self.url")
                    || reference.starts_with("self.public_url"))
            {
                return Err(fail(
                    &path,
                    "expression cycle: env.PORT determines self.port and cannot reference it",
                ));
            }
        }
        if let Some(name) = path.strip_prefix("lanes.vars.") {
            let key = format!("lane.{}", name.replace('-', "_"));
            origins.insert(key.clone(), path.clone());
            graph.entry(key).or_default().extend(edges);
        } else if let Some(name) = current_service {
            origins.insert(format!("svc.{name}"), path.clone());
            graph
                .entry(format!("svc.{name}"))
                .or_default()
                .extend(edges);
        }
    }
    crate::expression::check_cycles(&graph).map_err(|error| {
        let first = error
            .message
            .split_once(": ")
            .and_then(|(_, chain)| chain.split(" -> ").next());
        let path = first
            .and_then(|node| origins.get(node))
            .map_or("lanes.vars", String::as_str);
        fail(path, error.message)
    })
}

use crate::{
    ConfigError, Extensions, InfraCommand, ProfileConfig, ProjectConfig, ReadyConfig, Run,
    RunScript, TaskWhen, Templated,
};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::{BTreeSet, HashSet};

/// A problem that does not stop loading, such as an unused template or an ignored setting.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigWarning {
    /// Dotted field path, such as `templates.api` or `tls.trust.0`.
    pub path: String,
    /// Human-readable explanation, without the path.
    pub message: String,
}

/// Result of a successful project check: the project name and its warnings.
#[derive(Debug, Clone, Serialize)]
pub struct CheckReport {
    /// Project name from the base file.
    pub project: String,
    /// Nonfatal findings in declaration order.
    pub warnings: Vec<ConfigWarning>,
}

/// Validate project structure and references that do not require a resolved Compose model.
pub fn validate<E: Extensions>(config: &ProjectConfig<E>) -> Result<CheckReport, ConfigError> {
    validate_inner(config, None, None)
}

/// Integration-owned parser callback; the optional object is absent when no block was given.
pub type PresetValidator<'a> = dyn Fn(&str, Option<&serde_json::Value>) -> Result<(), String> + 'a;

/// Named ports a registered preset allocates for a service, keyed by preset name.
pub type PresetPorts<'a> = dyn Fn(&str) -> Vec<String> + 'a;

/// Validate each service's opaque preset options using the composition root's registry.
/// Callers may validate evaluated services when an integration requires concrete scalar types.
pub fn validate_with_presets(
    config: &ProjectConfig,
    validator: &PresetValidator<'_>,
) -> Result<CheckReport, ConfigError> {
    validate_with_preset_ports(config, validator, &|_| Vec::new())
}

/// Like [`validate_with_presets`], also counting each preset's named ports against a fixed
/// `lanes.block_size`.
pub fn validate_with_preset_ports(
    config: &ProjectConfig,
    validator: &PresetValidator<'_>,
    ports: &PresetPorts<'_>,
) -> Result<CheckReport, ConfigError> {
    let report = validate_inner(config, None, Some(ports))?;
    for (name, service) in &config.services {
        if let Some(preset) = &service.preset {
            validator(preset, service.preset_options.get(preset)).map_err(|message| {
                // Point at the offending option when the integration names it.
                let field = message
                    .split_once("unknown field `")
                    .and_then(|(_, rest)| rest.split_once('`'))
                    .map(|(key, _)| format!(".{key}"))
                    .unwrap_or_default();
                config
                    .sources
                    .locate(fail(&format!("services.{name}.{preset}{field}"), message))
            })?;
        }
    }
    Ok(report)
}

/// Validate infra names against the integration's fully resolved Compose service names.
pub fn validate_with_compose_services<E: Extensions>(
    config: &ProjectConfig<E>,
    names: &HashSet<String>,
) -> Result<CheckReport, ConfigError> {
    validate_inner(config, Some(names), None)
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

/// Check routed hostnames: literal names fully, templates by their literal text.
fn hosts<'a>(
    path: &str,
    entries: impl IntoIterator<Item = &'a crate::HostConfig>,
) -> Result<(), ConfigError> {
    for entry in entries {
        let host = match entry {
            crate::HostConfig::Name(host) => host,
            crate::HostConfig::Route(route) => &route.host,
        };
        let checked = match host {
            Templated::Literal(host) => crate::normalize_host(host).map(drop),
            Templated::Expression(template) => crate::hostname::check_host_template(template),
        };
        checked.map_err(|message| fail(path, message))?;
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
    preset_ports: Option<&PresetPorts<'_>>,
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
                    crate::service::unknown_service_key::<E>(key, template.preset.as_deref(), true),
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
        if service.run.is_none() && service.run_script.is_none() {
            return Err(fail(&path, "service requires run or run_script"));
        }
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
                    crate::service::unknown_service_key::<E>(
                        option,
                        service.preset.as_deref(),
                        true,
                    ),
                ));
            }
        }
        if matches!(service.replicas, Some(Templated::Literal(0))) {
            return Err(fail(&format!("{path}.replicas"), "must be positive"));
        }
        hosts(&format!("{path}.hosts"), service.hosts.iter().flatten())?;
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
    for (name, entries) in &config.infra.hosts {
        hosts(&format!("infra.hosts.{name}"), entries)?;
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
    if let Some(matrix) = &config.lanes.matrix {
        // Each matrix row is the lane's vars for one lane, so each row gets its own graph.
        for (index, row) in matrix.iter().enumerate() {
            let mut lane_config = config.clone();
            lane_config.lanes.vars = row.clone();
            lane_config.lanes.matrix = None;
            expression_graph(&lane_config).map_err(|e| {
                ConfigError::plain(e.message.replacen(
                    "lanes.vars.",
                    &format!("lanes.matrix.{index}."),
                    1,
                ))
            })?;
        }
    }
    crate::expression::validate_constants(config)?;
    validate_port_capacity(config, preset_ports)?;
    Ok(report)
}

/// Count distinct allocated names, including preset ports, before accepting a fixed block.
fn validate_port_capacity<E: Extensions>(
    config: &ProjectConfig<E>,
    preset_ports: Option<&PresetPorts<'_>>,
) -> Result<(), ConfigError> {
    let Some(Templated::Literal(capacity)) = config.lanes.block_size else {
        return Ok(());
    };
    let mut ports = BTreeSet::new();
    for (name, service) in &config.services {
        if !service.env.contains_key("PORT") {
            ports.insert((name.clone(), "default".to_owned()));
        }
        if let Some(preset) = &service.preset
            && let Some(preset_ports) = preset_ports
        {
            for port in preset_ports(preset) {
                ports.insert((name.clone(), port));
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
            for offset in crate::expression::lane_port_offsets(&text)? {
                if offset >= u64::from(capacity) {
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

/// Collect expressions from serialized typed fields, including unused templates.
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
            expressions(value, key, &mut all);
        }
    }
    // Only fields that determine a service's endpoint are graph nodes: env.PORT sets
    // `svc.X.port` (and so `url`), hosts set `public_url`. Other fields only read endpoints, so
    // two services may reference each other's URLs, and a service its own.
    let mut graph: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut origins = IndexMap::new();
    for name in config.lanes.vars.keys() {
        graph.insert(format!("lane.{}", name.replace('-', "_")), Vec::new());
    }
    for name in config.services.keys() {
        graph.insert(format!("svc.{name}.port"), Vec::new());
        // Without hosts, public_url falls back to the local URL.
        graph.insert(
            format!("svc.{name}.public_url"),
            vec![format!("svc.{name}.port")],
        );
    }
    for (path, text) in all {
        let refs = crate::references(&text).map_err(|e| fail(&path, e.message))?;
        let mut edges = BTreeSet::new();
        let current_service = config
            .services
            .keys()
            .filter(|name| path.starts_with(&format!("services.{name}.")))
            .max_by_key(|name| name.len())
            .map(String::as_str);
        let node = if let Some(name) = path.strip_prefix("lanes.vars.") {
            Some(format!("lane.{}", name.replace('-', "_")))
        } else {
            current_service.and_then(|name| {
                let field = &path[format!("services.{name}.").len()..];
                if field == "env.PORT" {
                    Some(format!("svc.{name}.port"))
                } else if field.starts_with("hosts.") {
                    Some(format!("svc.{name}.public_url"))
                } else {
                    None
                }
            })
        };
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
            let target = match root {
                "svc" => match components.next() {
                    Some(alias) => Some(service_name(config, alias).ok_or_else(|| {
                        fail(&path, format!("unknown service reference `{alias}`"))
                    })?),
                    None => None,
                },
                "self" => current_service,
                _ => None,
            };
            if let Some(target) = target {
                match components.next() {
                    Some("public_url") => {
                        edges.insert(format!("svc.{target}.public_url"));
                    }
                    // Named ports are allocated, never computed.
                    Some("ports") => {}
                    Some("port" | "url") => {
                        edges.insert(format!("svc.{target}.port"));
                    }
                    _ => {
                        edges.insert(format!("svc.{target}.port"));
                        edges.insert(format!("svc.{target}.public_url"));
                    }
                }
            }
            if root == "lane"
                && let Some(name) = components.next()
            {
                edges.insert(format!("lane.{}", name.replace('-', "_")));
            }
            if root == "infra"
                && config.infra.compose.is_none()
                && let Some(name) = components.next()
                && !config
                    .infra
                    .containers
                    .keys()
                    .any(|key| key == name || key.replace('-', "_") == name)
            {
                return Err(fail(
                    &path,
                    format!(
                        "unknown container `{name}`; infra.containers does not declare it and there is no compose file"
                    ),
                ));
            }
            if let Some(alias) = reference
                .strip_prefix("lanes.main.svc.")
                .and_then(|rest| rest.split('.').next())
                && service_name(config, alias).is_none()
            {
                return Err(fail(
                    &path,
                    format!("unknown service reference `{alias}` in lanes.main"),
                ));
            }
        }
        if let Some(node) = node {
            origins.entry(node.clone()).or_insert_with(|| path.clone());
            graph.entry(node).or_default().extend(edges);
        }
    }
    crate::expression::check_cycles(&graph).map_err(|error| {
        let path = error
            .message
            .split_once(": ")
            .and_then(|(_, chain)| chain.split(" -> ").find_map(|node| origins.get(node)))
            .map_or("lanes.vars", String::as_str);
        fail(path, error.message)
    })
}

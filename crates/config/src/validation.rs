use crate::{
    ConfigError, Extensions, InfraCommand, ProfileConfig, ProjectConfig, ReadyConfig, Run,
    RunScript, TaskWhen, Templated,
};
use indexmap::IndexMap;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};

/// A problem that does not stop loading, such as an unused template or an ignored setting.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigWarning {
    /// Dotted field path, such as `templates.api` or `tls.trust.0`.
    pub path: String,
    /// Human-readable explanation, without the path unless the sentence needs
    /// it (the daemon's `shell_fallback` warnings name it and how many more).
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
    validate_with_registry(config, &[], validator, ports)
}

/// Like [`validate_with_preset_ports`], also rejecting a `preset:` that is not in `names`
/// with the closest registered name. An empty `names` leaves that check to `validator`.
pub fn validate_with_registry(
    config: &ProjectConfig,
    names: &[&str],
    validator: &PresetValidator<'_>,
    ports: &PresetPorts<'_>,
) -> Result<CheckReport, ConfigError> {
    let report = validate_inner(config, None, Some(ports))?;
    for (name, service) in &config.services {
        let Some(preset) = &service.preset else {
            continue;
        };
        let unknown = |message: String| {
            config.sources.locate(fail(
                &format!("services.{name}.preset"),
                format!(
                    "{message}{}",
                    crate::loader::did_you_mean(preset, names.iter().copied())
                ),
            ))
        };
        if !names.is_empty() && !names.contains(&preset.as_str()) {
            return Err(unknown(format!("unknown preset `{preset}`")));
        }
        validator(preset, service.preset_options.get(preset)).map_err(|message| {
            // Registries without a name list report unknown presets through the validator.
            if message.starts_with("unknown preset") || message.starts_with("unregistered preset") {
                return unknown(message);
            }
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

/// Whether a literal `PORT` value is a usable port, as the resolver reads it.
fn literal_port(value: &crate::Scalar) -> bool {
    match value {
        crate::Scalar::Integer(port) => u16::try_from(*port).is_ok_and(|p| p > 0),
        crate::Scalar::String(text) => text.trim().parse::<u16>().is_ok_and(|p| p > 0),
        _ => false,
    }
}

/// Rejects two units whose logs devlanes would store under the same directory,
/// `logs/<project>/<lane>/<disk name>`. Services, tasks and infra setup and
/// teardown entries (units `setup.<name>` and `teardown.<name>`) share that
/// directory. The name on disk is [`devlanes_core::paths::disk_name`], which
/// replaces what Windows refuses in file names (`db:migrate` becomes
/// `db_migrate`), and Windows ignores case (`Web` and `web`). Checked on every
/// OS because one config is shared by machines running different ones.
///
/// The same name spelled the same way in two kinds (a service and a task both
/// called `web`) stays allowed: it already shared one directory before names
/// were encoded, and refusing it would break configs that work today.
fn distinct_on_disk<E: Extensions>(config: &ProjectConfig<E>) -> Result<(), ConfigError> {
    let infra = |phase: &'static str| {
        move |name: &String| (format!("infra.{phase}.{name}"), format!("{phase}.{name}"))
    };
    let units = config
        .services
        .keys()
        .map(|name| (format!("services.{name}"), name.clone()))
        .chain(
            config
                .tasks
                .keys()
                .map(|name| (format!("tasks.{name}"), name.clone())),
        )
        .chain(config.infra.setup.keys().map(infra("setup")))
        .chain(config.infra.teardown.keys().map(infra("teardown")));
    // Lowercased disk name -> (config path, unit name) of the first unit there.
    let mut seen: HashMap<String, (String, String)> = HashMap::new();
    for (path, unit) in units {
        let stored = devlanes_core::paths::disk_name(&unit);
        let key = stored.to_lowercase();
        match seen.get(&key) {
            None => {
                seen.insert(key, (path, unit));
            }
            Some((_, other)) if *other == unit => {}
            Some((other_path, other)) => {
                return Err(fail(
                    &path,
                    format!(
                        "`{other}` ({other_path}) and `{unit}` would share the log directory `{stored}` (Windows ignores case and does not allow some characters in file names, and configs are shared across operating systems); rename one of them"
                    ),
                ));
            }
        }
    }
    Ok(())
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
        if let Some(Templated::Literal(port)) = crate::env_value(&template.env, "PORT")
            && !literal_port(port)
        {
            return Err(fail(
                &format!("templates.{name}.env.PORT"),
                "must be a port number from 1 to 65535 or a `${{ ... }}` expression",
            ));
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
    distinct_on_disk(config)?;
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
                format!(
                    "unknown preset `{preset}`{}",
                    crate::loader::did_you_mean(preset, E::preset_names().iter().copied())
                ),
            ));
        }
        if let Some(Templated::Literal(port)) = crate::env_value(&service.env, "PORT")
            && !literal_port(port)
        {
            return Err(fail(
                &format!("{path}.env.PORT"),
                "must be a port number from 1 to 65535 or a `${{ ... }}` expression",
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
            && devlanes_core::StopSignal::from_config_name(signal).is_none()
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
    // Compose service names are only known once the integration resolves the Compose model.
    let can_validate_infra = config.infra.compose.is_none() || compose.is_some();
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
    // A key from any matrix row is a lane attribute; rows missing it fail when evaluated.
    let mut lane_keys: BTreeSet<String> = LANE_ATTRS.iter().map(|s| (*s).to_owned()).collect();
    for key in config.lanes.vars.keys().chain(
        config
            .lanes
            .matrix
            .iter()
            .flatten()
            .flat_map(|row| row.keys()),
    ) {
        lane_keys.insert(key.clone());
        lane_keys.insert(key.replace('-', "_"));
    }
    expression_graph(config, &lane_keys)?;
    if let Some(matrix) = &config.lanes.matrix {
        // Each matrix row is the lane's vars for one lane, so each row gets its own graph.
        for (index, row) in matrix.iter().enumerate() {
            let mut lane_config = config.clone();
            lane_config.lanes.vars = row.clone();
            lane_config.lanes.matrix = None;
            expression_graph(&lane_config, &lane_keys).map_err(|e| {
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
        if crate::env_value(&service.env, "PORT").is_none() {
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

/// Attributes of `lane` before `lanes.vars` or matrix keys are added.
const LANE_ATTRS: &[&str] = &["index", "name", "label", "path", "domain", "port"];
/// Attributes of `project`.
const PROJECT_ATTRS: &[&str] = &["name", "primary_path"];
/// Attributes of `self`, `svc.<name>` and `lanes.main.svc.<name>`.
const SERVICE_ATTRS: &[&str] = &["name", "port", "ports", "url", "public_url", "hosts"];
/// Attributes of `replica`.
const REPLICA_ATTRS: &[&str] = &["index", "count"];
/// Attributes of `infra.<container>`.
const INFRA_ATTRS: &[&str] = &["port", "ports"];
/// Expression roots; `env` and `projects` contents are only known at runtime.
const ROOTS: &[&str] = &[
    "project", "lane", "lanes", "self", "svc", "infra", "env", "projects", "replica",
];

/// Reject `<namespace>.<attr>` when `attr` is not one of `known`, naming the closest one.
fn attribute<'a>(
    path: &str,
    namespace: &str,
    attr: Option<&str>,
    known: impl IntoIterator<Item = &'a str> + Clone,
) -> Result<(), ConfigError> {
    let Some(attr) = attr else {
        return Ok(());
    };
    if known.clone().into_iter().any(|name| name == attr) {
        return Ok(());
    }
    let hint = match crate::loader::closest(attr, known.clone()) {
        Some(name) => format!("; did you mean `{namespace}.{name}`?"),
        None => format!(
            "; available: {}",
            known.into_iter().collect::<Vec<_>>().join(", ")
        ),
    };
    Err(fail(
        path,
        format!("`{namespace}.{attr}` is not defined{hint}"),
    ))
}

/// Explain `svc.my-api.url`, which MiniJinja reads as `svc.my - api.url`.
fn dashed_name_hint(text: &str) -> Option<String> {
    let chain = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '.';
    let mut quote = None;
    for (index, c) in text.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '-') => {
                let before = &text[..index];
                let after = &text[index + 1..];
                let left_start = before.rfind(|c| !chain(c)).map_or(0, |i| i + 1);
                let left = &before[left_start..];
                let right_end = after.find(|c| !chain(c)).unwrap_or(after.len());
                let right = &after[..right_end];
                let (Some((prefix, last)), Some(first)) =
                    (left.rsplit_once('.'), right.split('.').next())
                else {
                    continue;
                };
                if last.is_empty()
                    || first.is_empty()
                    || first.starts_with(|c: char| c.is_ascii_digit())
                    || !ROOTS.contains(&prefix.split('.').next().unwrap_or_default())
                {
                    continue;
                }
                let rest = &right[first.len()..];
                return Some(format!(
                    "`{left}-{right}` reads as `{left} - {right}`; write `{prefix}['{last}-{first}']{rest}` or `{prefix}.{last}_{first}{rest}` for a dashed name"
                ));
            }
            _ => {}
        }
    }
    None
}

/// Check one static reference against the names the runtime context defines.
fn check_reference<E: Extensions>(
    config: &ProjectConfig<E>,
    path: &str,
    reference: &str,
    lane_keys: &BTreeSet<String>,
) -> Result<(), ConfigError> {
    let components: Vec<&str> = reference.split('.').collect();
    let at = |i: usize| components.get(i).copied();
    let services = || config.services.keys().map(String::as_str);
    let service = |alias: &str, namespace: &str| {
        service_name(config, alias).ok_or_else(|| {
            let scope = if namespace == "svc" {
                String::new()
            } else {
                format!(" in {namespace}")
            };
            fail(
                path,
                format!(
                    "unknown service reference `{alias}`{scope}{}",
                    crate::loader::did_you_mean(alias, services())
                ),
            )
        })
    };
    match components[0] {
        "lane" => attribute(path, "lane", at(1), lane_keys.iter().map(String::as_str)),
        "project" => attribute(path, "project", at(1), PROJECT_ATTRS.iter().copied()),
        "self" => attribute(path, "self", at(1), SERVICE_ATTRS.iter().copied()),
        "replica" => attribute(path, "replica", at(1), REPLICA_ATTRS.iter().copied()),
        "svc" => match at(1) {
            Some(alias) => {
                service(alias, "svc")?;
                attribute(
                    path,
                    &format!("svc.{alias}"),
                    at(2),
                    SERVICE_ATTRS.iter().copied(),
                )
            }
            None => Ok(()),
        },
        "lanes" => {
            attribute(path, "lanes", at(1), ["main"])?;
            attribute(path, "lanes.main", at(2), ["svc"])?;
            match at(3) {
                Some(alias) => {
                    service(alias, "lanes.main")?;
                    attribute(
                        path,
                        &format!("lanes.main.svc.{alias}"),
                        at(4),
                        SERVICE_ATTRS.iter().copied(),
                    )
                }
                None => Ok(()),
            }
        }
        "infra" => {
            let Some(name) = at(1) else {
                return Ok(());
            };
            let container = config
                .infra
                .containers
                .iter()
                .find(|(key, _)| *key == name || key.replace('-', "_") == name);
            if container.is_none() && config.infra.compose.is_none() {
                return Err(fail(
                    path,
                    format!(
                        "unknown container `{name}`; infra.containers does not declare it and there is no compose file{}",
                        crate::loader::did_you_mean(
                            name,
                            config.infra.containers.keys().map(String::as_str)
                        )
                    ),
                ));
            }
            attribute(
                path,
                &format!("infra.{name}"),
                at(2),
                INFRA_ATTRS.iter().copied(),
            )?;
            // Compose ports are keyed by container port; only declared containers have names.
            if let (Some((_, container)), Some("ports")) = (container, at(2)) {
                attribute(
                    path,
                    &format!("infra.{name}.ports"),
                    at(3),
                    container.ports.keys().map(String::as_str),
                )?;
            }
            Ok(())
        }
        "env" | "projects" => Ok(()),
        root => {
            let hint = match crate::loader::closest(root, ROOTS.iter().copied()) {
                Some(name) => format!("did you mean `{name}`?"),
                None => "use underscores or brackets for dashed names".to_owned(),
            };
            Err(fail(
                path,
                format!("unknown expression context `{root}`; {hint}"),
            ))
        }
    }
}

/// Build dependency edges from MiniJinja's static references, including PORT derivation.
fn expression_graph<E: Extensions>(
    config: &ProjectConfig<E>,
    lane_keys: &BTreeSet<String>,
) -> Result<(), ConfigError> {
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
            check_reference(config, &path, reference, lane_keys).map_err(|error| {
                match dashed_name_hint(&text) {
                    Some(hint) => fail(&path, hint),
                    None => error,
                }
            })?;
            let mut components = reference.split('.');
            let root = components.next().unwrap_or_default();
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
                "svc" => components
                    .next()
                    .and_then(|alias| service_name(config, alias)),
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

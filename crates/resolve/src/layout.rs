use std::collections::{BTreeMap, BTreeSet};

use devlanes_config::{
    ExpressionContext, Extensions, HostConfig, HostHeader, ProjectConfig, Scalar, ServiceConfig,
    Templated,
};
use devlanes_core::{color::unit_color, ports::PortName};
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::{
    LANE_DATA_ENV, LaneFacts, LaneLayout, ResolveError, ResolveInput, RouteSpec, ServiceLayout,
    model::services_namespace,
    ports::{
        DEFAULT_PORT, PortOverride, port_override, port_unit, service_port_names, static_replicas,
    },
    util::{expression_strings, public_url_env_key},
};

/// Expression namespaces shared by every unit of a lane.
pub(crate) struct Namespaces {
    pub project: Value,
    pub lane: Value,
    pub lanes: Value,
    pub services: Value,
    pub infra: Value,
    pub projects: Value,
}

impl Namespaces {
    /// Builds the context for one unit. `this` is `self`, `env` the `${{ env.X }}` map.
    pub(crate) fn context(&self, env: &Value, this: Value, replica: Value) -> ExpressionContext {
        ExpressionContext {
            project: self.project.clone(),
            lane: self.lane.clone(),
            lanes: self.lanes.clone(),
            service: this,
            services: self.services.clone(),
            infra: self.infra.clone(),
            env: env.clone(),
            projects: self.projects.clone(),
            replica,
        }
    }

    /// Context without `self`, for lane-level values such as `global_env`.
    pub(crate) fn lane_context(&self, env: &Value) -> ExpressionContext {
        self.context(env, json!({}), json!({}))
    }
}

/// `${{ env.X }}` lookup order is client env, then secrets, then `.env`, so the
/// map is built lowest first. Names go through `fold_name`, so on Windows a
/// `.env` file's `Path` and the client's `PATH` are one entry.
pub(crate) fn env_namespace(
    client: &BTreeMap<String, String>,
    secrets: Option<&BTreeMap<String, String>>,
    dotenv: &BTreeMap<String, String>,
) -> Value {
    let mut map = serde_json::Map::new();
    for source in [Some(dotenv), secrets, Some(client)].into_iter().flatten() {
        for (key, value) in source {
            map.insert(
                devlanes_core::env::fold_name(key.clone()),
                value.clone().into(),
            );
        }
    }
    Value::Object(map)
}

/// Treats a missing namespace as an empty object.
fn object_or_empty(value: &Value) -> Value {
    if value.is_null() {
        json!({})
    } else {
        value.clone()
    }
}

/// Default `lane.domain`: `<project>.localhost` for lane 0, else `<project>-<index>.localhost`.
fn default_domain(project: &str, index: u32) -> String {
    if index == 0 {
        format!("{project}.localhost")
    } else {
        format!("{project}-{index}.localhost")
    }
}

/// `project` and `lane` namespaces before `lanes.vars` are applied.
fn base_namespaces(project: &str, facts: &LaneFacts, port: Option<u16>) -> (Value, Value) {
    let project_ns = json!({
        "name": project,
        "primary_path": facts.primary_path.display().to_string(),
    });
    let mut lane = json!({
        "index": facts.index,
        "name": facts.name,
        "label": facts.label,
        "path": facts.path.display().to_string(),
        // `/` on Windows too, so the path survives interpolation into `sh` strings.
        "data_path": devlanes_core::paths::forward_slashes(&facts.data_path),
        "domain": default_domain(project, facts.index),
    });
    if let (Some(port), Some(map)) = (port, lane.as_object_mut()) {
        map.insert("port".into(), port.into());
    }
    (project_ns, lane)
}

/// Evaluates `lanes.vars.port` (or the matrix row's `port`) so the daemon can
/// place the lane's block itself. Returns `None` when the config leaves block
/// placement to the pool. `lane.port` is undefined during this evaluation.
pub fn fixed_block_start<E: Extensions>(
    config: &ProjectConfig<E>,
    facts: &LaneFacts,
    client_env: &BTreeMap<String, String>,
    dotenv: &BTreeMap<String, String>,
) -> Result<Option<u16>, ResolveError> {
    let declares_port = config.lanes.vars.contains_key("port")
        || config.lanes.matrix.as_ref().is_some_and(|rows| {
            usize::try_from(facts.index)
                .ok()
                .and_then(|i| rows.get(i))
                .is_some_and(|row| row.contains_key("port"))
        });
    if !declares_port {
        return Ok(None);
    }
    let (project, lane) = base_namespaces(&config.project, facts, None);
    let context = ExpressionContext {
        project,
        lane,
        env: env_namespace(client_env, None, dotenv),
        ..ExpressionContext::default()
    };
    let vars = devlanes_config::resolve_lane_vars(&config.lanes, &context)
        .map_err(|e| ResolveError::expression("lanes.vars", e).locate(config))?;
    match vars.get("port") {
        Some(Scalar::Integer(port)) => u16::try_from(*port)
            .ok()
            .filter(|p| *p > 0)
            .map(Some)
            .ok_or_else(|| ResolveError::invalid("lanes.vars.port", "must be a port number")),
        Some(_) => Err(ResolveError::invalid(
            "lanes.vars.port",
            "must evaluate to a number",
        )),
        None => Ok(None),
    }
}

/// Computes ports, URLs and routes for a lane without contributors or secrets.
/// `${{ env.X }}` sees the client env and `.env` only.
pub fn lane_layout<E: Extensions>(input: &ResolveInput<'_, E>) -> Result<LaneLayout, ResolveError> {
    let env = env_namespace(input.client_env, None, input.dotenv);
    build(input, &env)
        .map(|(layout, _)| layout)
        .map_err(|error| error.locate(input.config))
}

/// Static per-service facts needed before evaluation.
struct Pending<'c, E: Extensions> {
    service: &'c ServiceConfig<E>,
    replicas: u16,
    names: BTreeSet<String>,
    port: PortOverride,
    /// Same for every replica: color follows tags, preset and name only.
    color: &'static str,
}

/// Builds the layout and the namespaces (with a complete `services`) for launch planning.
pub(crate) fn build<E: Extensions>(
    input: &ResolveInput<'_, E>,
    env: &Value,
) -> Result<(LaneLayout, Namespaces), ResolveError> {
    let config = input.config;
    let facts = input.facts;
    let (project, lane) = base_namespaces(&config.project, facts, Some(input.block.start));
    let mut ns = Namespaces {
        project,
        lane,
        lanes: json!({"main": {"services": input.lanes_main.cloned().unwrap_or_else(|| json!({}))}}),
        services: json!({}),
        infra: object_or_empty(input.infra),
        projects: object_or_empty(input.projects),
    };
    let vars = devlanes_config::resolve_lane_vars(&config.lanes, &ns.lane_context(env))
        .map_err(|e| ResolveError::expression("lanes.vars", e))?;
    if let Some(lane) = ns.lane.as_object_mut() {
        for (key, value) in &vars {
            let value = serde_json::to_value(value)
                .map_err(|e| ResolveError::invalid(format!("lanes.vars.{key}"), e.to_string()))?;
            lane.insert(key.clone(), value);
        }
    }
    let domain = ns.lane["domain"]
        .as_str()
        .ok_or_else(|| ResolveError::invalid("lanes.vars.domain", "must be a string"))?
        .to_owned();
    let port = ns.lane["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| ResolveError::invalid("lanes.vars.port", "must be a port number"))?;
    let mut warnings = Vec::new();
    if port != input.block.start {
        warnings.push(format!(
            "lane.port {port} from lanes.vars differs from the allocated block start {}",
            input.block.start
        ));
    }

    let colors = tag_colors(config);
    let mut pending = IndexMap::new();
    for (name, service) in &config.services {
        pending.insert(
            name.as_str(),
            Pending {
                service,
                replicas: static_replicas(name, service)?,
                names: service_port_names(name, service, input.contributors)?,
                port: port_override(name, service)?,
                color: unit_color(
                    service.tags.as_deref().unwrap_or_default(),
                    name,
                    service.preset.as_deref(),
                    &colors,
                ),
            },
        );
    }
    let order = identity_order(config)?;
    let mut layouts: BTreeMap<String, Vec<ServiceLayout>> = BTreeMap::new();
    for name in order {
        let Some(entry) = pending.get(name.as_str()) else {
            continue;
        };
        let mut replicas = Vec::new();
        for replica in 1..=entry.replicas {
            replicas.push(service_layout(input, &ns, env, &name, entry, replica)?);
        }
        if let (Some(first), Some(namespace)) = (replicas.first(), ns.services.as_object_mut()) {
            namespace.insert(name.clone(), first.self_value());
        }
        layouts.insert(name, replicas);
    }
    let services: IndexMap<String, ServiceLayout> = config
        .services
        .keys()
        .filter_map(|name| layouts.remove(name))
        .flatten()
        .map(|layout| (layout.unit.clone(), layout))
        .collect();
    if input.lanes_main.is_none()
        && facts.index == 0
        && let Some(main) = ns.lanes.get_mut("main")
    {
        main["services"] = services_namespace(services.values());
    }

    let mut routes: Vec<RouteSpec> = Vec::new();
    for (name, entry) in &pending {
        let hosts = entry.service.hosts.as_deref().unwrap_or_default();
        for layout in services.values().filter(|s| s.name == *name) {
            let context = ns.context(env, layout.self_value(), replica_value(layout));
            for host in hosts {
                let route = route_spec(name, layout, host, &context)?;
                if routes.iter().any(|r| r.host == route.host) {
                    warnings.push(format!(
                        "host `{}` of {} is already routed; keeping the first",
                        route.host, route.unit
                    ));
                    continue;
                }
                routes.push(route);
            }
        }
    }

    let mut origins = Vec::new();
    for layout in services.values() {
        let local = [
            format!("http://localhost:{}", layout.port),
            format!("http://127.0.0.1:{}", layout.port),
        ];
        let hosts = layout
            .hosts
            .iter()
            .filter(|h| !h.contains('*'))
            .map(|h| https_url(h, input.public_https_port));
        for origin in hosts.chain(local) {
            if !origins.contains(&origin) {
                origins.push(origin);
            }
        }
    }

    let mut lane_env = BTreeMap::from([
        ("DEVLANES".to_owned(), "1".to_owned()),
        ("DEVLANES_PROJECT".to_owned(), config.project.clone()),
        ("DEVLANES_LANE".to_owned(), facts.index.to_string()),
        ("DEVLANES_LANE_NAME".to_owned(), facts.name.clone()),
        (
            LANE_DATA_ENV.to_owned(),
            devlanes_core::paths::forward_slashes(&facts.data_path),
        ),
    ]);
    // Services without hosts get their internal URL so tests always find a base URL.
    for layout in services.values().filter(|s| s.replica == 1) {
        lane_env.insert(
            public_url_env_key(&layout.name),
            layout
                .public_url
                .clone()
                .unwrap_or_else(|| layout.url.clone()),
        );
    }
    lane_env.extend(input.ca_env.iter().map(|(k, v)| (k.clone(), v.clone())));

    let layout = LaneLayout {
        domain,
        port,
        vars,
        services,
        routes,
        origins,
        warnings,
        lane_env,
    };
    Ok((layout, ns))
}

/// Public URL of `host` through the proxy: `https://host` on 443 (or when the
/// port is unknown), `https://host:<port>` otherwise. Browsers send that same
/// form as `Origin`, so origins use it too.
pub fn https_url(host: &str, port: Option<u16>) -> String {
    match port {
        Some(port) if port != 443 => format!("https://{host}:{port}"),
        _ => format!("https://{host}"),
    }
}

/// `replica` namespace: `{index, count}`.
pub(crate) fn replica_value(layout: &ServiceLayout) -> Value {
    json!({"index": layout.replica, "count": layout.replicas})
}

/// Ports, URLs and hosts of one replica, evaluated with the services before it.
fn service_layout<E: Extensions>(
    input: &ResolveInput<'_, E>,
    ns: &Namespaces,
    env: &Value,
    name: &str,
    entry: &Pending<'_, E>,
    replica: u16,
) -> Result<ServiceLayout, ResolveError> {
    let unit = if entry.replicas > 1 {
        format!("{name}.{replica}")
    } else {
        name.to_owned()
    };
    let owner = port_unit(name, replica);
    let lookup = |port: &str| -> Result<u16, ResolveError> {
        let key = PortName::new(owner.clone(), port);
        let offset = input
            .offsets
            .get(&key)
            .ok_or_else(|| ResolveError::MissingOffset {
                unit: unit.clone(),
                name: port.to_owned(),
            })?;
        input.block.port(*offset).map_err(|e| ResolveError::Port {
            unit: unit.clone(),
            message: format!("port `{port}`: {e}"),
        })
    };
    let mut ports = BTreeMap::new();
    for port in entry.names.iter().filter(|p| *p != DEFAULT_PORT) {
        ports.insert(port.clone(), lookup(port)?);
    }
    let replica_ns = json!({"index": replica, "count": entry.replicas});
    let partial = json!({"name": name, "ports": ports});
    let path = format!("services.{name}");
    let (port, literal_port) = match entry.port {
        PortOverride::Literal(port) => (port, true),
        PortOverride::None => (lookup(DEFAULT_PORT)?, false),
        PortOverride::Expression => {
            let context = ns.context(env, partial.clone(), replica_ns.clone());
            let value = entry.service.env["PORT"]
                .evaluate(&context)
                .map_err(|e| ResolveError::expression(format!("{path}.env.PORT"), e))?;
            let port = match &value {
                Scalar::Integer(n) => u16::try_from(*n).ok(),
                Scalar::String(s) => s.trim().parse().ok(),
                _ => None,
            }
            .filter(|p| *p > 0)
            .ok_or_else(|| {
                ResolveError::invalid(format!("{path}.env.PORT"), "must evaluate to a port number")
            })?;
            (port, false)
        }
    };
    let url = format!("http://127.0.0.1:{port}");
    let mut this = partial;
    this["port"] = port.into();
    this["url"] = url.clone().into();
    let context = ns.context(env, this, replica_ns);
    let mut hosts = Vec::new();
    for host in entry.service.hosts.iter().flatten() {
        let template = match host {
            HostConfig::Name(name) => name,
            HostConfig::Route(route) => &route.host,
        };
        let host = template
            .evaluate(&context)
            .map_err(|e| ResolveError::expression(format!("{path}.hosts"), e))?;
        hosts.push(
            devlanes_config::normalize_host(&host)
                .map_err(|message| ResolveError::invalid(format!("{path}.hosts"), message))?,
        );
    }
    let public_url = hosts
        .iter()
        .find(|h| !h.contains('*'))
        .map(|h| https_url(h, input.public_https_port));
    let flag = |value: &Option<Templated<bool>>, key: &str, default: bool| {
        value.as_ref().map_or(Ok(default), |v| {
            v.evaluate(&context)
                .map_err(|e| ResolveError::expression(format!("{path}.{key}"), e))
        })
    };
    Ok(ServiceLayout {
        name: name.to_owned(),
        unit,
        replica,
        replicas: entry.replicas,
        port,
        ports,
        url,
        public_url,
        hosts,
        literal_port,
        hidden: flag(&entry.service.hidden, "hidden", false)?,
        autostart: flag(&entry.service.autostart, "autostart", true)?,
        tags: entry.service.tags.clone().unwrap_or_default(),
        color: entry.color.to_owned(),
        display_name: entry.service.name.as_ref().map(|display| {
            if entry.replicas > 1 {
                format!("{display} #{replica}")
            } else {
                display.clone()
            }
        }),
    })
}

/// Tag colors configured under the top-level `tags:` key, as palette names, for
/// [`devlanes_core::color::unit_color`]. The daemon uses the same map for exec
/// units and containers.
pub fn tag_colors<E: Extensions>(config: &ProjectConfig<E>) -> BTreeMap<String, String> {
    config
        .tags
        .iter()
        .filter_map(|(tag, settings)| Some((tag.clone(), settings.color?.as_str().to_owned())))
        .collect()
}

/// Evaluates one host entry into a route.
fn route_spec(
    name: &str,
    layout: &ServiceLayout,
    host: &HostConfig,
    context: &ExpressionContext,
) -> Result<RouteSpec, ResolveError> {
    let path = format!("services.{name}.hosts");
    let evaluate = |t: &Templated<String>| {
        let host = t
            .evaluate(context)
            .map_err(|e| ResolveError::expression(path.clone(), e))?;
        devlanes_config::normalize_host(&host)
            .map_err(|message| ResolveError::invalid(path.clone(), message))
    };
    let mut route = RouteSpec {
        host: String::new(),
        service: name.to_owned(),
        unit: layout.unit.clone(),
        port: layout.port,
        port_name: DEFAULT_PORT.to_owned(),
        fallback: false,
        host_header_upstream: false,
    };
    match host {
        HostConfig::Name(host) => route.host = evaluate(host)?,
        HostConfig::Route(spec) => {
            route.host = evaluate(&spec.host)?;
            if let Some(port) = spec.port.as_deref().filter(|p| *p != DEFAULT_PORT) {
                route.port =
                    *layout
                        .ports
                        .get(port)
                        .ok_or_else(|| ResolveError::MissingOffset {
                            unit: layout.unit.clone(),
                            name: port.to_owned(),
                        })?;
                route.port_name = port.to_owned();
            }
            if let Some(fallback) = &spec.fallback {
                route.fallback = fallback
                    .evaluate(context)
                    .map_err(|e| ResolveError::expression(path.clone(), e))?;
            }
            route.host_header_upstream = matches!(spec.host_header, Some(HostHeader::Upstream));
        }
    }
    Ok(route)
}

/// Orders services so each one's ports and hosts are evaluated after the
/// services those values reference. Only `hosts` and `env.PORT` count: other
/// fields are evaluated once every port and host is known.
fn identity_order<E: Extensions>(config: &ProjectConfig<E>) -> Result<Vec<String>, ResolveError> {
    let mut graph: IndexMap<String, BTreeSet<String>> = IndexMap::new();
    for (name, service) in &config.services {
        let mut texts = Vec::new();
        if let Some(hosts) = &service.hosts {
            let value = serde_json::to_value(hosts).map_err(|e| {
                ResolveError::invalid(format!("services.{name}.hosts"), e.to_string())
            })?;
            expression_strings(&value, &mut texts);
        }
        if let Some(Templated::Expression(text)) = service.env.get("PORT") {
            texts.push(text.clone());
        }
        let mut edges = BTreeSet::new();
        for text in texts {
            let references = devlanes_config::references(&text)
                .map_err(|e| ResolveError::expression(format!("services.{name}"), e))?;
            for reference in references {
                let Some(alias) = reference
                    .strip_prefix("services.")
                    .and_then(|r| r.split('.').next())
                else {
                    continue;
                };
                if let Some(target) = config
                    .services
                    .keys()
                    .find(|n| n.as_str() == alias || n.replace('-', "_") == alias)
                    .filter(|n| *n != name)
                {
                    edges.insert(target.clone());
                }
            }
        }
        graph.insert(name.clone(), edges);
    }
    let mut order = Vec::new();
    let mut done = BTreeSet::new();
    for name in graph.keys() {
        visit(name, &graph, &mut Vec::new(), &mut done, &mut order)?;
    }
    Ok(order)
}

/// Depth-first post-order with an explicit stack for cycle reporting.
fn visit(
    node: &str,
    graph: &IndexMap<String, BTreeSet<String>>,
    active: &mut Vec<String>,
    done: &mut BTreeSet<String>,
    order: &mut Vec<String>,
) -> Result<(), ResolveError> {
    if done.contains(node) {
        return Ok(());
    }
    if let Some(start) = active.iter().position(|n| n == node) {
        let mut chain = active[start..].to_vec();
        chain.push(node.to_owned());
        return Err(ResolveError::Cycle { chain });
    }
    active.push(node.to_owned());
    for edge in graph.get(node).into_iter().flatten() {
        visit(edge, graph, active, done, order)?;
    }
    active.pop();
    done.insert(node.to_owned());
    order.push(node.to_owned());
    Ok(())
}

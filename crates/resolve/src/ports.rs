use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use devlanes_config::{
    Extensions, HostConfig, ProjectConfig, ReadyConfig, Scalar, ServiceConfig, TcpReady, Templated,
};
use devlanes_core::{
    contrib::{Contributor, ServiceView},
    ports::PortName,
};

use crate::{ResolveError, util::expression_strings};

/// Name of the primary port (`self.port`) in [`PortName::name`].
pub(crate) const DEFAULT_PORT: &str = "default";

/// Ports a project needs in every lane's block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortNeeds {
    /// Allocated names, one per (unit, port name). Replica N>1 of a service uses
    /// unit `<service>.<N>` (see [`port_unit`]).
    pub names: BTreeSet<PortName>,
    /// Offsets used by explicit `lane.port + N` expressions; never allocated to names.
    pub reserved: BTreeSet<u16>,
    /// Services whose `env.PORT` is a literal: same port in every lane, no offset.
    pub literal_ports: BTreeMap<String, u16>,
    /// Services whose `env.PORT` is an expression: no offset either.
    pub derived_ports: BTreeSet<String>,
}

impl PortNeeds {
    /// Count of offsets the block must hold: allocated names plus reservations.
    /// Pass it to `devlanes_core::ports::block_size`.
    pub fn needed(&self) -> usize {
        self.names.len() + self.reserved.len()
    }

    /// Smallest block that fits [`Self::needed`] and the highest reserved offset.
    pub fn min_block_size(&self) -> usize {
        let highest = self.reserved.last().map_or(0, |o| usize::from(*o) + 1);
        self.needed().max(highest)
    }
}

/// Unit key used for a replica's port names: the service name for replica 1, so
/// raising `replicas` never moves the first instance, and `<service>.<N>` after.
pub fn port_unit(service: &str, replica: u16) -> String {
    if replica <= 1 {
        service.to_owned()
    } else {
        format!("{service}.{replica}")
    }
}

/// Computes the port names to allocate, the reserved offsets and the services
/// with their own `PORT`. Infra containers are not included; their integrations
/// request their own names.
///
/// `contributors` add named ports through [`Contributor::required_ports`];
/// presets declared by the config's [`Extensions`] are included too.
pub fn port_needs<E: Extensions>(
    config: &ProjectConfig<E>,
    contributors: &[Arc<dyn Contributor>],
) -> Result<PortNeeds, ResolveError> {
    let mut needs = PortNeeds::default();
    for (name, service) in &config.services {
        match port_override(name, service).map_err(|error| error.locate(config))? {
            PortOverride::Literal(port) => {
                needs.literal_ports.insert(name.clone(), port);
            }
            PortOverride::Expression => {
                needs.derived_ports.insert(name.clone());
            }
            PortOverride::None => {}
        }
        let replicas = static_replicas(name, service)?;
        let names = service_port_names(name, service, contributors)?;
        for replica in 1..=replicas {
            let unit = port_unit(name, replica);
            for port in &names {
                needs
                    .names
                    .insert(PortName::new(unit.clone(), port.clone()));
            }
        }
    }
    let mut value =
        serde_json::to_value(config).map_err(|e| ResolveError::invalid("config", e.to_string()))?;
    // Templates are already merged into services; unused ones reserve nothing.
    if let Some(map) = value.as_object_mut() {
        map.remove("templates");
    }
    let mut texts = Vec::new();
    expression_strings(&value, &mut texts);
    for text in texts {
        needs.reserved.extend(reserved_offsets(&text));
    }
    Ok(needs)
}

/// How a service's `env.PORT` affects `self.port`.
pub(crate) enum PortOverride {
    None,
    Literal(u16),
    Expression,
}

/// Classifies `env.PORT`; a literal must be a valid port number.
pub(crate) fn port_override<E: Extensions>(
    name: &str,
    service: &ServiceConfig<E>,
) -> Result<PortOverride, ResolveError> {
    let invalid = || {
        ResolveError::invalid(
            format!("services.{name}.env.PORT"),
            "must be a port number between 1 and 65535",
        )
    };
    match devlanes_config::env_value(&service.env, "PORT") {
        None => Ok(PortOverride::None),
        Some(Templated::Expression(_)) => Ok(PortOverride::Expression),
        Some(Templated::Literal(Scalar::Integer(port))) => u16::try_from(*port)
            .ok()
            .filter(|p| *p > 0)
            .map(PortOverride::Literal)
            .ok_or_else(invalid),
        Some(Templated::Literal(Scalar::String(text))) => text
            .trim()
            .parse::<u16>()
            .ok()
            .filter(|p| *p > 0)
            .map(PortOverride::Literal)
            .ok_or_else(invalid),
        Some(_) => Err(invalid()),
    }
}

/// Reads `replicas` without lane context; port allocation is lane-independent.
pub(crate) fn static_replicas<E: Extensions>(
    name: &str,
    service: &ServiceConfig<E>,
) -> Result<u16, ResolveError> {
    match &service.replicas {
        None => Ok(1),
        Some(Templated::Literal(count)) if *count > 0 => Ok(*count),
        Some(Templated::Literal(_)) => Err(ResolveError::invalid(
            format!("services.{name}.replicas"),
            "must be positive",
        )),
        Some(Templated::Expression(_)) => Err(ResolveError::invalid(
            format!("services.{name}.replicas"),
            "must be a literal number: every lane allocates the same ports",
        )),
    }
}

/// Port names one replica of `service` needs, including `default` unless
/// `env.PORT` sets the port.
pub(crate) fn service_port_names<E: Extensions>(
    name: &str,
    service: &ServiceConfig<E>,
    contributors: &[Arc<dyn Contributor>],
) -> Result<BTreeSet<String>, ResolveError> {
    let mut names = BTreeSet::new();
    let sets_port = devlanes_config::env_value(&service.env, "PORT").is_some();
    if !sets_port {
        names.insert(DEFAULT_PORT.to_owned());
    }
    if let Some(preset) = &service.preset {
        names.extend(E::required_ports(preset).iter().map(|p| (*p).to_owned()));
    }
    let tags = service.tags.as_deref().unwrap_or_default();
    let view = ServiceView {
        name,
        platform: service.platform.as_deref(),
        preset: service.preset.as_deref(),
        tags,
    };
    for contributor in contributors {
        names.extend(contributor.required_ports(&view));
    }
    for host in service.hosts.iter().flatten() {
        if let HostConfig::Route(route) = host
            && let Some(port) = &route.port
        {
            names.insert(port.clone());
        }
    }
    if let Some(ReadyConfig::Check(check)) = &service.ready
        && let Some(TcpReady::Port(port)) = &check.tcp
    {
        names.insert(port.clone());
    }
    let value = serde_json::to_value(service)
        .map_err(|e| ResolveError::invalid(format!("services.{name}"), e.to_string()))?;
    let mut texts = Vec::new();
    expression_strings(&value, &mut texts);
    for text in texts {
        let references = devlanes_config::references(&text)
            .map_err(|e| ResolveError::expression(format!("services.{name}"), e))?;
        for reference in references {
            if let Some(port) = reference
                .strip_prefix("self.ports.")
                .and_then(|rest| rest.split('.').next())
            {
                names.insert(port.to_owned());
            }
        }
    }
    // `self.port` already covers it; a literal PORT means there is nothing to allocate.
    if sets_port {
        names.remove(DEFAULT_PORT);
    }
    Ok(names)
}

/// Finds offsets used by `lane.port`, `lane.port + N` and `N + lane.port` in one
/// expression string. Static analysis: computed offsets (`lane.port + x * 2`)
/// are not detected.
///
/// ```
/// let offsets = devlanes_resolve::reserved_offsets("${{ lane.port + 5 }}:${{ lane.port }}");
/// assert_eq!(offsets.into_iter().collect::<Vec<_>>(), [0, 5]);
/// ```
pub fn reserved_offsets(text: &str) -> BTreeSet<u16> {
    const NEEDLE: &str = "lane.port";
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let bytes = compact.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b == b'.';
    let mut offsets = BTreeSet::new();
    let mut from = 0;
    while let Some(found) = compact[from..].find(NEEDLE) {
        let start = from + found;
        let end = start + NEEDLE.len();
        from = end;
        if start > 0 && ident(bytes[start - 1]) || bytes.get(end).copied().is_some_and(ident) {
            continue;
        }
        let digits_after: String = compact[end..]
            .strip_prefix('+')
            .map(|rest| rest.chars().take_while(char::is_ascii_digit).collect())
            .unwrap_or_default();
        let digits_before: String = compact[..start]
            .strip_suffix('+')
            .map(|rest| {
                let count = rest.chars().rev().take_while(char::is_ascii_digit).count();
                rest[rest.len() - count..].to_owned()
            })
            .unwrap_or_default();
        let offset = if !digits_after.is_empty() {
            digits_after.parse().ok()
        } else if !digits_before.is_empty() {
            digits_before.parse().ok()
        } else if compact[end..].starts_with('-') {
            // Below the block start: not an offset inside the block.
            None
        } else {
            Some(0)
        };
        if let Some(offset) = offset {
            offsets.insert(offset);
        }
    }
    offsets
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Each spelling of explicit block math reserves its offset; look-alikes do not.
    fn reserved_offset_forms() {
        let cases: &[(&str, &[u16])] = &[
            ("${{ lane.port + 5 }}", &[5]),
            ("${{lane.port+12}}", &[12]),
            ("${{ 3 + lane.port }}", &[3]),
            ("http://x:${{ lane.port }}/", &[0]),
            ("${{ lane.port - 1 }}", &[]),
            ("${{ lane.ports }}", &[]),
            ("${{ other_lane.port + 2 }}", &[]),
            ("${{ lane.port + 1 }} ${{ lane.port + 2 }}", &[1, 2]),
        ];
        for (text, expected) in cases {
            assert_eq!(
                reserved_offsets(text).into_iter().collect::<Vec<_>>(),
                *expected,
                "{text}"
            );
        }
    }

    #[test]
    /// Replica 1 keeps the plain service name so adding replicas never moves it.
    fn replica_units() {
        assert_eq!(port_unit("desktop", 1), "desktop");
        assert_eq!(port_unit("desktop", 2), "desktop.2");
    }
}

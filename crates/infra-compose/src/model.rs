use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Deserializer};

use crate::Error;

/// The parts of `docker compose config --format json` devlanes reads. Unknown keys are
/// ignored so newer Compose releases keep parsing.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ComposeModel {
    pub name: String,
    #[serde(default)]
    pub services: BTreeMap<String, ServiceModel>,
    #[serde(default)]
    pub volumes: BTreeMap<String, VolumeModel>,
}

/// One resolved service. Compose has already applied interpolation, `extends`, `include`
/// and profile filtering.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct ServiceModel {
    pub image: Option<String>,
    pub container_name: Option<String>,
    #[serde(default)]
    pub ports: Vec<PortModel>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub depends_on: BTreeMap<String, serde_json::Value>,
    pub healthcheck: Option<serde_json::Value>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    #[serde(default)]
    pub volumes: Vec<ServiceVolume>,
}

/// A port in Compose's long syntax. `published` is `None` for ephemeral host ports and may
/// be a range such as `9200-9201`.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PortModel {
    pub target: u16,
    #[serde(default, deserialize_with = "string_or_number")]
    pub published: Option<String>,
    pub host_ip: Option<String>,
    #[serde(default = "tcp")]
    pub protocol: String,
}

/// A service mount. Only named volumes matter for lane isolation checks.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ServiceVolume {
    #[serde(rename = "type")]
    pub kind: String,
    pub source: Option<String>,
    pub target: String,
}

/// A top-level volume. Compose fills `name` with `<project>_<key>` unless the file sets one.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct VolumeModel {
    pub name: Option<String>,
    #[serde(default)]
    pub external: Option<serde_json::Value>,
}

impl VolumeModel {
    /// External volumes are shared by every project that names them.
    pub fn is_external(&self) -> bool {
        match &self.external {
            Some(serde_json::Value::Bool(external)) => *external,
            Some(serde_json::Value::Object(_)) => true,
            _ => false,
        }
    }
}

impl ComposeModel {
    /// Parses the JSON printed by `docker compose config --format json`.
    pub fn from_json(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Fails for names that are absent from the model, so typos in `exclude`/`per_lane`
    /// and profile mismatches surface before anything starts.
    pub fn require_services<'a>(
        &self,
        names: impl IntoIterator<Item = &'a String>,
    ) -> Result<(), Error> {
        for name in names {
            if !self.services.contains_key(name) {
                return Err(Error::UnknownService(name.clone()));
            }
        }
        Ok(())
    }

    /// Services the shared project starts: everything resolved except `exclude` and
    /// `per_lane`. Fails when a started service depends on a skipped one, because Compose
    /// would start the dependency anyway.
    pub fn shared_services(
        &self,
        exclude: &BTreeSet<String>,
        per_lane: &BTreeSet<String>,
    ) -> Result<Vec<String>, Error> {
        self.require_services(exclude)?;
        self.require_services(per_lane)?;
        let selected: Vec<String> = self
            .services
            .keys()
            .filter(|name| !exclude.contains(*name) && !per_lane.contains(*name))
            .cloned()
            .collect();
        for name in &selected {
            let Some(service) = self.services.get(name) else {
                continue;
            };
            for dependency in service.depends_on.keys() {
                if exclude.contains(dependency) || per_lane.contains(dependency) {
                    return Err(Error::ExcludedDependency {
                        service: name.clone(),
                        dependency: dependency.clone(),
                    });
                }
            }
        }
        Ok(selected)
    }

    /// Named volumes of `services` that a lane project would share with other projects,
    /// because they are external or have an explicit `name:` outside the project prefix.
    pub fn shared_volumes(&self, services: &[String]) -> Vec<String> {
        let prefix = format!("{}_", self.name);
        let mut shared = BTreeSet::new();
        for service in services.iter().filter_map(|name| self.services.get(name)) {
            for mount in service
                .volumes
                .iter()
                .filter(|mount| mount.kind == "volume")
            {
                let Some(source) = &mount.source else {
                    continue;
                };
                let Some(volume) = self.volumes.get(source) else {
                    continue;
                };
                let named_outside = volume
                    .name
                    .as_ref()
                    .is_some_and(|name| !name.starts_with(&prefix));
                if volume.is_external() || named_outside {
                    shared.insert(source.clone());
                }
            }
        }
        shared.into_iter().collect()
    }
}

/// Container health as reported by `docker compose ps`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum Health {
    /// Running without a healthcheck.
    Running,
    /// Running and the healthcheck has not passed yet.
    Starting,
    Healthy,
    Unhealthy,
    Exited {
        code: i64,
    },
    /// Created, paused, restarting or another transitional state.
    Other {
        state: String,
    },
    /// The service was selected but no container exists.
    Missing,
}

/// One row of `docker compose ps --all --format json`.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct ContainerStatus {
    #[serde(rename = "ID")]
    pub id: String,
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Service")]
    pub service: String,
    #[serde(rename = "Project", default)]
    pub project: String,
    #[serde(rename = "State")]
    pub state: String,
    #[serde(rename = "Health", default)]
    pub health: String,
    #[serde(rename = "ExitCode", default)]
    pub exit_code: i64,
    #[serde(rename = "Publishers", default, deserialize_with = "null_as_empty")]
    pub publishers: Vec<Publisher>,
}

/// A published port of a running container. Docker lists IPv4 and IPv6 bindings separately.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Publisher {
    #[serde(rename = "URL", default)]
    pub url: String,
    #[serde(rename = "TargetPort")]
    pub target_port: u16,
    #[serde(rename = "PublishedPort")]
    pub published_port: u16,
    #[serde(rename = "Protocol", default = "tcp")]
    pub protocol: String,
}

impl ContainerStatus {
    /// Combines the container state and healthcheck status into one value.
    pub fn health(&self) -> Health {
        match self.state.as_str() {
            "running" => match self.health.as_str() {
                "healthy" => Health::Healthy,
                "unhealthy" => Health::Unhealthy,
                "starting" => Health::Starting,
                _ => Health::Running,
            },
            "exited" | "dead" => Health::Exited {
                code: self.exit_code,
            },
            other => Health::Other {
                state: other.to_owned(),
            },
        }
    }
}

/// Parses `ps --format json` output. Compose 2.21+ prints one object per line; older
/// releases print a single array.
pub fn parse_ps(output: &str) -> Result<Vec<ContainerStatus>, serde_json::Error> {
    let trimmed = output.trim();
    if trimmed.starts_with('[') {
        return serde_json::from_str(trimmed);
    }
    trimmed
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(serde_json::from_str)
        .collect()
}

/// Health per selected service; services without a container are `Missing`. With several
/// replicas the least healthy one wins.
pub fn service_health(
    services: &[String],
    statuses: &[ContainerStatus],
) -> BTreeMap<String, Health> {
    services
        .iter()
        .map(|service| {
            let health = statuses
                .iter()
                .filter(|status| &status.service == service)
                .map(ContainerStatus::health)
                .min_by_key(health_rank)
                .unwrap_or(Health::Missing);
            (service.clone(), health)
        })
        .collect()
}

/// Lower is worse; used to report the least healthy replica.
fn health_rank(health: &Health) -> u8 {
    match health {
        Health::Missing => 0,
        Health::Exited { .. } => 1,
        Health::Unhealthy => 2,
        Health::Other { .. } => 3,
        Health::Starting => 4,
        Health::Running => 5,
        Health::Healthy => 6,
    }
}

fn tcp() -> String {
    "tcp".to_owned()
}

/// Compose prints `published` as a string, but hand-written fixtures and older releases
/// use numbers.
fn string_or_number<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(match value {
        Some(serde_json::Value::String(text)) if !text.is_empty() => Some(text),
        Some(serde_json::Value::Number(number)) => Some(number.to_string()),
        _ => None,
    })
}

fn null_as_empty<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Vec<T>, D::Error> {
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = include_str!("../tests/fixtures/config.json");
    const PS: &str = include_str!("../tests/fixtures/ps.ndjson");

    fn model() -> ComposeModel {
        ComposeModel::from_json(CONFIG).expect("fixture parses")
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn parses_resolved_model() {
        let model = model();
        assert_eq!(model.name, "acme");
        let postgres = &model.services["postgres"];
        assert_eq!(postgres.container_name.as_deref(), Some("acme-postgres"));
        assert_eq!(postgres.ports[0].target, 5432);
        assert_eq!(postgres.ports[0].published.as_deref(), Some("15432"));
        assert!(postgres.healthcheck.is_some());
        let redis = &model.services["redis"];
        assert_eq!(redis.ports[0].published, None);
        assert_eq!(redis.ports[0].host_ip.as_deref(), Some("127.0.0.1"));
        assert!(
            !model.services.contains_key("search"),
            "profile-disabled services are absent"
        );
    }

    #[test]
    fn shared_services_skip_exclude_and_per_lane() {
        let model = model();
        let selected = model
            .shared_services(&set(&["caddy"]), &set(&["mailpit"]))
            .expect("valid selection");
        assert_eq!(selected, ["postgres", "redis"]);
    }

    #[test]
    fn unknown_names_are_errors() {
        let error = model().shared_services(&set(&["cady"]), &set(&[])).err();
        assert!(matches!(error, Some(Error::UnknownService(name)) if name == "cady"));
    }

    #[test]
    fn dependency_on_skipped_service_is_an_error() {
        let error = model().shared_services(&set(&["redis"]), &set(&[])).err();
        assert!(
            matches!(&error, Some(Error::ExcludedDependency { dependency, .. }) if dependency == "redis"),
            "{error:?}"
        );
    }

    #[test]
    fn detects_volumes_shared_across_projects() {
        let json = r#"{"name":"acme-lane1","services":{"db":{"volumes":[
            {"type":"volume","source":"own","target":"/a"},
            {"type":"volume","source":"fixed","target":"/b"},
            {"type":"volume","source":"ext","target":"/c"},
            {"type":"bind","source":"/tmp","target":"/d"}]}},
            "volumes":{"own":{"name":"acme-lane1_own"},"fixed":{"name":"pgdata"},
            "ext":{"name":"ext","external":true}}}"#;
        let model = ComposeModel::from_json(json).expect("valid model");
        assert_eq!(model.shared_volumes(&["db".to_owned()]), ["ext", "fixed"]);
    }

    #[test]
    fn parses_ps_lines_and_arrays() {
        let statuses = parse_ps(PS).expect("fixture parses");
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].service, "postgres");
        assert_eq!(statuses[0].health(), Health::Healthy);
        assert_eq!(statuses[1].health(), Health::Running);
        let array = format!("[{}]", PS.trim().lines().collect::<Vec<_>>().join(","));
        assert_eq!(parse_ps(&array).unwrap_or_default(), statuses);
        assert_eq!(parse_ps("").unwrap_or_default(), []);
    }

    #[test]
    fn health_per_service() {
        let statuses = parse_ps(PS).unwrap_or_default();
        let services = [
            "postgres".to_owned(),
            "redis".to_owned(),
            "mailpit".to_owned(),
        ];
        let health = service_health(&services, &statuses);
        assert_eq!(health["postgres"], Health::Healthy);
        assert_eq!(health["redis"], Health::Running);
        assert_eq!(health["mailpit"], Health::Missing);
    }

    #[test]
    fn exited_and_transitional_states() {
        let line = r#"{"ID":"x","Name":"n","Service":"s","State":"exited","ExitCode":3,"Publishers":null}"#;
        let statuses = parse_ps(line).unwrap_or_default();
        assert_eq!(statuses[0].health(), Health::Exited { code: 3 });
        let line = r#"{"ID":"x","Name":"n","Service":"s","State":"created"}"#;
        let statuses = parse_ps(line).unwrap_or_default();
        assert_eq!(
            statuses[0].health(),
            Health::Other {
                state: "created".into()
            }
        );
    }
}

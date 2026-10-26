use std::collections::{BTreeMap, HashMap};

use bollard::models::{EventMessage, EventMessageTypeEnum};
use serde::Serialize;

use crate::{Scope, labels};

/// Compose's project and service labels, so compose containers can be watched too.
const COMPOSE_PROJECT: &str = "com.docker.compose.project";
const COMPOSE_SERVICE: &str = "com.docker.compose.service";

/// Docker event names worth subscribing to. Healthcheck runs also emit `exec_*` events,
/// which this filter keeps off the stream. `pause` and `unpause` are left out: a paused
/// container keeps its state, and the daemon shows a frozen lane from its own records.
pub const WATCHED_EVENTS: &[&str] = &[
    "start",
    "restart",
    "die",
    "stop",
    "destroy",
    "oom",
    "health_status",
];

/// A container lifecycle change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum EventKind {
    Started,
    HealthStarting,
    Healthy,
    Unhealthy,
    /// The main process exited; `stop` and `kill` also end in this event.
    Died {
        exit_code: Option<i64>,
    },
    Stopped,
    OutOfMemory,
    Removed,
}

/// A container event with the labels Docker attaches to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HealthEvent {
    pub container_id: String,
    pub container_name: String,
    /// Container labels plus Docker's `name`/`image` attributes.
    pub attributes: BTreeMap<String, String>,
    pub kind: EventKind,
    /// Unix time in nanoseconds, when Docker sends it.
    pub time_nano: Option<i64>,
}

impl HealthEvent {
    /// `dev.devlanes.project` of a plain container.
    pub fn project(&self) -> Option<&str> {
        self.attributes.get(labels::PROJECT).map(String::as_str)
    }

    /// The `infra.containers` key of a plain container.
    pub fn container_key(&self) -> Option<&str> {
        self.attributes.get(labels::CONTAINER).map(String::as_str)
    }

    /// Shared or lane scope of a devlanes container.
    pub fn scope(&self) -> Option<Scope> {
        Scope::from_labels(&self.attributes)
    }

    /// Compose project and service, for containers created by Compose.
    pub fn compose_service(&self) -> Option<(&str, &str)> {
        Some((
            self.attributes.get(COMPOSE_PROJECT)?.as_str(),
            self.attributes.get(COMPOSE_SERVICE)?.as_str(),
        ))
    }
}

/// Maps a Docker event to a lifecycle change; other types and actions return `None`.
pub fn map_event(event: &EventMessage) -> Option<HealthEvent> {
    if event.typ != Some(EventMessageTypeEnum::CONTAINER) {
        return None;
    }
    let actor = event.actor.as_ref()?;
    let empty = HashMap::new();
    let attributes = actor.attributes.as_ref().unwrap_or(&empty);
    let action = event.action.as_deref()?;
    let kind = match action {
        "start" | "restart" => EventKind::Started,
        "die" => EventKind::Died {
            exit_code: attributes
                .get("exitCode")
                .and_then(|code| code.parse().ok()),
        },
        "stop" => EventKind::Stopped,
        "oom" => EventKind::OutOfMemory,
        "destroy" => EventKind::Removed,
        "health_status: healthy" => EventKind::Healthy,
        "health_status: unhealthy" => EventKind::Unhealthy,
        "health_status: starting" => EventKind::HealthStarting,
        _ => return None,
    };
    Some(HealthEvent {
        container_id: actor.id.clone().unwrap_or_default(),
        container_name: attributes.get("name").cloned().unwrap_or_default(),
        attributes: attributes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        kind,
        time_nano: event.time_nano,
    })
}

#[cfg(test)]
mod tests {
    use bollard::models::EventActor;

    use super::*;

    fn event(action: &str, attributes: &[(&str, &str)]) -> EventMessage {
        EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some(action.into()),
            actor: Some(EventActor {
                id: Some("abc".into()),
                attributes: Some(
                    attributes
                        .iter()
                        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                        .collect(),
                ),
            }),
            time_nano: Some(7),
            ..Default::default()
        }
    }

    const LANE: &[(&str, &str)] = &[
        ("name", "devlanes-acme-lane2-cerbos"),
        ("dev.devlanes.project", "acme"),
        ("dev.devlanes.container", "cerbos"),
        ("dev.devlanes.scope", "lane"),
        ("dev.devlanes.lane", "2"),
    ];

    #[test]
    fn maps_lifecycle_actions() {
        let cases = [
            ("start", EventKind::Started),
            ("restart", EventKind::Started),
            ("stop", EventKind::Stopped),
            ("destroy", EventKind::Removed),
            ("oom", EventKind::OutOfMemory),
            ("health_status: healthy", EventKind::Healthy),
            ("health_status: unhealthy", EventKind::Unhealthy),
            ("health_status: starting", EventKind::HealthStarting),
        ];
        for (action, kind) in cases {
            let mapped = map_event(&event(action, LANE)).map(|event| event.kind);
            assert_eq!(mapped, Some(kind), "{action}");
        }
    }

    #[test]
    fn die_keeps_exit_code_and_labels() {
        let mut attributes = LANE.to_vec();
        attributes.push(("exitCode", "137"));
        let mapped = map_event(&event("die", &attributes)).expect("mapped");
        assert_eq!(
            mapped.kind,
            EventKind::Died {
                exit_code: Some(137)
            }
        );
        assert_eq!(mapped.container_name, "devlanes-acme-lane2-cerbos");
        assert_eq!(mapped.container_id, "abc");
        assert_eq!(mapped.project(), Some("acme"));
        assert_eq!(mapped.container_key(), Some("cerbos"));
        assert_eq!(mapped.scope(), Some(Scope::Lane(2)));
        assert_eq!(mapped.time_nano, Some(7));
    }

    #[test]
    fn healthcheck_execs_and_other_types_are_ignored() {
        // Healthcheck runs emit exec_die with an exitCode; it is not a container exit.
        assert_eq!(map_event(&event("exec_die", &[("exitCode", "1")])), None);
        assert_eq!(
            map_event(&event("exec_start: /bin/sh -c redis-cli ping", &[])),
            None
        );
        assert_eq!(map_event(&event("create", LANE)), None);
        let mut network = event("start", LANE);
        network.typ = Some(EventMessageTypeEnum::NETWORK);
        assert_eq!(map_event(&network), None);
    }

    #[test]
    fn compose_labels() {
        let attributes = [
            ("name", "acme-postgres"),
            ("com.docker.compose.project", "acme"),
            ("com.docker.compose.service", "postgres"),
        ];
        let mapped = map_event(&event("health_status: healthy", &attributes)).expect("mapped");
        assert_eq!(mapped.compose_service(), Some(("acme", "postgres")));
        assert_eq!(mapped.project(), None);
    }
}

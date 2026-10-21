//! Plain Docker containers for `infra.containers`, through the Engine API (bollard).
//!
//! Containers are named `devlanes-<project>[-lane<N>]-<name>` and carry `dev.devlanes.*`
//! labels, which drive re-adoption after a daemon restart, event filtering and lane clean.
//! Host ports come from the caller's port block and bind to 127.0.0.1. Relative volume
//! paths resolve against the caller's base directory; named volumes are namespaced per
//! project and lane.
//!
//! This crate does not depend on `devlanes-core`; the daemon adapts [`DockerInfra`] to the
//! core `Infra` trait.

mod docker;
mod error;
mod events;
mod health;
mod host;
mod spec;

pub use docker::{
    DockerInfra, Endpoint, EnsureAction, EnsureOptions, Ensured, ManagedContainer, Removed,
    create_body,
};
pub use error::Error;
pub use events::{EventKind, HealthEvent, WATCHED_EVENTS, map_event};
pub use health::{Health, InspectHealth, health_from_inspect, health_from_summary, tcp_probe};
pub use host::{DEFAULT_HOST, config_dir, resolve_host};
pub use spec::{
    ContainerSpec, Mount, NamedPort, PortBlock, Protocol, Scope, config_hash, container_labels,
    container_name, labels, primary_port, resolve_volume, scope_labels, validate, validate_name,
    volume_name,
};

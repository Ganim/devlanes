//! Docker Compose projects for shared and per-lane infra.
//!
//! The shared project runs once per devlanes project from the primary worktree. Per-lane
//! services run as `<project>-lane<N>` from each lane's worktree, with a generated override
//! that moves their ports into the lane block, drops fixed `container_name`s and adds
//! `dev.devlanes.*` labels. Lifecycle goes through the `docker compose` CLI so Compose's
//! own merge, interpolation and profile rules apply; the resolved model comes from
//! `docker compose config --format json`.
//!
//! This crate does not depend on `devlanes-core`; the daemon adapts [`Compose`] to the
//! core `Infra` trait. Running `infra.setup` entries is the daemon's job;
//! [`setup_env`] provides their Compose variables.

mod cli;
mod env;
mod error;
mod infra;
mod model;
mod overrides;
mod ports;
mod version;

pub use cli::{ComposeCli, ComposeProject, ExistingContainer, UpOptions};
pub use env::setup_env;
pub use error::{Error, NameConflict};
pub use infra::{
    Compose, LaneInfra, LaneReport, ProjectLocks, SharedInfra, SharedReport, classify_conflicts,
    lane_project_name, validate_project_name,
};
pub use model::{
    ComposeModel, ContainerStatus, Health, PortModel, Publisher, ServiceModel, ServiceVolume,
    VolumeModel, parse_ps, service_containers, service_health,
};
pub use overrides::{
    LaneOverride, OVERRIDE_FILE, labels, lane_labels, render_override, write_override,
};
pub use ports::{
    Endpoint, Endpoints, LanePorts, PortBlock, PortRequest, endpoints_from_ps, lane_endpoints,
    port_requests,
};
pub use version::ComposeVersion;

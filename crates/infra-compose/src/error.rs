use std::{path::PathBuf, time::Duration};

use crate::version::ComposeVersion;

/// Failures of the Compose adapter. Command errors keep stderr because Compose explains most
/// configuration problems there; process environments are never included.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("compose I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("`{command}` failed ({status}): {stderr}")]
    Command {
        command: String,
        status: String,
        stderr: String,
    },
    #[error("`{command}` did not finish within {timeout:?}")]
    Timeout { command: String, timeout: Duration },
    #[error("unexpected output from `{command}`: {source}")]
    Json {
        command: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("cannot parse the compose version from {0:?}")]
    VersionParse(String),
    #[error(
        "docker compose {found} does not support the `!override`/`!reset` tags devlanes needs for per-lane services; upgrade to {required} or later"
    )]
    UnsupportedVersion {
        found: ComposeVersion,
        required: ComposeVersion,
    },
    #[error(
        "invalid compose project name {0:?}: use lowercase letters, digits, `-` and `_`, starting with a letter or digit"
    )]
    InvalidProjectName(String),
    #[error(
        "compose service {0:?} is not in the resolved model (missing, or disabled by the active profiles)"
    )]
    UnknownService(String),
    #[error(
        "compose service {service:?} depends on {dependency:?}, which devlanes does not start in this project (exclude or per_lane)"
    )]
    ExcludedDependency { service: String, dependency: String },
    #[error("invalid port block starting at {first} with size {size}")]
    InvalidBlock { first: u16, size: u16 },
    #[error("host port {port} for {service}:{target} is outside the lane block {first}-{last}")]
    PortOutsideBlock {
        service: String,
        target: u16,
        port: u16,
        first: u16,
        last: u16,
    },
    #[error("no host port assigned for container port {target} of per-lane service {service:?}")]
    MissingPortAssignment { service: String, target: u16 },
    #[error("host port {port} is assigned twice in the lane block")]
    DuplicatePort { port: u16 },
    #[error("compose path {0:?} contains the COMPOSE_FILE separator `:`")]
    PathSeparator(PathBuf),
    #[error("{}", conflict_message(.0))]
    ContainerNameConflict(Vec<NameConflict>),
}

/// An existing container that blocks a `container_name` the shared project wants to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NameConflict {
    pub service: String,
    pub container_name: String,
    /// The Compose project that created the container, if any.
    pub owner: Option<String>,
}

/// Explains each conflict with the command that frees the name, since the user created it.
fn conflict_message(conflicts: &[NameConflict]) -> String {
    let lines: Vec<String> = conflicts
        .iter()
        .map(|conflict| match &conflict.owner {
            Some(owner) => format!(
                "container {:?} (service {:?}) belongs to compose project {owner:?}; stop it with `docker compose -p {owner} down` or set infra.project_name: {owner} to adopt it",
                conflict.container_name, conflict.service
            ),
            None => format!(
                "container {:?} (service {:?}) was not created by compose; remove it with `docker rm -f {}`",
                conflict.container_name, conflict.service, conflict.container_name
            ),
        })
        .collect();
    lines.join("\n")
}

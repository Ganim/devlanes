use std::{path::PathBuf, time::Duration};

use crate::Health;

/// Failures of the plain-container adapter.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("docker API: {0}")]
    Docker(#[from] bollard::errors::Error),
    #[error("docker I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot read docker config {path}: {source}")]
    DockerConfig {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("docker context {0:?} not found; check `docker context ls`")]
    UnknownContext(String),
    #[error(
        "invalid name {0:?}: use letters, digits, `_`, `.` and `-`, starting with a letter or digit"
    )]
    InvalidName(String),
    #[error("invalid volume {spec:?} for container {container:?}: {reason}")]
    InvalidVolume {
        container: String,
        spec: String,
        reason: &'static str,
    },
    #[error("invalid port block starting at {first} with size {size}")]
    InvalidBlock { first: u16, size: u16 },
    #[error("host port {port} of {container}.{name} is outside the block {first}-{last}")]
    PortOutsideBlock {
        container: String,
        name: String,
        port: u16,
        first: u16,
        last: u16,
    },
    #[error("container {container:?} uses host port {port} twice")]
    DuplicatePort { container: String, port: u16 },
    #[error("container {container:?} declares port name {name:?} twice")]
    DuplicatePortName { container: String, name: String },
    #[error(
        "a container named {name:?} already exists and devlanes does not manage it{}; remove it with `docker rm -f {name}`",
        .owner.as_ref().map(|owner| format!(" (it belongs to devlanes project {owner:?})")).unwrap_or_default()
    )]
    NameConflict { name: String, owner: Option<String> },
    #[error("container {name:?} is {health:?} instead of healthy")]
    Unhealthy { name: String, health: Health },
    #[error("container {name:?} was not healthy within {timeout:?} (last state {last:?})")]
    HealthTimeout {
        name: String,
        timeout: Duration,
        last: Health,
    },
}

impl Error {
    /// Docker answers 404 for missing containers, images and volumes.
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Error::Docker(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })
        )
    }
}

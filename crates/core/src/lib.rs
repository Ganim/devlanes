//! Pure domain contracts shared by daemon integrations and clients.

pub mod clock;
pub mod color;
pub mod contrib;
pub mod env;
pub mod gate;
pub mod paths;
pub mod ports;
pub mod refs;
pub mod runtime;
pub mod shell;
pub mod stop;

pub use runtime::*;
use serde::{Deserialize, Serialize};
pub use stop::StopSignal;
#[cfg(windows)]
pub use stop::WindowsStop;

macro_rules! id {
    ($name:ident) => {
        #[doc = concat!("Persistent ", stringify!($name), " identity, independent of display names.")]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub i64);
    };
}
id!(ProjectId);
id!(WorktreeId);
id!(LaneId);
id!(UnitRunId);

/// Domain and integration failures without presentation-layer diagnostics.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid input: {0}")]
    InvalidInput(String),
    #[error("operation unsupported: {0}")]
    Unsupported(String),
    #[error("unit is no longer running")]
    Exited,
    #[error("I/O failure: {0}")]
    Io(#[from] std::io::Error),
    #[error("integration failed: {0}")]
    Integration(String),
}

/// Attribution supplied by the client; integrations never infer it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    pub kind: ActorKind,
    pub agent_id: Option<String>,
    pub session_id: Option<String>,
}

/// Client classification for ownership filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    Human,
    Agent,
}

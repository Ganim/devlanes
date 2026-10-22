//! Transport-independent command parsing, request construction, and presentation.

pub mod agent_hook;
mod args;
mod choose;
mod ci;
mod completion;
mod output;
mod progress;
mod render;
mod runtime;
mod with;

pub use args::*;
pub use choose::*;
pub use ci::*;
pub use clap;
pub use clap_complete::Shell;
pub use completion::*;
pub use output::*;
pub use progress::*;
pub use render::{DoctorCheck, DoctorState, doctor_checks_of};
pub use runtime::*;
pub use with::*;

/// Whether the CLI words daemon code `code` itself (tests keep every code the
/// daemon sends covered).
#[doc(hidden)]
pub fn has_wording(code: &str) -> bool {
    render::has_wording(code)
}

/// Process exit statuses shared by all command handlers.
pub mod exit {
    pub const OK: u8 = 0;
    pub const FAILED: u8 = 1;
    pub const USAGE: u8 = 2;
    pub const NOT_FOUND: u8 = 3;
    pub const DAEMON: u8 = 4;
    pub const TIMEOUT: u8 = 124;
}

/// Argument validation is separate from transport and daemon failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    Usage(String),
    #[error("cannot capture invocation context: {0}")]
    Io(#[from] std::io::Error),
    #[error("cannot encode request: {0}")]
    Json(#[from] serde_json::Error),
}

impl Error {
    /// Maps local failures without concealing daemon-provided exit statuses.
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Usage(_) => exit::USAGE,
            Self::Io(_) | Self::Json(_) => exit::FAILED,
        }
    }
}

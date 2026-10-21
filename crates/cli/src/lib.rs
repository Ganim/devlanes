//! Transport-independent command parsing, request construction, and presentation.

mod args;
mod ci;
mod completion;
mod output;
mod runtime;
mod with;

pub use args::*;
pub use ci::*;
pub use clap;
pub use clap_complete::Shell;
pub use completion::*;
pub use output::*;
pub use runtime::*;
pub use with::*;

/// Local help for agents; process ownership always remains with the daemon.
pub const AGENT_GUIDE: &str = "Use devlanes start to bring up the current worktree lane.\n\
Use devlanes exec --name NAME -- COMMAND instead of backgrounding a process.\n\
Inspect with status, wait UNIT --timeout 180s, errors, and why UNIT.\n\
Read bounded logs with logs UNIT --tail 100; use --cursor to continue.\n\
Follow logs only with --timeout. Use --json for complete structured results.\n\
Targets are project@lane:unit; selectors accept globs and +tags.\n\
Use status --mine and stop --mine for your session's units.\n\
Ctrl-C detaches from services. Foreground run tasks are the exception.\n\
Use --yes when confirmation is required; agents are never prompted.\n";

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

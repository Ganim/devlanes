//! Transport-independent command parsing, request construction, and presentation.

mod args;
mod ci;
mod completion;
mod output;
mod render;
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
/// Short form of `agent-plugin/skills/devlanes/SKILL.md`; update both together.
pub const AGENT_GUIDE: &str = "Use devlanes start to bring up the current worktree lane; it waits for ready or failure.\n\
Never background a process with &. Use devlanes exec --name NAME -- COMMAND instead.\n\
Lanes are project@N or project@branch; units are LANE:UNIT; globs and +tags select several.\n\
Inspect with status, wait UNIT --timeout 180s, debug errors, and debug why UNIT.\n\
Read bounded logs with logs UNIT --tail 100 or --grep RE; use --cursor to continue.\n\
Follow logs only with --timeout. Use --json for complete structured results.\n\
Run tests with devlanes with -- COMMAND: start, wait, run, stop, exit with COMMAND's code.\n\
Use status --mine and stop --mine for your session's units.\n\
A command that needs confirmation exits 2; rerun it with --yes. Agents are never prompted.\n\
Ctrl-C detaches from services. Foreground run tasks are the exception.\n\
Inside a sandbox the daemon is not auto-started; ask the user to run devlanes daemon install.\n";

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

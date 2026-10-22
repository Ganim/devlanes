use std::io;
use std::path::PathBuf;

use crate::TrustTarget;

/// Failure of a trust-store operation.
#[derive(Debug, thiserror::Error)]
pub enum TrustError {
    /// The target is excluded from v1 (iOS simulator, Android emulator).
    #[error("{target} certificate trust is not supported yet")]
    Unsupported {
        /// The rejected target.
        target: TrustTarget,
    },
    /// A `tls.trust` entry that names no known target.
    #[error("unknown trust target `{0}`")]
    UnknownTarget(String),
    /// The PEM or name handed to the store is unusable.
    #[error("invalid CA certificate: {0}")]
    InvalidCertificate(String),
    /// A required tool is not installed.
    #[error("`{tool}` was not found; {hint}")]
    ToolMissing {
        /// Program name.
        tool: String,
        /// How to install it.
        hint: String,
    },
    /// No database exists for a target that cannot create one (Firefox profiles).
    #[error("no {target} certificate databases found under {}", join_paths(.searched))]
    NoDatabases {
        /// Target whose databases are missing.
        target: TrustTarget,
        /// Directories that were searched.
        searched: Vec<PathBuf>,
    },
    /// The OS has none of the CA store layouts this crate knows.
    #[error("unsupported system CA store: {0}")]
    UnsupportedSystem(String),
    /// The command needs a password or GUI approval that cannot be given here.
    #[error("`{command}` needs interactive approval: {detail}")]
    InteractionRequired {
        /// The command line, for the user to rerun.
        command: String,
        /// What is missing, with a hint.
        detail: String,
    },
    /// The user declined the OS confirmation dialog (the Windows root store warning).
    #[error("{detail}")]
    Cancelled {
        /// What was declined and how to retry.
        detail: String,
    },
    /// A command exited unsuccessfully.
    #[error("`{command}` failed with {}: {}", exit_code(*.code), stderr.trim())]
    CommandFailed {
        /// The command line.
        command: String,
        /// Exit code, `None` when killed by a signal.
        code: Option<i32>,
        /// Captured standard error.
        stderr: String,
    },
    /// Local I/O around a command failed (temp files, reading stores).
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Underlying error.
        #[source]
        source: io::Error,
    },
}

impl TrustError {
    /// Wraps an I/O error with the operation that hit it.
    pub fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

/// Formats searched paths for [`TrustError::NoDatabases`].
fn join_paths(paths: &[PathBuf]) -> String {
    let joined = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    if joined.is_empty() {
        "no directories".into()
    } else {
        joined
    }
}

/// Formats an optional exit code for [`TrustError::CommandFailed`].
fn exit_code(code: Option<i32>) -> String {
    code.map_or_else(|| "a signal".into(), |code| format!("exit code {code}"))
}

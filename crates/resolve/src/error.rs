/// Resolution failures. Messages name config paths and units, never env values.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// An expression or typed value failed to evaluate.
    #[error("{path}: {message}")]
    Expression { path: String, message: String },
    #[error("unknown profile `{name}`; available: {}", list(.available))]
    UnknownProfile {
        name: String,
        available: Vec<String>,
    },
    #[error("selector `{selector}` matches no services; available: {}", list(.candidates))]
    NoMatch {
        selector: String,
        candidates: Vec<String>,
    },
    #[error("invalid selector `{selector}`: {message}")]
    InvalidSelector { selector: String, message: String },
    /// Service ports or hostnames depend on each other in a loop.
    #[error("service reference cycle: {}", .chain.join(" -> "))]
    Cycle { chain: Vec<String> },
    #[error("{unit}: port `{name}` has no allocated offset; allocate port_needs() first")]
    MissingOffset { unit: String, name: String },
    #[error("{unit}: {message}")]
    Port { unit: String, message: String },
    #[error("{unit}: no script runner handles `{file}`")]
    NoScriptRunner { unit: String, file: String },
    #[error("{unit}: {message}")]
    Script { unit: String, message: String },
    #[error("{unit}: contributor `{id}` failed: {message}")]
    Contributor {
        unit: String,
        id: String,
        message: String,
    },
    /// The secrets provider needs an interactive unlock.
    #[error("secrets{} are locked ({message}); run `devlanes unlock`", profile_suffix(.profile))]
    SecretsLocked {
        profile: Option<String>,
        message: String,
    },
    #[error("secrets{}: {message}", profile_suffix(.profile))]
    Secrets {
        profile: Option<String>,
        message: String,
    },
    #[error("{path}: {message}")]
    Invalid { path: String, message: String },
}

impl ResolveError {
    /// Wraps a config evaluation error with the config path it came from.
    pub(crate) fn expression(path: impl Into<String>, error: devlanes_config::ConfigError) -> Self {
        Self::Expression {
            path: path.into(),
            message: error.message,
        }
    }

    /// Builds an [`ResolveError::Invalid`] for `path`.
    pub(crate) fn invalid(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Invalid {
            path: path.into(),
            message: message.into(),
        }
    }
}

/// Formats a candidate list for messages.
fn list(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_owned()
    } else {
        values.join(", ")
    }
}

/// Formats ` for profile `x`` when a profile is named.
fn profile_suffix(profile: &Option<String>) -> String {
    profile
        .as_ref()
        .map(|p| format!(" for fnox profile `{p}`"))
        .unwrap_or_default()
}

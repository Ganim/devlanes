//! SSH [`devlanes_transport::Transport`] over system OpenSSH.
//!
//! One ControlMaster per box lives under the given control directory; each
//! session runs `devlanes tunnel --stdio` on the box over that master. Prompts
//! go through [`askpass`]; a missing binary yields a [`BootstrapPlan`].
//!
//! Unix only for now: on other platforms this crate is empty.

// ControlMaster, the askpass wrapper script and its Unix socket have no Windows
// counterpart yet, so the whole crate is compiled out there.
#![cfg(unix)]

pub mod askpass;
mod bootstrap;
mod ssh;

pub use askpass::{
    AskpassConfig, AskpassError, AskpassPrompt, PromptKind, PromptRelay, run_askpass,
};
pub use bootstrap::{
    Artifact, ArtifactFormat, BootstrapPlan, BootstrapPolicy, INSTALL_DIR, ManifestError,
    ReleaseManifest, RemotePlatform, RemoteProbe,
};
pub use ssh::{SshError, SshOptions, SshStream, SshTransport};

/// Quotes `value` for a POSIX shell using single quotes.
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting_survives_embedded_single_quotes() {
        assert_eq!(super::sh_quote("it's"), r"'it'\''s'");
    }
}

//! SSH [`devlanes_transport::Transport`] over system OpenSSH.
//!
//! On Unix, one ControlMaster per box lives under the given control directory
//! and each session runs `devlanes tunnel --stdio` on the box over it. Windows'
//! OpenSSH has no ControlMaster, so there each session is one `ssh` process that
//! probes the box and then execs the tunnel. Prompts go through [`askpass`]; a
//! missing binary yields a [`BootstrapPlan`].

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

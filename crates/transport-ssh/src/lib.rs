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

/// 128 bits from the OS random source as 32 lowercase hex digits, for names
/// another process must not be able to predict (relay pipes, session sentinels).
#[cfg(windows)]
pub(crate) fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    #[test]
    fn quoting_survives_embedded_single_quotes() {
        assert_eq!(super::sh_quote("it's"), r"'it'\''s'");
    }
}

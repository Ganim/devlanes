//! Relays ssh passphrase, password and host-key prompts to an attached client.
//!
//! ssh runs `SSH_ASKPASS` as a bare program with the prompt as its only
//! argument. On Unix, `SSH_ASKPASS` points at a generated wrapper script that
//! execs `<devlanes exe> askpass <prompt>`; on Windows it is the devlanes
//! executable itself, and the [`MARKER_ENV`] variable tells it to act as askpass.
//! Either way [`run_askpass`] asks the daemon over a private endpoint (a Unix
//! socket, or a named pipe on Windows) named by [`SOCKET_ENV`], and the daemon
//! hands the prompt to a [`PromptRelay`].

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
pub(crate) use unix::AskpassServer;
#[cfg(windows)]
pub(crate) use windows::AskpassServer;
#[cfg(windows)]
pub use windows::relay_process_id;

/// Environment variable carrying the relay endpoint to the askpass process: a
/// socket path on Unix, a pipe name on Windows.
pub const SOCKET_ENV: &str = "DEVLANES_ASKPASS_SOCKET";

/// Set to `1` in the environment of ssh, whose `SSH_ASKPASS` is the devlanes
/// executable on Windows: the binary then answers the prompt in its single
/// argument instead of parsing a command line.
#[cfg(windows)]
pub const MARKER_ENV: &str = "DEVLANES_ASKPASS";

/// Upper bound for one request or reply line.
const MAX_LINE: u64 = 64 * 1024;

/// What ssh expects back for a prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Typed text such as a passphrase, OTP or host-key `yes`.
    Secret,
    /// Yes or no, answered through the exit status (`SSH_ASKPASS_PROMPT=confirm`).
    Confirm,
    /// Informational; ssh does not wait for an answer (`SSH_ASKPASS_PROMPT=none`).
    Notify,
}

impl PromptKind {
    /// Interprets the `SSH_ASKPASS_PROMPT` value ssh sets for the askpass process.
    pub fn from_env_value(value: Option<&str>) -> Self {
        match value {
            Some("confirm") => Self::Confirm,
            Some("none") => Self::Notify,
            _ => Self::Secret,
        }
    }
}

/// One prompt from ssh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskpassPrompt {
    /// The ssh destination being authenticated.
    pub destination: String,
    /// Prompt text from ssh.
    pub prompt: String,
    /// Expected answer shape.
    pub kind: PromptKind,
}

/// Shows prompts to whoever is attached, such as the CLI that ran `connect`.
#[async_trait]
pub trait PromptRelay: Send + Sync {
    /// Returns the answer, or `None` when the user cancelled or nobody is attached.
    ///
    /// For [`PromptKind::Confirm`], any `Some` answer other than `no` accepts.
    async fn prompt(&self, prompt: AskpassPrompt) -> Option<String>;
}

/// Askpass settings for ssh launches.
#[derive(Clone)]
pub struct AskpassConfig {
    /// The devlanes binary that implements the `askpass` subcommand.
    pub exe: PathBuf,
    /// Where prompts go.
    pub relay: Arc<dyn PromptRelay>,
}

impl AskpassConfig {
    /// Uses the running executable, which ships the `askpass` subcommand.
    pub fn current_exe(relay: Arc<dyn PromptRelay>) -> io::Result<Self> {
        Ok(Self {
            exe: std::env::current_exe()?,
            relay,
        })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Request {
    prompt: String,
    kind: PromptKind,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Reply {
    Answer { reply: String },
    Cancelled,
}

/// Failure inside the askpass subcommand.
#[derive(Debug, thiserror::Error)]
pub enum AskpassError {
    /// `DEVLANES_ASKPASS_SOCKET` is not set, so this was not started by devlanes' ssh.
    #[error("askpass was not started by a devlanes ssh connection")]
    NotConfigured,
    /// The relay name is not one devlanes creates, so it is never opened: a UNC
    /// name such as `\\host\pipe\x` would send the user's NTLM credentials there.
    #[cfg(windows)]
    #[error("{0:?} is not a devlanes askpass relay")]
    UnknownRelay(String),
    /// The user declined or nobody was attached to answer.
    #[error("prompt cancelled")]
    Cancelled,
    /// Talking to the daemon failed.
    #[error("askpass relay: {0}")]
    Io(#[from] io::Error),
    /// The daemon answered with something unreadable.
    #[error("askpass relay sent an invalid reply: {0}")]
    Protocol(#[from] serde_json::Error),
}

/// Entry point for `devlanes askpass <prompt>`, reading ssh's environment.
///
/// Print the returned answer on stdout and exit 0; on error exit 1, which ssh
/// treats as a cancelled prompt.
pub async fn run_askpass(prompt: &str) -> Result<String, AskpassError> {
    let socket = std::env::var_os(SOCKET_ENV).ok_or(AskpassError::NotConfigured)?;
    let kind = PromptKind::from_env_value(std::env::var("SSH_ASKPASS_PROMPT").ok().as_deref());
    request(Path::new(&socket), prompt, kind).await
}

/// Sends one prompt to the relay endpoint (a socket path, or a pipe name on
/// Windows) and waits for the answer.
pub async fn request(
    socket: &Path,
    prompt: &str,
    kind: PromptKind,
) -> Result<String, AskpassError> {
    #[cfg(unix)]
    let stream = unix::connect(socket).await?;
    #[cfg(windows)]
    let stream = windows::connect(socket).await?;
    let (read, mut write) = tokio::io::split(stream);
    let mut line = serde_json::to_vec(&Request {
        prompt: prompt.to_owned(),
        kind,
    })?;
    line.push(b'\n');
    write.write_all(&line).await?;
    let mut reply = String::new();
    BufReader::new(read.take(MAX_LINE))
        .read_line(&mut reply)
        .await?;
    match serde_json::from_str(&reply)? {
        Reply::Answer { reply } if kind != PromptKind::Confirm || reply != "no" => Ok(reply),
        _ => Err(AskpassError::Cancelled),
    }
}

/// Reads the one request line an askpass process sends.
async fn read_request(reader: impl AsyncRead + Unpin) -> Result<Request, AskpassError> {
    let mut line = String::new();
    BufReader::new(reader.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    Ok(serde_json::from_str(&line)?)
}

/// Asks the relay and writes its answer as the reply line.
async fn relay_reply(
    request: Request,
    relay: &dyn PromptRelay,
    destination: String,
    mut writer: impl AsyncWrite + Unpin,
) -> Result<(), AskpassError> {
    let prompt = AskpassPrompt {
        destination,
        prompt: request.prompt,
        kind: request.kind,
    };
    let reply = match relay.prompt(prompt).await {
        Some(reply) => Reply::Answer { reply },
        None => Reply::Cancelled,
    };
    let mut bytes = serde_json::to_vec(&reply)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    Ok(())
}

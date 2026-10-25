//! Relays ssh passphrase, password and host-key prompts to an attached client.
//!
//! ssh runs `SSH_ASKPASS` as a bare program with the prompt as its only
//! argument, so `SSH_ASKPASS` points at a generated wrapper script that execs
//! `<devlanes exe> askpass <prompt>`. That subcommand calls [`run_askpass`],
//! which asks the daemon over a private Unix socket; the daemon hands the
//! prompt to a [`PromptRelay`]. Windows has no wrapper script and a named pipe
//! in place of the socket; see the `windows` module.

#[cfg(unix)]
use std::ffi::OsString;
use std::io;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tokio::task::{JoinHandle, JoinSet};

#[cfg(unix)]
use crate::sh_quote;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::AskpassServer;
#[cfg(windows)]
pub use windows::{is_relay_name, request};

/// Environment variable carrying the relay socket to the askpass process. On
/// Windows it holds the relay pipe's name and also tells `devlanes.exe` that
/// ssh started it as askpass.
pub const SOCKET_ENV: &str = "DEVLANES_ASKPASS_SOCKET";

/// Longest Unix socket path (`sun_path` is 104 bytes on macOS, with the NUL).
#[cfg(unix)]
const MAX_SOCKET_PATH: usize = 103;

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

/// Askpass settings for master launches.
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

/// Sends one prompt to the relay socket and waits for the answer.
#[cfg(unix)]
pub async fn request(
    socket: &Path,
    prompt: &str,
    kind: PromptKind,
) -> Result<String, AskpassError> {
    let stream = UnixStream::connect(socket).await?;
    let (read, mut write) = stream.into_split();
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

/// A relay socket and wrapper script that live for one master launch.
#[cfg(unix)]
pub(crate) struct AskpassServer {
    socket: PathBuf,
    script: PathBuf,
    task: JoinHandle<()>,
}

#[cfg(unix)]
impl AskpassServer {
    /// Binds the socket and writes the wrapper next to it in `dir` (mode 0700).
    pub(crate) async fn start(
        dir: &Path,
        name: &str,
        config: &AskpassConfig,
        destination: &str,
    ) -> io::Result<Self> {
        let socket = dir.join(format!("{name}.sock"));
        if socket.as_os_str().len() > MAX_SOCKET_PATH {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "askpass socket path {} is too long; use a shorter devlanes home",
                    socket.display()
                ),
            ));
        }
        let script = dir.join(format!("{name}.askpass"));
        let exe = config.exe.to_str().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "devlanes path is not UTF-8")
        })?;
        let content = format!("#!/bin/sh\nexec {} askpass \"$@\"\n", sh_quote(exe));
        let script_path = script.clone();
        let socket_path = socket.clone();
        tokio::task::spawn_blocking(move || write_script(&script_path, &content, &socket_path))
            .await
            .map_err(io::Error::other)??;
        let listener = UnixListener::bind(&socket)?;
        // The directory is private to this user; answering only peers of the same
        // uid also covers a directory whose mode was loosened later.
        let owner = std::fs::metadata(dir)?.uid();
        let relay = config.relay.clone();
        let destination = destination.to_owned();
        let task = tokio::spawn(async move {
            // Held here so aborting this task also cancels prompts in flight.
            let mut answering = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok((stream, _)) = accepted else { break };
                        if !stream.peer_cred().is_ok_and(|cred| cred.uid() == owner) {
                            tracing::warn!("askpass request from another user refused");
                            continue;
                        }
                        let relay = relay.clone();
                        let destination = destination.clone();
                        answering.spawn(async move {
                            if let Err(error) = answer(stream, relay, destination).await {
                                tracing::debug!("askpass request failed: {error}");
                            }
                        });
                    }
                    Some(_) = answering.join_next(), if !answering.is_empty() => {}
                }
            }
        });
        Ok(Self {
            socket,
            script,
            task,
        })
    }

    /// Environment that routes ssh's prompts through this server.
    pub(crate) fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("SSH_ASKPASS", self.script.clone().into_os_string()),
            ("SSH_ASKPASS_REQUIRE", OsString::from("force")),
            (SOCKET_ENV, self.socket.clone().into_os_string()),
        ]
    }
}

#[cfg(unix)]
impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.task.abort();
        // Unlinking two small files does not block meaningfully.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.script);
    }
}

/// Writes the wrapper as 0700 and clears a stale socket left by a crash.
#[cfg(unix)]
fn write_script(path: &Path, content: &str, socket: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::remove_file(socket) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    std::fs::write(path, content)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Answers one askpass connection through the relay.
#[cfg(unix)]
async fn answer(
    stream: UnixStream,
    relay: Arc<dyn PromptRelay>,
    destination: String,
) -> Result<(), AskpassError> {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    BufReader::new(read.take(MAX_LINE))
        .read_line(&mut line)
        .await?;
    let request: Request = serde_json::from_str(&line)?;
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
    write.write_all(&bytes).await?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fixed;

    #[async_trait]
    impl PromptRelay for Fixed {
        async fn prompt(&self, prompt: AskpassPrompt) -> Option<String> {
            (prompt.destination == "box" && prompt.prompt.contains("passphrase"))
                .then(|| "hunter2".to_owned())
        }
    }

    #[tokio::test]
    async fn prompts_round_trip_through_the_relay() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(5), async {
            let dir = tempfile::tempdir()?;
            let config = AskpassConfig {
                exe: PathBuf::from("/opt/dev lanes/devlanes"),
                relay: Arc::new(Fixed),
            };
            let server = AskpassServer::start(dir.path(), "a-test", &config, "box").await?;
            let script = std::fs::read_to_string(dir.path().join("a-test.askpass"))?;
            assert_eq!(
                script,
                "#!/bin/sh\nexec '/opt/dev lanes/devlanes' askpass \"$@\"\n"
            );
            let env = server.env();
            assert!(
                env.iter()
                    .any(|(k, v)| *k == "SSH_ASKPASS_REQUIRE" && v == "force")
            );

            let socket = dir.path().join("a-test.sock");
            let answer = request(&socket, "Enter passphrase for key:", PromptKind::Secret).await?;
            assert_eq!(answer, "hunter2");
            let refused = request(&socket, "Password:", PromptKind::Secret).await;
            assert!(matches!(refused, Err(AskpassError::Cancelled)));
            drop(server);
            assert!(!socket.exists());
            Ok(())
        })
        .await?
    }
}

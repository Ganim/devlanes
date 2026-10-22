//! Unix relay: a wrapper script for `SSH_ASKPASS` and a private Unix socket.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::{UnixListener, UnixStream};
use tokio::task::{JoinHandle, JoinSet};

use super::{AskpassConfig, AskpassError, PromptRelay, SOCKET_ENV, read_request, relay_reply};
use crate::sh_quote;

/// Longest Unix socket path (`sun_path` is 104 bytes on macOS, with the NUL).
const MAX_SOCKET_PATH: usize = 103;

/// Connects to the relay socket.
pub(super) async fn connect(socket: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(socket).await
}

/// A relay socket and wrapper script that live for one master launch.
pub(crate) struct AskpassServer {
    socket: PathBuf,
    script: PathBuf,
    task: JoinHandle<()>,
}

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

impl Drop for AskpassServer {
    fn drop(&mut self) {
        self.task.abort();
        // Unlinking two small files does not block meaningfully.
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.script);
    }
}

/// Writes the wrapper as 0700 and clears a stale socket left by a crash.
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
async fn answer(
    stream: UnixStream,
    relay: Arc<dyn PromptRelay>,
    destination: String,
) -> Result<(), AskpassError> {
    let (read, write) = stream.into_split();
    let request = read_request(read).await?;
    relay_reply(request, relay.as_ref(), destination, write).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::askpass::{AskpassPrompt, PromptKind, request};
    use async_trait::async_trait;
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

//! Windows relay: `SSH_ASKPASS` is the devlanes executable itself and prompts
//! travel over a named pipe that only the current user can open.
//!
//! Win32-OpenSSH (checked with 9.5p2) runs `SSH_ASKPASS` as `<program> <prompt>`
//! with `SSH_ASKPASS_REQUIRE=force` honoured without a console, so a script
//! wrapper is not needed: the marker variable tells the binary what it is for.

use std::ffi::OsString;
use std::hash::{BuildHasher, RandomState};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use devlanes_ipc::local::{self, LocalListener, LocalStream};
use tokio::task::{JoinHandle, JoinSet};

use super::{
    AskpassConfig, AskpassError, MARKER_ENV, PromptRelay, SOCKET_ENV, read_request, relay_reply,
};

/// How long the askpass process waits for a busy pipe instance.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Distinguishes relays started by one process.
static NEXT_RELAY: AtomicU64 = AtomicU64::new(0);

/// Connects to the relay pipe, refusing one served by another account.
pub(super) async fn connect(pipe: &Path) -> io::Result<LocalStream> {
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, local::connect(pipe))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the askpass relay stayed busy"))??;
    if !stream.peer_is_current_user()? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the askpass relay is served by another user",
        ));
    }
    Ok(stream)
}

/// A relay pipe that lives for one ssh authentication.
pub(crate) struct AskpassServer {
    pipe: PathBuf,
    exe: PathBuf,
    task: JoinHandle<()>,
}

impl AskpassServer {
    /// Creates the pipe under a name nobody can guess, so another process cannot
    /// create it first and refuse to let it be created.
    pub(crate) async fn start(config: &AskpassConfig, destination: &str) -> io::Result<Self> {
        let unique = RandomState::new().hash_one(NEXT_RELAY.fetch_add(1, Ordering::Relaxed));
        let pipe = PathBuf::from(format!(
            r"\\.\pipe\devlanes-askpass-{}-{unique:016x}",
            std::process::id()
        ));
        let mut listener = LocalListener::bind(&pipe).await.map_err(io::Error::other)?;
        let relay = config.relay.clone();
        let destination = destination.to_owned();
        let task = tokio::spawn(async move {
            // Held here so aborting this task also cancels prompts in flight.
            let mut answering = JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let Ok(stream) = accepted else { break };
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
            pipe,
            exe: config.exe.clone(),
            task,
        })
    }

    /// Environment that routes ssh's prompts through this server.
    pub(crate) fn env(&self) -> Vec<(&'static str, OsString)> {
        vec![
            ("SSH_ASKPASS", self.exe.clone().into_os_string()),
            ("SSH_ASKPASS_REQUIRE", OsString::from("force")),
            (MARKER_ENV, OsString::from("1")),
            (SOCKET_ENV, self.pipe.clone().into_os_string()),
        ]
    }
}

impl Drop for AskpassServer {
    /// The pipe goes away with its last handle, so stopping the task is all.
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Answers one askpass connection, after confirming the client by impersonation
/// (Windows allows that only once the server has read from the pipe).
async fn answer(
    mut stream: LocalStream,
    relay: Arc<dyn PromptRelay>,
    destination: String,
) -> Result<(), AskpassError> {
    let request = read_request(&mut stream).await?;
    if !stream.confirm_peer_after_read()? {
        tracing::warn!("askpass request from another user refused");
        return Ok(());
    }
    relay_reply(request, relay.as_ref(), destination, &mut stream).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::askpass::{AskpassPrompt, PromptKind, request};
    use async_trait::async_trait;

    struct Fixed;

    #[async_trait]
    impl PromptRelay for Fixed {
        async fn prompt(&self, prompt: AskpassPrompt) -> Option<String> {
            (prompt.destination == "box" && prompt.prompt.contains("passphrase"))
                .then(|| "hunter2".to_owned())
        }
    }

    /// Prompts round-trip over the pipe; a refusal arrives as `Cancelled`; the
    /// environment names this executable and the pipe; dropping the server
    /// removes the pipe.
    #[tokio::test]
    async fn prompts_round_trip_through_the_relay() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let config = AskpassConfig {
                exe: PathBuf::from(r"C:\Program Files\devlanes\devlanes.exe"),
                relay: Arc::new(Fixed),
            };
            let server = AskpassServer::start(&config, "box").await?;
            let env: Vec<(&str, OsString)> = server.env();
            let value = |key: &str| {
                env.iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone())
            };
            assert_eq!(value("SSH_ASKPASS"), Some(config.exe.clone().into()));
            assert_eq!(value("SSH_ASKPASS_REQUIRE"), Some("force".into()));
            assert_eq!(value(MARKER_ENV), Some("1".into()));
            let pipe = PathBuf::from(value(SOCKET_ENV).ok_or("no pipe in the environment")?);

            let answer = request(&pipe, "Enter passphrase for key:", PromptKind::Secret).await?;
            assert_eq!(answer, "hunter2");
            let refused = request(&pipe, "Password:", PromptKind::Secret).await;
            assert!(matches!(refused, Err(AskpassError::Cancelled)));
            drop(server);
            // The waiting instance closes with the task; give the abort a moment.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let gone = request(&pipe, "Enter passphrase for key:", PromptKind::Secret).await;
            assert!(matches!(gone, Err(AskpassError::Io(_))), "{gone:?}");
            Ok(())
        })
        .await?
    }
}

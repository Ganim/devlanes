//! Windows relay: `SSH_ASKPASS` is the devlanes executable itself and prompts
//! travel over a named pipe that only the current user can open.
//!
//! Win32-OpenSSH (checked with 9.5p2) runs `SSH_ASKPASS` as `<program> <prompt>`
//! with `SSH_ASKPASS_REQUIRE=force` honoured without a console, so a script
//! wrapper is not needed: the marker variable tells the binary what it is for.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use devlanes_ipc::local::{LocalListener, LocalStream};
use devlanes_sys::windows::pipe;
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};
use tokio::task::{JoinHandle, JoinSet};

use super::{
    AskpassConfig, AskpassError, MARKER_ENV, PromptRelay, SOCKET_ENV, read_request, relay_reply,
};

/// How long the askpass process waits for a busy pipe instance.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Start of every relay pipe name; [`AskpassServer::start`] appends
/// `<pid>-<32 hex digits>`.
const PIPE_PREFIX: &str = r"\\.\pipe\devlanes-askpass-";

/// The ID of the process that created the relay pipe `name`, or `None` when
/// `name` is not exactly the shape [`AskpassServer::start`] creates. Anything
/// else is refused before it is opened: opening a UNC name (`\\host\pipe\x`)
/// would authenticate to that host with the user's NTLM credentials.
pub fn relay_process_id(name: &OsStr) -> Option<u32> {
    let (pid, token) = name.to_str()?.strip_prefix(PIPE_PREFIX)?.split_once('-')?;
    let digits = !pid.is_empty() && pid.bytes().all(|byte| byte.is_ascii_digit());
    let hex = token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if digits && hex {
        pid.parse().ok()
    } else {
        None
    }
}

/// Connects to the relay pipe. Refuses a name devlanes would not create, and a
/// pipe served by any process other than the one its name records: the pipe's
/// access list lets every process of the user add an instance, so another one
/// could otherwise answer prompts (a host key `yes` included).
pub(super) async fn connect(relay: &Path) -> Result<NamedPipeClient, AskpassError> {
    let owner = relay_process_id(relay.as_os_str())
        .ok_or_else(|| AskpassError::UnknownRelay(relay.display().to_string()))?;
    let client = tokio::time::timeout(CONNECT_TIMEOUT, open(relay.as_os_str()))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the askpass relay stayed busy"))??;
    if !pipe::server_is_current_user(&client)? || pipe::server_process_id(&client)? != owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the askpass relay is served by a process other than the one that created it",
        )
        .into());
    }
    Ok(client)
}

/// Opens the client end, waiting while the server is between instances.
async fn open(name: &OsStr) -> io::Result<NamedPipeClient> {
    loop {
        match ClientOptions::new().open(name) {
            Err(error) if error.raw_os_error() == Some(pipe::ERROR_PIPE_BUSY) => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            opened => return opened,
        }
    }
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
        let pipe = PathBuf::from(format!(
            "{PIPE_PREFIX}{}-{}",
            std::process::id(),
            crate::random_token()?
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

    /// Only the exact names the server creates pass; a UNC name is refused
    /// before anything is opened.
    #[tokio::test]
    async fn only_local_devlanes_relay_names_are_opened() {
        let token = "0123456789abcdef0123456789abcdef";
        let name = |text: String| OsString::from(text);
        assert_eq!(
            relay_process_id(&name(format!(r"\\.\pipe\devlanes-askpass-42-{token}"))),
            Some(42)
        );
        for bad in [
            format!(r"\\example.invalid\pipe\devlanes-askpass-42-{token}"),
            format!(r"\\?\pipe\devlanes-askpass-42-{token}"),
            format!(r"\\.\pipe\devlanes-askpass--{token}"),
            format!(r"\\.\pipe\devlanes-askpass-4x-{token}"),
            format!(r"\\.\pipe\devlanes-askpass-42-{}", token.to_uppercase()),
            format!(r"\\.\pipe\devlanes-askpass-42-{token}0"),
            format!(r"\\.\pipe\devlanes-askpass-42-{token}\..\x"),
            r"\\.\pipe\devlanes-askpass-42".to_owned(),
            r"\\.\pipe\other".to_owned(),
        ] {
            assert_eq!(relay_process_id(&name(bad.clone())), None, "{bad}");
        }
        let unc = PathBuf::from(format!(
            r"\\example.invalid\pipe\devlanes-askpass-42-{token}"
        ));
        let refused = request(&unc, "Password:", PromptKind::Secret).await;
        assert!(
            matches!(refused, Err(AskpassError::UnknownRelay(_))),
            "{refused:?}"
        );
    }

    /// A pipe whose server is not the process its name records is refused
    /// before the prompt is sent, as when another process of the user added an
    /// instance of the relay's pipe.
    #[tokio::test]
    async fn a_relay_served_by_another_process_is_refused() -> Result<(), Box<dyn std::error::Error>>
    {
        tokio::time::timeout(Duration::from_secs(10), async {
            use tokio::io::AsyncReadExt as _;
            let other = std::process::id().wrapping_add(1);
            let name = format!("{PIPE_PREFIX}{other}-{}", crate::random_token()?);
            let mut server = pipe::create_server(OsStr::new(&name), true)?;
            let refused = request(Path::new(&name), "Password:", PromptKind::Secret).await;
            match refused {
                Err(AskpassError::Io(error)) => {
                    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
            server.connect().await?;
            let mut sent = Vec::new();
            // The client is gone; a broken pipe ends the read like EOF does.
            let _ = server.read_to_end(&mut sent).await;
            assert!(sent.is_empty(), "the prompt reached the impostor");
            Ok(())
        })
        .await?
    }
}

//! The Windows relay: `SSH_ASKPASS` is `devlanes.exe` itself and prompts
//! travel over a named pipe that only the current user can open.
//!
//! Win32-OpenSSH runs `SSH_ASKPASS` as `<program> <prompt>` and honours
//! `SSH_ASKPASS_REQUIRE=force` without a console, and a script wrapper cannot
//! add the `askpass` subcommand there. So [`SOCKET_ENV`] carries the pipe name
//! and also tells the binary what it was started for (see [`is_relay_name`]).
//!
//! The pipe's name holds 128 random bits, so no other process can create it
//! first, and the server creates it as its first instance with the daemon
//! pipe's descriptor (this user only, medium integrity, no remote clients).
//! The askpass process opens only names of that exact shape, never a UNC
//! path, and talks only to a pipe owned by this user.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use devlanes_ipc::local::{self, ConnectError, LocalListener, LocalStream};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::task::{JoinHandle, JoinSet};

use super::{
    AskpassConfig, AskpassError, AskpassPrompt, MAX_LINE, PromptKind, PromptRelay, Reply, Request,
    SOCKET_ENV,
};

/// Start of every relay pipe name; [`AskpassServer::start`] appends
/// `<pid>-<32 hex digits>`.
const PIPE_PREFIX: &str = r"\\.\pipe\devlanes-askpass-";

/// How long the askpass process waits for the relay to take its connection.
/// The relay answers at once unless it is between two pipe instances.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `name` is exactly a relay pipe name [`AskpassServer::start`]
/// creates: `\\.\pipe\devlanes-askpass-<pid>-<32 lowercase hex digits>`.
/// Anything else is refused before it is opened, because opening a UNC name
/// (`\\host\pipe\x`) would send this user's credentials to that host.
pub fn is_relay_name(name: &OsStr) -> bool {
    let Some((pid, token)) = name
        .to_str()
        .and_then(|name| name.strip_prefix(PIPE_PREFIX))
        .and_then(|rest| rest.split_once('-'))
    else {
        return false;
    };
    !pid.is_empty()
        && pid.len() <= 10
        && pid.bytes().all(|byte| byte.is_ascii_digit())
        && token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Sends one prompt to the relay pipe `pipe` and waits for the answer. A name
/// of another shape is [`AskpassError::NotConfigured`]; a pipe this user does
/// not own is refused before the prompt is sent.
pub async fn request(pipe: &Path, prompt: &str, kind: PromptKind) -> Result<String, AskpassError> {
    if !is_relay_name(pipe.as_os_str()) {
        return Err(AskpassError::NotConfigured);
    }
    let stream = tokio::time::timeout(CONNECT_TIMEOUT, local::connect(pipe))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "the askpass relay stayed busy"))?
        .map_err(|error| match error {
            ConnectError::Io(error) => error,
            ConnectError::WrongPeer => io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the askpass relay pipe belongs to another user",
            ),
        })?;
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

/// A relay pipe that lives for one ssh authentication.
pub(crate) struct AskpassServer {
    pipe: PathBuf,
    exe: PathBuf,
    task: JoinHandle<()>,
}

impl AskpassServer {
    /// Creates the pipe and starts answering on it.
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
                        let stream = match accepted {
                            Ok(stream) => stream,
                            // The listener backs off itself when it cannot
                            // create the next instance.
                            Err(error) => {
                                tracing::debug!("askpass accept failed: {error}");
                                continue;
                            }
                        };
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

/// Answers one askpass connection through the relay. The pipe's descriptor
/// admits only this user, so the client needs no further check.
async fn answer(
    stream: LocalStream,
    relay: Arc<dyn PromptRelay>,
    destination: String,
) -> Result<(), AskpassError> {
    let (read, mut write) = tokio::io::split(stream);
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
    write.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use devlanes_sys_windows::pipe::PipeFactory;
    use devlanes_sys_windows::security::SecurityDescriptor;

    struct Fixed;

    #[async_trait]
    impl PromptRelay for Fixed {
        async fn prompt(&self, prompt: AskpassPrompt) -> Option<String> {
            (prompt.destination == "box" && prompt.prompt.contains("passphrase"))
                .then(|| "hunter2".to_owned())
        }
    }

    /// Prompts round-trip over the pipe and a refusal arrives as `Cancelled`;
    /// the environment names this executable and the pipe; once the server is
    /// dropped the pipe is gone.
    #[tokio::test]
    async fn prompts_round_trip_through_the_relay() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let config = AskpassConfig {
                exe: PathBuf::from(r"C:\Program Files\devlanes\devlanes.exe"),
                relay: Arc::new(Fixed),
            };
            let server = AskpassServer::start(&config, "box").await?;
            let env = server.env();
            let value = |key: &str| {
                env.iter()
                    .find(|(name, _)| *name == key)
                    .map(|(_, value)| value.clone())
            };
            assert_eq!(value("SSH_ASKPASS"), Some(config.exe.clone().into()));
            assert_eq!(value("SSH_ASKPASS_REQUIRE"), Some("force".into()));
            let pipe = PathBuf::from(value(SOCKET_ENV).ok_or("no pipe in the environment")?);
            assert!(is_relay_name(pipe.as_os_str()), "{}", pipe.display());

            let answer = request(&pipe, "Enter passphrase for key:", PromptKind::Secret).await?;
            assert_eq!(answer, "hunter2");
            let refused = request(&pipe, "Password:", PromptKind::Secret).await;
            assert!(matches!(refused, Err(AskpassError::Cancelled)), "{refused:?}");
            drop(server);
            // The waiting instance closes once the aborted task is dropped.
            let gone = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let result =
                        request(&pipe, "Enter passphrase for key:", PromptKind::Secret).await;
                    if matches!(&result, Err(AskpassError::Io(error)) if error.kind() == io::ErrorKind::NotFound)
                    {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await;
            assert!(gone.is_ok(), "the relay pipe outlived its server");
            Ok(())
        })
        .await?
    }

    /// Only the names the server creates pass; a UNC name or any other shape
    /// is refused before anything is opened.
    #[tokio::test]
    async fn only_local_relay_names_are_opened() {
        let token = "0123456789abcdef0123456789abcdef";
        assert!(is_relay_name(OsStr::new(&format!(
            r"\\.\pipe\devlanes-askpass-42-{token}"
        ))));
        for bad in [
            format!(r"\\example.invalid\pipe\devlanes-askpass-42-{token}"),
            format!(r"\\?\pipe\devlanes-askpass-42-{token}"),
            format!(r"\\.\PIPE\devlanes-askpass-42-{token}"),
            format!(r"\\.\pipe\devlanes-askpass--{token}"),
            format!(r"\\.\pipe\devlanes-askpass-4x-{token}"),
            format!(r"\\.\pipe\devlanes-askpass-42-{}", token.to_uppercase()),
            format!(r"\\.\pipe\devlanes-askpass-42-{token}0"),
            format!(r"\\.\pipe\devlanes-askpass-42-{token}\..\x"),
            format!(r"\\.\pipe\devlanes-askpass-12345678901-{token}"),
            r"\\.\pipe\devlanes-askpass-42".to_owned(),
            r"\\.\pipe\other".to_owned(),
            String::new(),
        ] {
            assert!(!is_relay_name(OsStr::new(&bad)), "{bad}");
        }
        let unc = PathBuf::from(format!(
            r"\\example.invalid\pipe\devlanes-askpass-42-{token}"
        ));
        let refused = request(&unc, "Password:", PromptKind::Secret).await;
        assert!(
            matches!(refused, Err(AskpassError::NotConfigured)),
            "{refused:?}"
        );
    }

    /// A pipe of the right shape that this user does not own is refused
    /// before the prompt is sent: the owner check runs on the open handle.
    #[tokio::test]
    async fn a_relay_owned_by_someone_else_is_refused() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let name = format!(
                "{PIPE_PREFIX}{}-{}",
                std::process::id(),
                crate::random_token()?
            );
            let user = devlanes_sys_windows::token::current_user()?;
            // Owned by Administrators, which only an elevated process can set
            // (as on CI runners); the descriptor still lets this user open it.
            // Unelevated, the impostor cannot be built and the test has
            // nothing to check.
            let descriptor = SecurityDescriptor::from_sddl(&format!("O:BAD:P(A;;GA;;;{user})"))?;
            let Ok(server) = PipeFactory::new(&name, descriptor).create(true) else {
                eprintln!("skipping: this process cannot create a pipe owned by Administrators");
                return Ok(());
            };
            let refused = request(Path::new(&name), "Password:", PromptKind::Secret).await;
            match refused {
                Err(AskpassError::Io(error)) => {
                    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied, "{error}");
                }
                other => panic!("expected a refusal, got {other:?}"),
            }
            drop(server);
            Ok(())
        })
        .await?
    }
}

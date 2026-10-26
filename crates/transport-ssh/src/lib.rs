//! SSH [`devlanes_transport::Transport`] over system OpenSSH.
//!
//! On Unix, one ControlMaster per box lives under the given control directory
//! and each session runs `devlanes tunnel --stdio` on the box over it. Windows'
//! OpenSSH has no ControlMaster, so there each session is one `ssh.exe` that
//! probes the box and then execs the tunnel. Prompts go through [`askpass`]; a
//! box without devlanes fails with [`SshError::NotInstalled`].

pub mod askpass;
mod probe;
#[cfg(unix)]
mod ssh;
#[cfg(windows)]
#[path = "ssh_windows.rs"]
mod ssh;

use std::io;
use std::process::ExitStatus;

pub use askpass::{
    AskpassConfig, AskpassError, AskpassPrompt, PromptKind, PromptRelay, run_askpass,
};
pub use probe::{INSTALL_DIR, RemotePlatform, RemoteProbe};
pub use ssh::{SshError, SshOptions, SshStream, SshTransport};

/// How long a started tunnel gets to write its preamble line. It writes the
/// line before anything else, so only a box that runs an older release (whose
/// tunnel waits for the client) or a stuck login shell takes this long.
pub(crate) const PREAMBLE_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// Quotes `value` for a POSIX shell using single quotes.
pub(crate) fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// 128 bits from the OS random source as 32 lowercase hex digits, for names
/// another process must not predict: relay pipes and session sentinels.
#[cfg(windows)]
pub(crate) fn random_token() -> std::io::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// How the remote tunnel process ended.
pub(crate) struct ExitInfo {
    /// Its exit status, or why waiting for it failed.
    pub(crate) status: Result<ExitStatus, String>,
    /// The tail of its stderr.
    pub(crate) stderr: String,
}

/// Why the box's `devlanes tunnel --stdio` ended with an error. A stream
/// keeps it, so every read after the end reports the same failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TunnelExit {
    /// No box daemon ran and the box's user config turns off starting one.
    DaemonStopped,
    /// Any other failure, described with the tunnel's stderr.
    Failed(String),
}

impl TunnelExit {
    /// Classifies a finished tunnel; `None` for a clean exit.
    pub(crate) fn from_info(info: ExitInfo) -> Option<Self> {
        let detail = if info.stderr.is_empty() {
            String::new()
        } else {
            format!(": {}", info.stderr)
        };
        match info.status {
            Ok(status) if status.success() => None,
            Ok(status) if status.code() == Some(devlanes_transport::EXIT_DAEMON_STOPPED) => {
                Some(Self::DaemonStopped)
            }
            Ok(status) => Some(Self::Failed(format!(
                "remote `devlanes tunnel --stdio` exited with {status}{detail}"
            ))),
            Err(error) => Some(Self::Failed(format!(
                "remote `devlanes tunnel --stdio` failed: {error}{detail}"
            ))),
        }
    }

    /// The read error a stream to `destination` reports. A stopped daemon
    /// carries [`SshError::BoxDaemonStopped`], whose message the link shows
    /// while it keeps retrying.
    pub(crate) fn to_io(&self, destination: &str) -> io::Error {
        match self {
            Self::DaemonStopped => io::Error::new(
                io::ErrorKind::ConnectionAborted,
                SshError::BoxDaemonStopped(destination.to_owned()),
            ),
            Self::Failed(message) => {
                io::Error::new(io::ErrorKind::ConnectionAborted, message.clone())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_embedded_single_quotes() {
        assert_eq!(super::sh_quote("it's"), r"'it'\''s'");
    }

    /// An exit status built from a process exit code.
    #[cfg(unix)]
    fn exited(code: i32) -> ExitStatus {
        use std::os::unix::process::ExitStatusExt as _;
        ExitStatus::from_raw(code << 8)
    }

    /// An exit status built from a process exit code.
    #[cfg(windows)]
    fn exited(code: i32) -> ExitStatus {
        use std::os::windows::process::ExitStatusExt as _;
        ExitStatus::from_raw(code.cast_unsigned())
    }

    /// The tunnel's daemon-stopped status becomes a transient error that names
    /// the box and the fix; other failures keep the tunnel's stderr; a clean
    /// exit is no error.
    #[test]
    fn a_stopped_box_daemon_is_transient() {
        let info = |code: i32| ExitInfo {
            status: Ok(exited(code)),
            stderr: "devlanes tunnel: box daemon is not running".to_owned(),
        };
        let stopped = TunnelExit::from_info(info(devlanes_transport::EXIT_DAEMON_STOPPED));
        assert_eq!(stopped, Some(TunnelExit::DaemonStopped));
        let error = stopped.map(|exit| exit.to_io("acme")).expect("an error");
        let inner = error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<SshError>())
            .expect("an ssh error");
        assert!(matches!(inner, SshError::BoxDaemonStopped(host) if host == "acme"));
        assert!(!inner.is_permanent());
        let message = error.to_string();
        assert!(message.contains("acme"), "{message}");
        assert!(message.contains("`devlanes daemon start`"), "{message}");
        assert!(message.contains("`devlanes daemon install`"), "{message}");
        let transport =
            devlanes_transport::TransportError::from(SshError::BoxDaemonStopped("acme".to_owned()));
        assert!(!transport.is_permanent());

        let Some(TunnelExit::Failed(message)) = TunnelExit::from_info(info(1)) else {
            panic!("exit 1 is a failure");
        };
        assert!(message.contains("box daemon is not running"), "{message}");
        assert_eq!(TunnelExit::from_info(info(0)), None);
    }

    /// A box without devlanes points at the setup guide, and the error's
    /// permanence decides whether the link stops.
    #[test]
    fn a_missing_binary_points_at_the_setup_guide() {
        let error = SshError::NotInstalled {
            destination: "acme".to_owned(),
            permanent: true,
        };
        assert!(error.is_permanent());
        let message = error.to_string();
        assert!(
            message.starts_with("devlanes is not installed on acme"),
            "{message}"
        );
        assert!(
            message.contains("docs/guides/remote-setup.md#on-the-box"),
            "{message}"
        );
        assert!(devlanes_transport::TransportError::from(error).is_permanent());
    }

    /// The first connect yields a pipe that fails like a tunnel whose box
    /// daemon is stopped; later connects reach a working box.
    struct StoppedThenRunning {
        box_side: devlanes_transport::memory::MemoryTransport,
        attempts: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl devlanes_transport::Transport for StoppedThenRunning {
        async fn connect(
            &self,
        ) -> Result<devlanes_transport::BoxDuplex, devlanes_transport::TransportError> {
            if self
                .attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                == 0
            {
                return Ok(Box::new(Stopped));
            }
            self.box_side.connect().await
        }
    }

    /// A tunnel that exited with the daemon-stopped status: every read fails.
    struct Stopped;

    impl tokio::io::AsyncRead for Stopped {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Err(TunnelExit::DaemonStopped.to_io("acme")))
        }
    }

    impl tokio::io::AsyncWrite for Stopped {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A stopped box daemon keeps the link retrying with the hint as its error,
    /// and the link connects once the box serves again.
    #[tokio::test]
    async fn a_stopped_box_daemon_retries_until_it_runs() -> Result<(), Box<dyn std::error::Error>>
    {
        use devlanes_transport::{
            Backoff, BoxServices, Link, LinkState, LoopbackDialer, NoSync, RegistrySnapshot,
            ServeOptions, SessionOptions, SharedRegistry,
        };
        use std::sync::Arc;
        use std::time::Duration;

        tokio::time::timeout(Duration::from_secs(20), async {
            let (box_side, mut listener) = devlanes_transport::memory::memory_transport();
            let serving = tokio::spawn(async move {
                while let Some(stream) = listener.accept().await {
                    let services = BoxServices {
                        registry: Arc::new(SharedRegistry::new(RegistrySnapshot::default(), 4)),
                        dialer: Arc::new(LoopbackDialer),
                        sync: Arc::new(NoSync),
                    };
                    tokio::spawn(devlanes_transport::serve(
                        stream,
                        services,
                        ServeOptions::new("box"),
                    ));
                }
            });
            let transport = Arc::new(StoppedThenRunning {
                box_side,
                attempts: std::sync::atomic::AtomicUsize::new(0),
            });
            let link = Link::spawn(
                transport,
                SessionOptions::new("client"),
                Backoff::new(Duration::from_millis(200), Duration::from_millis(200)),
            );
            let mut state = link.state();
            let mut retried = None;
            loop {
                match &*state.borrow_and_update() {
                    LinkState::Retrying { error, .. } if retried.is_none() => {
                        retried = Some(error.clone());
                    }
                    LinkState::Connected(_) => break,
                    LinkState::Failed { error } => panic!("the link gave up: {error}"),
                    _ => {}
                }
                state.changed().await?;
            }
            let error = retried.ok_or("connected without retrying")?;
            assert!(
                error.contains("the devlanes daemon is not running on acme"),
                "{error}"
            );
            assert!(error.contains("`devlanes daemon start`"), "{error}");
            link.shutdown().await;
            serving.abort();
            Ok(())
        })
        .await?
    }
}

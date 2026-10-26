//! Keeps a session alive across transport failures.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::registry::ClientRegistry;
use crate::session::{Session, SessionError, SessionOptions};
use crate::transport::Transport;

/// Exponential backoff without jitter; one client per box makes jitter moot.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    current: Option<Duration>,
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(Duration::from_millis(500), Duration::from_secs(30))
    }
}

impl Backoff {
    /// Starts at `initial`, doubling up to `max`.
    pub fn new(initial: Duration, max: Duration) -> Self {
        Self {
            initial,
            max: max.max(initial),
            current: None,
        }
    }

    /// Returns the next delay and advances.
    pub fn next_delay(&mut self) -> Duration {
        let delay = match self.current {
            None => self.initial,
            Some(current) => current.saturating_mul(2).min(self.max),
        };
        self.current = Some(delay);
        delay
    }

    /// Restarts from `initial`, after a successful connection.
    pub fn reset(&mut self) {
        self.current = None;
    }
}

/// Observable state of a [`Link`].
#[derive(Debug, Clone)]
pub enum LinkState {
    /// Attempting a connection; `attempt` counts since the last success.
    Connecting {
        /// 1-based attempt number.
        attempt: u32,
    },
    /// A session is up.
    Connected(Session),
    /// Waiting before the next attempt.
    Retrying {
        /// Wait before the next attempt.
        delay: Duration,
        /// Why the previous attempt or session ended.
        error: String,
    },
    /// Waiting for a person to sign in again ([`Link::resume`]) after the
    /// transport reported [`crate::TransportError::NeedsAuth`]. Nothing is
    /// retried meanwhile, and the registry keeps the last known state.
    NeedsAuth {
        /// Why the attempt needed a person.
        error: String,
    },
    /// Stopped because retrying cannot help, such as a protocol mismatch.
    Failed {
        /// The permanent failure.
        error: String,
    },
    /// Stopped by [`Link::shutdown`] or drop.
    Stopped,
}

/// Why [`Link::connected`] gave up.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LinkError {
    /// A permanent failure stopped the link.
    #[error("{0}")]
    Failed(String),
    /// The link was shut down.
    #[error("link stopped")]
    Stopped,
}

/// A supervised connection to one box with reconnect and backoff.
///
/// The registry is shared across sessions: it keeps the last known state while
/// disconnected, so local listeners can stay bound, and each new session's
/// snapshot replaces it.
pub struct Link {
    state: watch::Receiver<LinkState>,
    /// Shared with the supervisor so [`Link::resume`] can leave
    /// [`LinkState::NeedsAuth`] itself, before the supervisor wakes up.
    state_tx: Arc<watch::Sender<LinkState>>,
    registry: ClientRegistry,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Link {
    /// Starts connecting in the background.
    pub fn spawn(transport: Arc<dyn Transport>, options: SessionOptions, backoff: Backoff) -> Self {
        let (state_tx, state) = watch::channel(LinkState::Connecting { attempt: 1 });
        let state_tx = Arc::new(state_tx);
        let cancel = CancellationToken::new();
        let registry = options.registry.clone();
        let task = tokio::spawn(supervise(
            transport,
            options,
            backoff,
            state_tx.clone(),
            cancel.clone(),
        ));
        Self {
            state,
            state_tx,
            registry,
            cancel,
            task: Some(task),
        }
    }

    /// Subscribes to state changes.
    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.state.clone()
    }

    /// Makes a link waiting in [`LinkState::NeedsAuth`] try again now, with a
    /// fresh backoff. The state is [`LinkState::Connecting`] when this returns
    /// true; false means the link was not waiting.
    pub fn resume(&self) -> bool {
        self.state_tx.send_if_modified(|state| {
            if matches!(state, LinkState::NeedsAuth { .. }) {
                *state = LinkState::Connecting { attempt: 1 };
                true
            } else {
                false
            }
        })
    }

    /// Registry state shared by every session of this link.
    pub fn registry(&self) -> &ClientRegistry {
        &self.registry
    }

    /// The current session, if connected.
    pub fn session(&self) -> Option<Session> {
        match &*self.state.borrow() {
            LinkState::Connected(session) if session.end().is_none() => Some(session.clone()),
            _ => None,
        }
    }

    /// Waits for a live session, or for the link to stop. A link waiting for
    /// sign-in keeps this waiting until it is resumed.
    pub async fn connected(&self) -> Result<Session, LinkError> {
        let mut state = self.state.clone();
        loop {
            match &*state.borrow_and_update() {
                LinkState::Connected(session) if session.end().is_none() => {
                    return Ok(session.clone());
                }
                LinkState::Failed { error } => return Err(LinkError::Failed(error.clone())),
                LinkState::Stopped => return Err(LinkError::Stopped),
                _ => {}
            }
            if state.changed().await.is_err() {
                return Err(LinkError::Stopped);
            }
        }
    }

    /// Stops reconnecting, closes the session and waits for the supervisor.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Connect, wait for the session to end, back off, repeat. A failure that
/// needs a person parks the link in [`LinkState::NeedsAuth`] until
/// [`Link::resume`] moves it on.
async fn supervise(
    transport: Arc<dyn Transport>,
    options: SessionOptions,
    mut backoff: Backoff,
    state: Arc<watch::Sender<LinkState>>,
    cancel: CancellationToken,
) {
    let mut attempt = 0u32;
    loop {
        attempt = attempt.saturating_add(1);
        state.send_replace(LinkState::Connecting { attempt });
        let connecting = async {
            let io = transport.connect().await?;
            Session::establish(io, options.clone()).await
        };
        // A transport that hangs (a stuck ssh, a dead network path) must not
        // stall the link forever; the attempt fails and the backoff continues.
        let limited = async {
            tokio::time::timeout(options.connect_timeout, connecting)
                .await
                .unwrap_or(Err(SessionError::ConnectTimeout))
        };
        let result: Result<Session, SessionError> = tokio::select! {
            result = limited => result,
            () = cancel.cancelled() => break,
        };
        let error = match result {
            Ok(session) => {
                backoff.reset();
                attempt = 0;
                state.send_replace(LinkState::Connected(session.clone()));
                let end = tokio::select! {
                    end = session.closed() => end,
                    () = cancel.cancelled() => {
                        session.close();
                        break;
                    }
                };
                end.to_string()
            }
            Err(error) if error.is_permanent() => {
                state.send_replace(LinkState::Failed {
                    error: error.to_string(),
                });
                return;
            }
            Err(error) if error.is_needs_auth() => {
                tracing::debug!("waiting for sign-in: {error}");
                state.send_replace(LinkState::NeedsAuth {
                    error: error.to_string(),
                });
                let mut resumed = state.subscribe();
                tokio::select! {
                    changed = resumed.wait_for(|current| !matches!(current, LinkState::NeedsAuth { .. })) => {
                        if changed.is_err() {
                            break;
                        }
                    }
                    () = cancel.cancelled() => break,
                }
                backoff.reset();
                attempt = 0;
                continue;
            }
            Err(error) => error.to_string(),
        };
        let delay = backoff.next_delay();
        tracing::debug!("reconnecting in {delay:?}: {error}");
        state.send_replace(LinkState::Retrying { delay, error });
        tokio::select! {
            () = tokio::time::sleep(delay) => {}
            () = cancel.cancelled() => break,
        }
    }
    state.send_replace(LinkState::Stopped);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A transport whose connect never finishes, like an ssh stuck on the network.
    struct Hanging;

    #[async_trait::async_trait]
    impl Transport for Hanging {
        async fn connect(&self) -> Result<crate::BoxDuplex, crate::TransportError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn a_hanging_connect_is_retried() -> Result<(), Box<dyn std::error::Error>> {
        let mut options = SessionOptions::new("client");
        options.connect_timeout = Duration::from_millis(100);
        let link = Link::spawn(
            Arc::new(Hanging),
            options,
            Backoff::new(Duration::from_millis(10), Duration::from_millis(10)),
        );
        let mut state = link.state();
        let retried = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let LinkState::Retrying { error, .. } = &*state.borrow_and_update() {
                    return error.clone();
                }
                if state.changed().await.is_err() {
                    return String::new();
                }
            }
        })
        .await?;
        assert!(retried.contains("timed out"), "{retried}");
        link.shutdown().await;
        Ok(())
    }

    /// A transport whose every attempt needs a person to sign in, counting attempts.
    #[derive(Default)]
    struct SignInRequired {
        attempts: std::sync::atomic::AtomicU32,
    }

    #[async_trait::async_trait]
    impl Transport for SignInRequired {
        async fn connect(&self) -> Result<crate::BoxDuplex, crate::TransportError> {
            self.attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(crate::TransportError::needs_auth(
                "Permission denied (publickey)",
            ))
        }
    }

    /// Waits until the link reports [`LinkState::NeedsAuth`] and returns its error.
    async fn needs_auth(state: &mut watch::Receiver<LinkState>) -> String {
        loop {
            if let LinkState::NeedsAuth { error } = &*state.borrow_and_update() {
                return error.clone();
            }
            if state.changed().await.is_err() {
                return String::new();
            }
        }
    }

    /// A failure that needs a person parks the link without retrying until
    /// `resume`, which tries once more; `resume` does nothing otherwise.
    #[tokio::test]
    async fn needing_sign_in_waits_for_resume() -> Result<(), Box<dyn std::error::Error>> {
        let transport = Arc::new(SignInRequired::default());
        let link = Link::spawn(
            transport.clone(),
            SessionOptions::new("client"),
            Backoff::new(Duration::from_millis(10), Duration::from_millis(10)),
        );
        let mut state = link.state();
        let attempts = || transport.attempts.load(std::sync::atomic::Ordering::SeqCst);
        tokio::time::timeout(Duration::from_secs(5), async {
            let error = needs_auth(&mut state).await;
            assert!(error.contains("Permission denied"), "{error}");
            // Twenty backoff periods: a retrying link would have tried again.
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(attempts(), 1);
            assert!(matches!(*state.borrow(), LinkState::NeedsAuth { .. }));

            assert!(link.resume());
            assert!(matches!(
                *link.state().borrow(),
                LinkState::Connecting { .. }
            ));
            state.borrow_and_update();
            needs_auth(&mut state).await;
            assert_eq!(attempts(), 2);
        })
        .await?;
        link.shutdown().await;
        let connecting = Link::spawn(
            Arc::new(Hanging),
            SessionOptions::new("client"),
            Backoff::default(),
        );
        assert!(
            !connecting.resume(),
            "a link that is not waiting is not resumed"
        );
        connecting.shutdown().await;
        Ok(())
    }

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(350));
        let delays: Vec<_> = (0..4).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, [100, 200, 350, 350]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
    }
}

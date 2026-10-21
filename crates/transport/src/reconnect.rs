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
    registry: ClientRegistry,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl Link {
    /// Starts connecting in the background.
    pub fn spawn(transport: Arc<dyn Transport>, options: SessionOptions, backoff: Backoff) -> Self {
        let (state_tx, state) = watch::channel(LinkState::Connecting { attempt: 1 });
        let cancel = CancellationToken::new();
        let registry = options.registry.clone();
        let task = tokio::spawn(supervise(
            transport,
            options,
            backoff,
            state_tx,
            cancel.clone(),
        ));
        Self {
            state,
            registry,
            cancel,
            task: Some(task),
        }
    }

    /// Subscribes to state changes.
    pub fn state(&self) -> watch::Receiver<LinkState> {
        self.state.clone()
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

    /// Waits for a live session, or for the link to stop.
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

/// Connect, wait for the session to end, back off, repeat.
async fn supervise(
    transport: Arc<dyn Transport>,
    options: SessionOptions,
    mut backoff: Backoff,
    state: watch::Sender<LinkState>,
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
        let result: Result<Session, SessionError> = tokio::select! {
            result = connecting => result,
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

    #[test]
    fn backoff_doubles_to_cap_and_resets() {
        let mut backoff = Backoff::new(Duration::from_millis(100), Duration::from_millis(350));
        let delays: Vec<_> = (0..4).map(|_| backoff.next_delay().as_millis()).collect();
        assert_eq!(delays, [100, 200, 350, 350]);
        backoff.reset();
        assert_eq!(backoff.next_delay(), Duration::from_millis(100));
    }
}

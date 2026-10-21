use std::{
    collections::BTreeMap,
    io,
    net::SocketAddr,
    sync::{Arc, Mutex, PoisonError},
};

/// Live state of one accept loop, for doctor output that reflects more than the initial bind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenerHealth {
    /// Local address of the listening socket.
    pub address: SocketAddr,
    /// Whether the listener terminates TLS.
    pub tls: bool,
    /// False once the accept loop has returned, for example after cancellation.
    pub running: bool,
    /// Accept failures since the listener started; the loop retries after each one.
    pub accept_errors: u64,
    /// The latest accept failure, cleared by the next successful accept.
    pub last_error: Option<String>,
}

pub(crate) type Health = Arc<Mutex<BTreeMap<SocketAddr, ListenerHealth>>>;

/// Owned by one accept loop; dropping it marks the listener stopped.
pub(crate) struct ListenerGuard {
    health: Health,
    address: SocketAddr,
    faulted: bool,
}

impl ListenerGuard {
    /// Registers the listener as running when its accept loop starts.
    pub(crate) fn new(health: Health, address: SocketAddr, tls: bool) -> Self {
        health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                address,
                ListenerHealth {
                    address,
                    tls,
                    running: true,
                    accept_errors: 0,
                    last_error: None,
                },
            );
        Self {
            health,
            address,
            faulted: false,
        }
    }

    /// Records an accept failure while the loop keeps the socket and retries.
    pub(crate) fn failed(&mut self, error: &io::Error) {
        self.faulted = true;
        self.update(|state| {
            state.accept_errors = state.accept_errors.saturating_add(1);
            state.last_error = Some(error.to_string());
        });
    }

    /// Clears the current fault; locks only after a failure so accepts stay cheap.
    pub(crate) fn recovered(&mut self) {
        if std::mem::take(&mut self.faulted) {
            self.update(|state| state.last_error = None);
        }
    }

    /// Applies a change to this listener's entry.
    fn update(&self, change: impl FnOnce(&mut ListenerHealth)) {
        if let Some(state) = self
            .health
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&self.address)
        {
            change(state);
        }
    }
}

impl Drop for ListenerGuard {
    /// Covers cancellation, errors and aborted serving tasks alike.
    fn drop(&mut self) {
        self.update(|state| state.running = false);
    }
}

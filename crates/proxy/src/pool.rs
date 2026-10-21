use std::{
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use hyper::{body::Incoming, client::conn::http1::SendRequest};
use tokio_util::sync::CancellationToken;

use crate::Upstream;

/// Kept below Node's default 5 second keep-alive so the upstream rarely closes a socket we hand out.
const IDLE_TTL: Duration = Duration::from_secs(4);
/// Matches the downstream HTTP/2 stream limit, so one browser burst can be fully reused.
const PER_TARGET: usize = crate::server::MAX_H2_STREAMS as usize;
const TOTAL: usize = 256;

/// An HTTP/1.1 upstream connection and the authority used for `HostHeader::Upstream`.
#[derive(Debug)]
pub(crate) struct Connection {
    pub sender: SendRequest<Incoming>,
    pub authority: String,
}

#[derive(Debug)]
struct Idle {
    id: u64,
    target: Upstream,
    expires: Instant,
    connection: Connection,
}

/// Idle upstream connections keyed by the exact dial target, never shared across targets.
#[derive(Debug, Default)]
pub(crate) struct Pool {
    idle: Mutex<Vec<Idle>>,
    next_id: AtomicU64,
}

impl Pool {
    /// Takes an idle connection to `target`, discarding closed or expired ones first.
    pub(crate) fn take(&self, target: &Upstream) -> Option<Connection> {
        let mut idle = self.idle.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        idle.retain(|entry| entry.expires > now && !entry.connection.sender.is_closed());
        let index = idle
            .iter()
            .position(|entry| &entry.target == target && entry.connection.sender.is_ready())?;
        Some(idle.swap_remove(index).connection)
    }

    /// Returns a connection once its response body is fully read; failed or upgraded ones are dropped.
    pub(crate) fn recycle(
        self: &Arc<Self>,
        target: Upstream,
        mut connection: Connection,
        shutdown: CancellationToken,
    ) {
        let pool = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                ready = connection.sender.ready() => if ready.is_err() { return; },
            }
            let id = pool.next_id.fetch_add(1, Ordering::Relaxed);
            {
                let mut idle = pool.idle.lock().unwrap_or_else(PoisonError::into_inner);
                if idle.len() >= TOTAL
                    || idle.iter().filter(|entry| entry.target == target).count() >= PER_TARGET
                {
                    return;
                }
                idle.push(Idle {
                    id,
                    target,
                    expires: Instant::now() + IDLE_TTL,
                    connection,
                });
            }
            // Closes the socket at expiry even when no later request calls `take`.
            tokio::select! {
                _ = shutdown.cancelled() => {},
                _ = tokio::time::sleep(IDLE_TTL) => {},
            }
            pool.idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|entry| entry.id != id);
        });
    }
}

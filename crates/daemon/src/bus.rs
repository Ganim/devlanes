//! Event bus (plan 3.4). Every subscriber reads from its own bounded broadcast
//! position; a subscriber that falls behind gets one `resync` event instead of the
//! events it missed, then continues with live events.

use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use devlanes_proto::{Event, events};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::Session;

/// Per-subscriber queue length before overflow turns into `resync`.
pub const SUBSCRIBER_QUEUE: usize = 1024;
/// Recent events kept for non-following `devlanes events`.
const RECENT: usize = 200;

/// Sequenced publisher shared by the whole engine.
pub struct EventBus {
    sender: broadcast::Sender<Event>,
    seq: AtomicU64,
    recent: Mutex<VecDeque<Event>>,
}

impl Default for EventBus {
    /// Uses [`SUBSCRIBER_QUEUE`] as the per-subscriber bound.
    fn default() -> Self {
        Self::with_capacity(SUBSCRIBER_QUEUE)
    }
}

impl EventBus {
    /// Bus whose subscribers may lag by `capacity` events before a resync.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            sender: broadcast::channel(capacity.max(1)).0,
            seq: AtomicU64::new(1),
            recent: Mutex::new(VecDeque::with_capacity(RECENT)),
        }
    }

    /// Publishes an event; having no subscribers is normal.
    pub fn emit(&self, name: &str, data: Value) {
        let event = Event {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            event: name.to_owned(),
            data,
        };
        if let Ok(mut recent) = self.recent.lock() {
            if recent.len() == RECENT {
                recent.pop_front();
            }
            recent.push_back(event.clone());
        }
        let _ = self.sender.send(event);
    }

    /// A new subscription positioned at the next event.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }

    /// The last events, oldest first.
    pub fn recent(&self) -> Vec<Event> {
        self.recent
            .lock()
            .map(|recent| recent.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Current subscriber count, used to decide whether sampling is wanted.
    pub fn subscribers(&self) -> usize {
        self.sender.receiver_count()
    }

    /// Forwards events accepted by `filter` to `session` until it disconnects.
    pub async fn forward(&self, session: &Session, filter: impl Fn(&Event) -> bool) {
        let mut receiver = self.subscribe();
        let disconnected = session.disconnected();
        loop {
            let next = tokio::select! {
                () = disconnected.cancelled() => return,
                next = receiver.recv() => next,
            };
            let event = match next {
                Ok(event) if filter(&event) => event,
                Ok(_) => continue,
                Err(broadcast::error::RecvError::Lagged(missed)) => Event {
                    seq: self.seq.load(Ordering::Relaxed),
                    event: events::RESYNC.into(),
                    data: json!({"missed": missed}),
                },
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if session.event(event).await.is_err() {
                return;
            }
        }
    }
}

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
/// Recent events kept for non-following `devlanes debug events`.
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

    /// Publishes an event; having no subscribers is normal. The sequence number
    /// is taken and the event sent under one lock, so subscribers receive events
    /// in sequence order even when several threads emit at once.
    pub fn emit(&self, name: &str, data: Value) {
        let mut recent = crate::model::lock(&self.recent);
        let event = Event {
            seq: self.seq.fetch_add(1, Ordering::Relaxed),
            event: name.to_owned(),
            data,
        };
        if recent.len() == RECENT {
            recent.pop_front();
        }
        recent.push_back(event.clone());
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
        self.forward_from(session, |_| Vec::new(), filter).await;
    }

    /// Like [`Self::forward`], but subscribes first and then sends `initial` to
    /// this session only, so a snapshot never misses a change and other
    /// subscribers never see it. `initial` gets a sequence number of its own,
    /// after every event the subscription missed and before every one it gets.
    pub async fn forward_from(
        &self,
        session: &Session,
        initial: impl FnOnce(u64) -> Vec<Event>,
        filter: impl Fn(&Event) -> bool,
    ) {
        let (mut receiver, seq) = {
            // Emits take their number under this lock, so none falls in between.
            let _emits = crate::model::lock(&self.recent);
            (self.subscribe(), self.seq.fetch_add(1, Ordering::Relaxed))
        };
        let disconnected = session.disconnected();
        for event in initial(seq) {
            if session.event(event).await.is_err() {
                return;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Events emitted from many threads at once arrive in sequence order.
    fn concurrent_emits_arrive_in_sequence_order() {
        let bus = std::sync::Arc::new(EventBus::with_capacity(64 * 1024));
        let mut receiver = bus.subscribe();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let bus = bus.clone();
                std::thread::spawn(move || {
                    for _ in 0..2000 {
                        bus.emit("test", Value::Null);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().expect("emitter");
        }
        let mut last = 0;
        let mut count = 0;
        while let Ok(event) = receiver.try_recv() {
            assert!(event.seq > last, "{} after {last}", event.seq);
            last = event.seq;
            count += 1;
        }
        assert_eq!(count, 16_000);
    }
}

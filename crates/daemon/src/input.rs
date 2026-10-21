//! Terminal input from one connection. Each attach stream gets a bounded queue
//! and its own writer task, so a unit that stops reading stdin stalls only its
//! own stream, never the connection's requests or other streams. Bytes keep their
//! order per stream; a full queue drops its oldest input and tells the client.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use bytes::Bytes;
use devlanes_proto::{Event, events};
use serde_json::json;
use tokio::sync::Notify;

use crate::{Handler, Session, model::lock};

/// Frames a stream may queue before the oldest is dropped.
pub(crate) const MAX_FRAMES: usize = 1024;
/// Bytes a stream may queue before the oldest frames are dropped; a single larger
/// frame is still kept.
pub(crate) const MAX_BYTES: usize = 1 << 20;

/// Queued input of one stream.
#[derive(Default)]
struct State {
    items: VecDeque<Bytes>,
    bytes: usize,
    /// The stream rejected input; later frames are discarded.
    closed: bool,
    /// An overflow warning went out and the queue has not emptied since.
    warned: bool,
}

/// One stream's queue and the signal that wakes its writer.
#[derive(Default)]
struct Queue {
    state: Mutex<State>,
    ready: Notify,
}

/// The input queues of one connection.
pub(crate) struct Inputs {
    handler: Arc<dyn Handler>,
    session: Session,
    streams: HashMap<u64, Arc<Queue>>,
}

impl Inputs {
    /// Queues for the connection behind `session`; writers end when it disconnects.
    pub(crate) fn new(handler: Arc<dyn Handler>, session: Session) -> Self {
        Self {
            handler,
            session,
            streams: HashMap::new(),
        }
    }

    /// Queues `data` for `stream_id` without waiting on the unit.
    pub(crate) fn push(&mut self, stream_id: u64, data: Bytes) {
        let queue = self
            .streams
            .entry(stream_id)
            .or_insert_with(|| {
                let queue = Arc::new(Queue::default());
                tokio::spawn(write(
                    queue.clone(),
                    stream_id,
                    self.handler.clone(),
                    self.session.clone(),
                ));
                queue
            })
            .clone();
        let (frames, bytes, warn) = {
            let mut state = lock(&queue.state);
            if state.closed {
                return;
            }
            state.bytes += data.len();
            state.items.push_back(data);
            let (mut frames, mut bytes) = (0, 0);
            while state.items.len() > 1
                && (state.items.len() > MAX_FRAMES || state.bytes > MAX_BYTES)
            {
                if let Some(old) = state.items.pop_front() {
                    state.bytes -= old.len();
                    frames += 1;
                    bytes += old.len();
                }
            }
            let warn = frames > 0 && !state.warned;
            state.warned |= warn;
            (frames, bytes, warn)
        };
        queue.ready.notify_one();
        if warn {
            tracing::warn!(
                stream_id,
                frames,
                bytes,
                "attach input dropped: the unit is not reading"
            );
            let session = self.session.clone();
            // Sending waits for this connection's writer, which runs the caller's loop.
            tokio::spawn(async move {
                dropped(&session, stream_id, "input_overflow", frames, bytes).await;
            });
        }
    }
}

/// Tells the client that input of `stream_id` was discarded.
async fn dropped(session: &Session, stream_id: u64, code: &str, frames: usize, bytes: usize) {
    let event = Event {
        seq: 0,
        event: events::INPUT_DROPPED.into(),
        data: json!({
            "stream_id": stream_id,
            "code": code,
            "dropped_frames": frames,
            "dropped_bytes": bytes,
        }),
    };
    let _ = session.event(event).await;
}

/// Hands queued input to the handler in order until the connection closes or the
/// stream rejects input.
async fn write(queue: Arc<Queue>, stream_id: u64, handler: Arc<dyn Handler>, session: Session) {
    let disconnected = session.disconnected();
    loop {
        let next = {
            let mut state = lock(&queue.state);
            let next = state.items.pop_front();
            if let Some(data) = &next {
                state.bytes -= data.len();
            } else {
                state.warned = false;
            }
            next
        };
        let Some(data) = next else {
            tokio::select! {
                () = queue.ready.notified() => continue,
                () = disconnected.cancelled() => return,
            }
        };
        let result = tokio::select! {
            result = handler.input(stream_id, data, session.clone()) => result,
            () = disconnected.cancelled() => return,
        };
        if let Err(error) = result {
            let (frames, bytes) = {
                let mut state = lock(&queue.state);
                state.closed = true;
                let pending = (state.items.len(), state.bytes);
                state.items.clear();
                state.bytes = 0;
                pending
            };
            dropped(&session, stream_id, &error.code, frames, bytes).await;
            return;
        }
    }
}

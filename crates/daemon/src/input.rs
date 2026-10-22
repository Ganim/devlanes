//! Terminal input from one connection. Each attach stream gets a bounded queue
//! and its own writer task, so a unit that stops reading stdin stalls only its
//! own stream, never the connection's requests or other streams. Bytes keep their
//! order per stream; a full queue drops its oldest input and tells the client.

use std::{
    collections::{HashMap, HashSet, VecDeque},
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
    /// The attach stream ended: the writer leaves once the queue is empty.
    ended: bool,
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
    /// Streams that ended; their late input is dropped without a new writer.
    ended: HashSet<u64>,
}

impl Inputs {
    /// Queues for the connection behind `session`; writers end when it disconnects.
    pub(crate) fn new(handler: Arc<dyn Handler>, session: Session) -> Self {
        Self {
            handler,
            session,
            streams: HashMap::new(),
            ended: HashSet::new(),
        }
    }

    /// The attach stream `stream_id` ended (its unit exited): drops its queue,
    /// and its writer once that delivered what was already queued.
    pub(crate) fn end(&mut self, stream_id: u64) {
        self.ended.insert(stream_id);
        if let Some(queue) = self.streams.remove(&stream_id) {
            lock(&queue.state).ended = true;
            queue.ready.notify_one();
        }
    }

    /// How many streams still have a queue (tests).
    #[cfg(test)]
    fn queues(&self) -> usize {
        self.streams.len()
    }

    /// Queues `data` for `stream_id` without waiting on the unit.
    pub(crate) fn push(&mut self, stream_id: u64, data: Bytes) {
        if self.ended.contains(&stream_id) {
            return;
        }
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
        let (next, ended) = {
            let mut state = lock(&queue.state);
            let next = state.items.pop_front();
            if let Some(data) = &next {
                state.bytes -= data.len();
            } else {
                state.warned = false;
            }
            (next, state.ended)
        };
        if next.is_none() && ended {
            return;
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_proto::{Request, RpcError};
    use serde_json::Value;

    /// Records the input it gets.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<(u64, Bytes)>>);

    #[async_trait::async_trait]
    impl Handler for Recorder {
        fn managed_processes(&self) -> usize {
            0
        }
        async fn reconcile(&self) -> Result<(), RpcError> {
            Ok(())
        }
        async fn handle(&self, _: Request, _: Session) -> Result<Value, RpcError> {
            Ok(Value::Null)
        }
        async fn input(&self, stream_id: u64, data: Bytes, _: Session) -> Result<(), RpcError> {
            lock(&self.0).push((stream_id, data));
            Ok(())
        }
        async fn shutdown(&self, _: crate::Shutdown) -> Result<Value, RpcError> {
            Ok(Value::Null)
        }
    }

    #[tokio::test]
    /// An ended stream's idle queue and writer go away, and its late input
    /// starts no new writer; other streams keep theirs.
    async fn ended_streams_drop_their_queue_and_writer() {
        let recorder = Arc::new(Recorder::default());
        let (session, _frames) = Session::local(1);
        let mut inputs = Inputs::new(recorder.clone(), session);
        inputs.push(1, Bytes::from_static(b"a"));
        inputs.push(2, Bytes::from_static(b"b"));
        let queue = Arc::downgrade(&inputs.streams[&1]);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while lock(&recorder.0).len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("input delivered");

        inputs.end(1);
        assert_eq!(inputs.queues(), 1);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while queue.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the writer of the ended stream left");
        inputs.push(1, Bytes::from_static(b"late"));
        assert_eq!(inputs.queues(), 1, "no queue for an ended stream");
    }
}

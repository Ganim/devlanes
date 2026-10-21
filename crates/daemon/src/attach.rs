//! Terminal attach streams. A stream belongs to the connection that opened it;
//! input is accepted only on that connection, and only the most recent interactive
//! viewer of a unit may resize it (agents and observers never do).

use std::{collections::HashMap, sync::Arc};

use bytes::Bytes;
use devlanes_core::{TerminalSize, WorktreeId};
use devlanes_proto::{AttachParams, AttachResult, Context, ResizeParams, RpcError};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::{
    Session,
    engine::{Inner, required},
    model::lock,
    registry::internal,
    units::UnitHost,
};

/// Unit identity for viewer ownership.
type UnitKey = (WorktreeId, String);

/// One open stream.
struct Stream {
    host: UnitHost,
    key: UnitKey,
}

/// Open streams by `(session, stream)` and the active interactive viewer per unit.
#[derive(Default)]
pub(crate) struct AttachTable {
    streams: HashMap<(u64, u64), Stream>,
    active: HashMap<UnitKey, (u64, u64)>,
}

impl Inner {
    /// `attach`: answers with the stream id, then sends a screen snapshot and live
    /// output as binary frames; an empty frame marks the unit's exit.
    pub(crate) async fn attach_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: AttachParams = required(value)?;
        let ((project, lane), selectors) = self
            .lane_for_units(std::slice::from_ref(&args.unit), context)
            .await?;
        let name = selectors.into_iter().next().unwrap_or_default();
        let (host, reference) = {
            let st = lane.st();
            let host = st
                .units
                .get(&name)
                .map(|rec| rec.host.clone())
                .ok_or_else(|| {
                    RpcError::new(
                        "not_running",
                        json!({"unit": name, "lane": st.reference(&project.name())}),
                        1,
                    )
                })?;
            (host, st.reference(&project.name()))
        };
        let stream_id = self
            .next_stream
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let key = (lane.worktree, name.clone());
        {
            let mut table = lock(&self.attach);
            table.streams.insert(
                (session.id(), stream_id),
                Stream {
                    host: host.clone(),
                    key: key.clone(),
                },
            );
            if args.interactive {
                table.active.insert(key.clone(), (session.id(), stream_id));
            }
        }
        if args.interactive
            && let (Some(columns), Some(rows)) = (args.columns, args.rows)
            && columns > 0
            && rows > 0
        {
            let _ = host.resize(TerminalSize { columns, rows }).await;
        }
        let (snapshot, live) = host.attach();
        let engine = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            pump(&session, stream_id, &host, snapshot, live).await;
            engine.close_stream(session.id(), stream_id).await;
        });
        serde_json::to_value(AttachResult {
            stream_id,
            unit: name,
            lane: reference,
        })
        .map_err(|e| internal("encode", e))
    }

    /// Removes a stream; when it was the active viewer, the unit returns to 120x40.
    async fn close_stream(&self, session: u64, stream: u64) {
        let restore = {
            let mut table = lock(&self.attach);
            let Some(entry) = table.streams.remove(&(session, stream)) else {
                return;
            };
            if table.active.get(&entry.key) == Some(&(session, stream)) {
                table.active.remove(&entry.key);
                Some(entry.host)
            } else {
                None
            }
        };
        if let Some(host) = restore
            && host.is_alive()
        {
            let _ = host.resize(TerminalSize::default()).await;
        }
    }

    /// Terminal input on a stream this session opened.
    pub(crate) async fn input(
        &self,
        stream_id: u64,
        data: Bytes,
        session: &Session,
    ) -> Result<(), RpcError> {
        let host = lock(&self.attach)
            .streams
            .get(&(session.id(), stream_id))
            .map(|stream| stream.host.clone())
            .ok_or_else(|| RpcError::new("unknown_stream", json!({"stream_id": stream_id}), 1))?;
        // An exited unit drops input; the stream's empty frame already told the client.
        let _ = host.input(data).await;
        Ok(())
    }

    /// `resize`: applied only for the unit's active interactive viewer.
    pub(crate) async fn resize_method(
        self: &Arc<Self>,
        value: Value,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: ResizeParams = required(value)?;
        let host = {
            let table = lock(&self.attach);
            let stream = table
                .streams
                .get(&(session.id(), args.stream_id))
                .ok_or_else(|| {
                    RpcError::new("unknown_stream", json!({"stream_id": args.stream_id}), 1)
                })?;
            (table.active.get(&stream.key) == Some(&(session.id(), args.stream_id)))
                .then(|| stream.host.clone())
        };
        let Some(host) = host else {
            return Ok(json!({"applied": false}));
        };
        if args.columns == 0 || args.rows == 0 {
            return Err(RpcError::new(
                "bad_request",
                json!({"message": "size must be nonzero"}),
                2,
            ));
        }
        host.resize(TerminalSize {
            columns: args.columns,
            rows: args.rows,
        })
        .await?;
        Ok(json!({"applied": true}))
    }
}

/// Snapshot, then live bytes until exit or disconnect; redraws after lag.
async fn pump(
    session: &Session,
    stream_id: u64,
    host: &UnitHost,
    snapshot: Bytes,
    mut live: broadcast::Receiver<Bytes>,
) {
    if session.bytes(stream_id, snapshot).await.is_err() {
        return;
    }
    let disconnected = session.disconnected();
    loop {
        let next = tokio::select! {
            () = disconnected.cancelled() => return,
            _ = host.wait_exit() => None,
            received = live.recv() => Some(received),
        };
        match next {
            Some(Ok(bytes)) => {
                if session.bytes(stream_id, bytes).await.is_err() {
                    return;
                }
            }
            Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                if session.bytes(stream_id, host.redraw()).await.is_err() {
                    return;
                }
            }
            Some(Err(broadcast::error::RecvError::Closed)) | None => {
                while let Ok(bytes) = live.try_recv() {
                    if session.bytes(stream_id, bytes).await.is_err() {
                        return;
                    }
                }
                let _ = session.bytes(stream_id, Bytes::new()).await;
                return;
            }
        }
    }
}

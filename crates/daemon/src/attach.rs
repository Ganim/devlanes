//! Terminal attach streams. A stream belongs to the connection that opened it;
//! input is accepted only on that connection, and only the most recent interactive
//! viewer of a unit may resize it (agents and observers never do). When that
//! viewer leaves, the newest remaining interactive viewer takes over with its own
//! size; the unit returns to 120x40 only when none remains, or to
//! [`TerminalSize::RENDERED_HEADLESS`] when its output renders its terminal.

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

/// Smallest PTY a viewer may request; smaller sizes are raised to it.
pub(crate) const MIN_SIZE: TerminalSize = TerminalSize {
    columns: 20,
    rows: 5,
};

/// `size` raised to [`MIN_SIZE`] in each dimension.
fn at_least_min(size: TerminalSize) -> TerminalSize {
    TerminalSize {
        columns: size.columns.max(MIN_SIZE.columns),
        rows: size.rows.max(MIN_SIZE.rows),
    }
}

/// One open stream.
struct Stream {
    host: UnitHost,
    key: UnitKey,
    interactive: bool,
    /// The viewer's last requested size, applied when it owns the unit.
    size: Option<TerminalSize>,
}

impl AttachTable {
    /// After `(session, stream)` left `key`: the newest remaining interactive
    /// viewer becomes active. True when the leaver was the active viewer, so
    /// the unit's size changes.
    fn hand_over(&mut self, key: &UnitKey, session: u64, stream: u64) -> bool {
        if self.active.get(key) != Some(&(session, stream)) {
            return false;
        }
        // Stream ids grow with every attach, so the largest is the most recent.
        let next = self
            .streams
            .iter()
            .filter(|(_, s)| s.interactive && &s.key == key)
            .max_by_key(|((_, id), _)| *id)
            .map(|(owner, _)| *owner);
        match next {
            Some(owner) => self.active.insert(key.clone(), owner),
            None => self.active.remove(key),
        };
        true
    }

    /// The size `key` should have now: its active viewer's last request, or
    /// `headless` when nobody owns it.
    fn owner_size(&self, key: &UnitKey, headless: TerminalSize) -> TerminalSize {
        self.active
            .get(key)
            .and_then(|owner| self.streams.get(owner))
            .and_then(|stream| stream.size)
            .unwrap_or(headless)
    }
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
            // Its terminal belongs to a daemon that is gone; there is nothing to show.
            if let Some(error) = st.orphan_error(&project.name(), &name) {
                return Err(error);
            }
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
        let size = match (args.columns, args.rows) {
            (Some(columns), Some(rows)) if columns > 0 && rows > 0 => {
                Some(at_least_min(TerminalSize { columns, rows }))
            }
            _ => None,
        };
        {
            let mut table = lock(&self.attach);
            table.streams.insert(
                (session.id(), stream_id),
                Stream {
                    host: host.clone(),
                    key: key.clone(),
                    interactive: args.interactive,
                    size,
                },
            );
            if args.interactive {
                table.active.insert(key.clone(), (session.id(), stream_id));
            }
        }
        if args.interactive && size.is_some() {
            let _ = self.apply_owner_size(&key, &host).await;
        }
        // Drawn at this viewer's size: an observer's pane rarely matches the PTY.
        let (snapshot, live) = host.attach(size).await;
        let engine = self.clone();
        let session = session.clone();
        tokio::spawn(async move {
            pump(&engine, &session, stream_id, &host, snapshot, live).await;
            engine.close_stream(session.id(), stream_id).await;
        });
        serde_json::to_value(AttachResult {
            stream_id,
            unit: name,
            lane: reference,
        })
        .map_err(|e| internal("encode", e))
    }

    /// Removes a stream; when it was the active viewer, ownership and its size
    /// pass to the newest remaining interactive viewer, else the unit returns to 120x40.
    async fn close_stream(&self, session: u64, stream: u64) {
        let restore = {
            let mut table = lock(&self.attach);
            let Some(entry) = table.streams.remove(&(session, stream)) else {
                return;
            };
            table
                .hand_over(&entry.key, session, stream)
                .then_some((entry.key, entry.host))
        };
        if let Some((key, host)) = restore
            && host.is_alive()
        {
            let _ = self.apply_owner_size(&key, &host).await;
        }
    }

    /// Resizes `host` to whatever its owner wants at the moment of applying.
    /// A client that reopens a stream closes the old one while attaching the
    /// new one; computing the size inside the turn means the last resize is
    /// always the current owner's, never the stale 120x40 of the closed one.
    async fn apply_owner_size(
        &self,
        key: &UnitKey,
        host: &UnitHost,
    ) -> Result<TerminalSize, RpcError> {
        let _turn = self.attach_resize.lock().await;
        let size = lock(&self.attach).owner_size(key, host.headless_size());
        host.resize(size).await?;
        Ok(size)
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
        if args.columns == 0 || args.rows == 0 {
            return Err(RpcError::new(
                "bad_request",
                json!({"message": "size must be nonzero"}),
                2,
            ));
        }
        let size = at_least_min(TerminalSize {
            columns: args.columns,
            rows: args.rows,
        });
        let owned = {
            let mut table = lock(&self.attach);
            let owner = (session.id(), args.stream_id);
            let stream = table.streams.get_mut(&owner).ok_or_else(|| {
                RpcError::new("unknown_stream", json!({"stream_id": args.stream_id}), 1)
            })?;
            // Remembered even while another viewer owns the unit, for a later handover.
            stream.size = Some(size);
            let (key, host) = (stream.key.clone(), stream.host.clone());
            (table.active.get(&key) == Some(&owner)).then_some((key, host))
        };
        let Some((key, host)) = owned else {
            return Ok(json!({"applied": false}));
        };
        let size = self.apply_owner_size(&key, &host).await?;
        Ok(json!({"applied": true, "columns": size.columns, "rows": size.rows}))
    }
}

/// Snapshot, then live bytes until exit or disconnect; redraws after lag.
async fn pump(
    engine: &Inner,
    session: &Session,
    stream_id: u64,
    host: &UnitHost,
    snapshot: Bytes,
    mut live: broadcast::Receiver<Bytes>,
) {
    if session.bytes(stream_id, snapshot).await.is_err() {
        return;
    }
    if host.renders_screen() {
        return pump_rendered(engine, session, stream_id, host, live).await;
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
                // The viewer's latest size, which a resize may have changed since the attach.
                let size = lock(&engine.attach)
                    .streams
                    .get(&(session.id(), stream_id))
                    .and_then(|stream| stream.size);
                if session
                    .bytes(stream_id, host.redraw(size).await)
                    .await
                    .is_err()
                {
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

/// Shortest time between two redraws for one viewer of a rendered unit.
const REDRAW_GAP: std::time::Duration = std::time::Duration::from_millis(30);

/// Redraws until exit or disconnect, for a unit whose output renders its
/// terminal. That output is never forwarded: its cursor moves only fit a grid
/// of the unit's size, while the viewer shows the screen at its own. Output
/// instead brings a redraw, at most one per [`REDRAW_GAP`]; output arriving in
/// between stays queued and brings the next one, so the last state always
/// reaches the viewer. Each redraw changes only the rows that differ from the
/// one before.
async fn pump_rendered(
    engine: &Inner,
    session: &Session,
    stream_id: u64,
    host: &UnitHost,
    mut live: broadcast::Receiver<Bytes>,
) {
    use broadcast::error::{RecvError, TryRecvError};
    // Skips what is already queued; one redraw covers it. True when anything was.
    let skip_queued = |live: &mut broadcast::Receiver<Bytes>| {
        let mut skipped = false;
        while let Ok(_) | Err(TryRecvError::Lagged(_)) = live.try_recv() {
            skipped = true;
        }
        skipped
    };
    // What the viewer shows since the last redraw; the attach snapshot is not
    // kept, so the first redraw is a whole screen.
    let mut shown = None;
    let redraw = async |shown: &mut Option<vt100::Screen>| {
        // The viewer's latest size, which a resize may have changed since the attach.
        let size = lock(&engine.attach)
            .streams
            .get(&(session.id(), stream_id))
            .and_then(|stream| stream.size);
        let (bytes, screen) = host.render(size, shown.take()).await;
        *shown = screen;
        // An unreadable screen gives nothing to send; an empty frame would tell
        // the viewer that the unit exited.
        if bytes.is_empty() {
            return Ok(());
        }
        session.bytes(stream_id, bytes).await
    };
    let disconnected = session.disconnected();
    let mut next_redraw = tokio::time::Instant::now();
    loop {
        let open = tokio::select! {
            () = disconnected.cancelled() => return,
            _ = host.wait_exit() => false,
            received = live.recv() => !matches!(received, Err(RecvError::Closed)),
        };
        if !open {
            if skip_queued(&mut live) && redraw(&mut shown).await.is_err() {
                return;
            }
            let _ = session.bytes(stream_id, Bytes::new()).await;
            return;
        }
        tokio::select! {
            () = disconnected.cancelled() => return,
            () = tokio::time::sleep_until(next_redraw) => {}
        }
        skip_queued(&mut live);
        if redraw(&mut shown).await.is_err() {
            return;
        }
        next_redraw = tokio::time::Instant::now() + REDRAW_GAP;
    }
}

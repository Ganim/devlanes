//! Reader threads that drain a unit's output pipes into a broadcast channel.

use std::{
    io::{self, PipeReader, Read},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicUsize, Ordering},
    },
};

use bytes::Bytes;
use devlanes_core::{OutputChunk, OutputStream};
use tokio::sync::{broadcast, watch};

use crate::terminal::{Sizing, Toggles};

/// Read size per call; large enough for bursty build output.
const CHUNK: usize = 16 * 1024;

/// The broadcast sender while the unit's output is open. The supervisor
/// empties it once the report is out, so receivers see `Closed` even when a
/// reader is still blocked on a pipe whose end of file never came.
#[derive(Debug)]
pub(crate) struct Slot(Mutex<Option<broadcast::Sender<OutputChunk>>>);

impl Slot {
    /// A slot holding `sender`.
    pub(crate) fn new(sender: broadcast::Sender<OutputChunk>) -> Self {
        Self(Mutex::new(Some(sender)))
    }

    /// A new receiver, or one that is already closed once output ended.
    pub(crate) fn subscribe(&self) -> broadcast::Receiver<OutputChunk> {
        match &*self.0.lock().unwrap_or_else(PoisonError::into_inner) {
            Some(sender) => sender.subscribe(),
            None => broadcast::channel(1).1,
        }
    }

    /// Drops the sender; later chunks are discarded.
    pub(crate) fn close(&self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take();
    }

    /// Broadcasts one chunk; having no subscribers is not an error.
    fn publish(&self, stream: OutputStream, bytes: Bytes) {
        if let Some(sender) = &*self.0.lock().unwrap_or_else(PoisonError::into_inner) {
            let _ = sender.send(OutputChunk { stream, bytes });
        }
    }
}

/// Starts one thread per pipe and flips `done` once every pipe reached end of
/// file, which happens when the last process holding a write end exits.
///
/// Readers never wait for subscribers: a lagging receiver loses chunks rather
/// than stalling the unit on a full pipe. `Terminal` output (a pseudo
/// console's) loses the console host's own mode switches, so logs, the screen
/// model and every client get clean bytes, and gets the repaints that
/// `sizing` waits for marked (see [`Toggles`]).
pub(crate) fn start(
    pipes: Vec<(OutputStream, PipeReader)>,
    slot: Arc<Slot>,
    done: watch::Sender<bool>,
    sizing: Option<Arc<Sizing>>,
) -> io::Result<()> {
    let done = Arc::new(done);
    let remaining = Arc::new(AtomicUsize::new(pipes.len()));
    if pipes.is_empty() {
        done.send_replace(true);
    }
    for (stream, pipe) in pipes {
        let (slot, done, remaining) = (slot.clone(), done.clone(), remaining.clone());
        let toggles = sizing
            .clone()
            .map_or_else(Toggles::default, Toggles::with_sizing);
        std::thread::Builder::new()
            .name("unit-output".into())
            .spawn(move || {
                read(stream, pipe, &slot, toggles);
                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    done.send_replace(true);
                }
            })?;
    }
    Ok(())
}

/// Reads `pipe` until end of file or an error, through `toggles` for
/// `Terminal` output.
fn read(stream: OutputStream, mut pipe: PipeReader, slot: &Slot, mut toggles: Toggles) {
    let mut buffer = vec![0_u8; CHUNK];
    let publish = |bytes: Vec<u8>| {
        // A read that held only toggles leaves nothing; an empty chunk would
        // tell attached viewers that the unit exited.
        if !bytes.is_empty() {
            slot.publish(stream, Bytes::from(bytes));
        }
    };
    loop {
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) if stream == OutputStream::Terminal => publish(toggles.filter(&buffer[..n])),
            Ok(n) => slot.publish(stream, Bytes::copy_from_slice(&buffer[..n])),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            // A broken pipe is how Windows reports end of file on a pipe.
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => {
                tracing::debug!(%error, "pipe read failed");
                break;
            }
        }
    }
    publish(toggles.finish());
}

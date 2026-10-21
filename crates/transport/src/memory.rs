//! In-process transport for tests, with fault injection.
//!
//! Lives here rather than in `testing` so the transport's own tests and
//! downstream mirroring tests can use it without a dependency cycle.

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, DuplexStream, ReadBuf};
use tokio::sync::mpsc;

use crate::transport::{BoxDuplex, Transport, TransportError};

/// Buffer per direction; large enough that yamux frames rarely block.
const BUFFER: usize = 256 * 1024;

/// Creates a connected transport/listener pair.
pub fn memory_transport() -> (MemoryTransport, MemoryListener) {
    let (accept_tx, accept_rx) = mpsc::unbounded_channel();
    let transport = MemoryTransport {
        accept_tx,
        state: Arc::new(Mutex::new(TransportState::default())),
    };
    (transport, MemoryListener { accept_rx })
}

#[derive(Default)]
struct TransportState {
    refuse: Option<bool>,
    links: Vec<LinkControl>,
}

/// Client end: each `connect` creates a fresh in-memory link.
#[derive(Clone)]
pub struct MemoryTransport {
    accept_tx: mpsc::UnboundedSender<MemoryStream>,
    state: Arc<Mutex<TransportState>>,
}

impl MemoryTransport {
    /// Makes later connects fail; `permanent` selects the error class.
    pub fn refuse(&self, permanent: bool) {
        self.lock().refuse = Some(permanent);
    }

    /// Lets connects succeed again.
    pub fn accept(&self) {
        self.lock().refuse = None;
    }

    /// Number of successful connects so far.
    pub fn connections(&self) -> usize {
        self.lock().links.len()
    }

    /// Fault controls for the most recent link.
    pub fn last_link(&self) -> Option<LinkControl> {
        self.lock().links.last().cloned()
    }

    /// Locks state; poisoning only happens if a test panicked mid-update.
    fn lock(&self) -> std::sync::MutexGuard<'_, TransportState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[async_trait]
impl Transport for MemoryTransport {
    async fn connect(&self) -> Result<BoxDuplex, TransportError> {
        match self.lock().refuse {
            Some(true) => return Err(TransportError::permanent("memory transport refused")),
            Some(false) => return Err(TransportError::transient("memory transport refused")),
            None => {}
        }
        let (client, server) = tokio::io::duplex(BUFFER);
        let gate = Arc::new(Mutex::new(Gate::default()));
        let server = MemoryStream {
            inner: server,
            gate: gate.clone(),
            end: 1,
        };
        self.accept_tx
            .send(server)
            .map_err(|_| TransportError::transient("memory listener closed"))?;
        let control = LinkControl { gate: gate.clone() };
        self.lock().links.push(control);
        Ok(Box::new(MemoryStream {
            inner: client,
            gate,
            end: 0,
        }))
    }
}

/// Box end: yields the server side of each link.
pub struct MemoryListener {
    accept_rx: mpsc::UnboundedReceiver<MemoryStream>,
}

impl MemoryListener {
    /// Waits for the next link; `None` once every transport clone is dropped.
    pub async fn accept(&mut self) -> Option<MemoryStream> {
        self.accept_rx.recv().await
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Mode {
    #[default]
    Open,
    Frozen,
    Severed,
}

/// Link state plus parked tasks, indexed by `[end][read=0|write=1]`.
#[derive(Default)]
struct Gate {
    mode: Mode,
    wakers: [[Option<Waker>; 2]; 2],
}

impl Gate {
    /// Records the task so a mode change can wake it, whatever it is waiting on.
    fn park(&mut self, end: usize, direction: usize, waker: &Waker) {
        let slot = &mut self.wakers[end][direction];
        if !slot.as_ref().is_some_and(|w| w.will_wake(waker)) {
            *slot = Some(waker.clone());
        }
    }

    fn set(&mut self, mode: Mode) {
        self.mode = mode;
        for waker in self.wakers.iter_mut().flatten().filter_map(Option::take) {
            waker.wake();
        }
    }
}

/// Fault injection for one link.
#[derive(Clone)]
pub struct LinkControl {
    gate: Arc<Mutex<Gate>>,
}

impl LinkControl {
    /// Stops all traffic without closing, like a dead network path.
    pub fn freeze(&self) {
        self.lock().set(Mode::Frozen);
    }

    /// Resumes traffic after [`LinkControl::freeze`].
    pub fn thaw(&self) {
        self.lock().set(Mode::Open);
    }

    /// Breaks the link; both ends see connection resets.
    pub fn sever(&self) {
        self.lock().set(Mode::Severed);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Gate> {
        self.gate.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One end of an in-memory link.
pub struct MemoryStream {
    inner: DuplexStream,
    gate: Arc<Mutex<Gate>>,
    end: usize,
}

impl MemoryStream {
    /// Checks the gate before an operation; `Some` short-circuits it.
    fn check<T>(&self, direction: usize, cx: &Context<'_>) -> Option<Poll<io::Result<T>>> {
        let mut gate = self.gate.lock().unwrap_or_else(PoisonError::into_inner);
        gate.park(self.end, direction, cx.waker());
        match gate.mode {
            Mode::Open => None,
            Mode::Frozen => Some(Poll::Pending),
            Mode::Severed => Some(Poll::Ready(Err(io::Error::from(
                io::ErrorKind::ConnectionReset,
            )))),
        }
    }
}

impl AsyncRead for MemoryStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if let Some(poll) = self.check(0, cx) {
            return poll;
        }
        Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for MemoryStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if let Some(poll) = self.check(1, cx) {
            return poll;
        }
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(poll) = self.check(1, cx) {
            return poll;
        }
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(poll) = self.check(1, cx) {
            return poll;
        }
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

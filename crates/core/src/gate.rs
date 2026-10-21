//! Startup pacing contracts. One machine-wide [`StartGate`] decides when each
//! service may spawn; a [`PressureSource`] reports system CPU and memory pressure
//! to gates that adapt. Gates only schedule starts: nothing here limits or
//! signals a running process.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, PoisonError},
    task::{Context, Poll},
};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::Error;

/// Memory assumed for a start without history: [`StartRequest::memory_need`]
/// when the unit has no baseline yet.
pub const DEFAULT_MEMORY_NEED: u64 = 512 << 20;

/// One service start waiting for a slot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartRequest {
    /// Canonical lane reference (`project@index`).
    pub lane: String,
    /// Unit name within the lane (`web`, `worker/2`).
    pub unit: String,
    /// Position of the unit in its config file. Starts are granted first come,
    /// first served by batch ([`StartGate::enqueue_batch`]: one start request,
    /// any lane or project); within a batch, lower values go first.
    pub order: usize,
    /// Bytes the unit is expected to need while starting: its baseline peak,
    /// or [`DEFAULT_MEMORY_NEED`]. A gate that reads available memory holds the
    /// head of the queue while this does not fit.
    pub memory_need: u64,
}

/// Why a slot was given back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Release {
    /// The unit's ready check passed.
    Ready,
    /// The unit failed or exited before its startup finished.
    Failed,
    /// `slot_timeout` passed; the gate released the slot on its own.
    Timeout,
    /// The unit's tree CPU stayed below `settle_cpu` long enough (`ready: started`).
    Settled,
    /// The slot holder went away without saying why.
    Dropped,
}

/// Gate side of a granted slot. Implementations must make every call idempotent:
/// only the first release counts.
pub trait SlotHandle: Send + Sync {
    /// Gives the slot back.
    fn release(&self, reason: Release);
    /// Feeds the unit's process-tree CPU (percent of one core) for the settle rule.
    fn report_cpu(&self, cpu_percent: f64);
    /// The reason the slot was released, by the holder or by the gate.
    fn released(&self) -> Option<Release>;
}

/// Releases the slot when the last [`StartSlot`] clone goes away.
struct SlotGuard(Box<dyn SlotHandle>);

impl Drop for SlotGuard {
    /// A holder that never said why counts as [`Release::Dropped`].
    fn drop(&mut self) {
        self.0.release(Release::Dropped);
    }
}

/// A granted start slot. Clones share the slot; it is released on the first
/// [`StartSlot::release`], by the gate (timeout), or when the last clone drops.
#[derive(Clone)]
pub struct StartSlot(Arc<SlotGuard>);

impl StartSlot {
    /// Wraps a gate's handle.
    pub fn new(handle: impl SlotHandle + 'static) -> Self {
        Self(Arc::new(SlotGuard(Box::new(handle))))
    }

    /// A slot that no gate counts, for starts that bypass the gate.
    pub fn unmetered() -> Self {
        Self::new(Unmetered::default())
    }

    /// Gives the slot back; later calls are ignored.
    pub fn release(&self, reason: Release) {
        self.0.0.release(reason);
    }

    /// Reports the unit's tree CPU; the gate may release the slot as [`Release::Settled`].
    pub fn report_cpu(&self, cpu_percent: f64) {
        self.0.0.report_cpu(cpu_percent);
    }

    /// Why the slot was released, if it was.
    pub fn released(&self) -> Option<Release> {
        self.0.0.released()
    }
}

impl fmt::Debug for StartSlot {
    /// Shows the release state only.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartSlot")
            .field("released", &self.released())
            .finish()
    }
}

/// Handle of [`StartSlot::unmetered`]: remembers its release and nothing else.
#[derive(Default)]
struct Unmetered(Mutex<Option<Release>>);

impl SlotHandle for Unmetered {
    /// Records the first reason.
    fn release(&self, reason: Release) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert(reason);
    }

    /// Nothing listens.
    fn report_cpu(&self, _: f64) {}

    /// The recorded reason.
    fn released(&self) -> Option<Release> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A queued start. Await it for the slot; dropping it leaves the queue.
pub struct StartTicket {
    receiver: Option<oneshot::Receiver<StartSlot>>,
    ready: Option<StartSlot>,
    /// Gate-provided value whose `Drop` removes the queue entry. Kept until the
    /// ticket is dropped; after a grant the removal finds nothing.
    _leave: Option<Box<dyn Send + Sync>>,
}

impl StartTicket {
    /// A ticket the gate will fill through `receiver`. `leave` is dropped with the
    /// ticket, so its `Drop` can take an abandoned request out of the queue.
    pub fn queued(
        receiver: oneshot::Receiver<StartSlot>,
        leave: impl Send + Sync + 'static,
    ) -> Self {
        Self {
            receiver: Some(receiver),
            ready: None,
            _leave: Some(Box::new(leave)),
        }
    }

    /// A ticket granted at once.
    pub fn granted(slot: StartSlot) -> Self {
        Self {
            receiver: None,
            ready: Some(slot),
            _leave: None,
        }
    }
}

impl Future for StartTicket {
    type Output = StartSlot;

    /// Resolves with the granted slot. A gate that vanished without granting
    /// yields an unmetered slot, so a start is never blocked by a dead gate.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<StartSlot> {
        if let Some(slot) = self.ready.take() {
            return Poll::Ready(slot);
        }
        let Some(receiver) = self.receiver.as_mut() else {
            return Poll::Ready(StartSlot::unmetered());
        };
        match Pin::new(receiver).poll(cx) {
            Poll::Ready(result) => {
                self.receiver = None;
                Poll::Ready(result.unwrap_or_else(|_| StartSlot::unmetered()))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Why the head of the queue is not starting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldReason {
    /// Every slot of the window is taken.
    WindowFull,
    /// Memory pressure is critical; no new starts until it clears.
    MemoryCritical,
    /// The next start's [`StartRequest::memory_need`] exceeds the available
    /// memory minus what starting units may still claim. It goes ahead once
    /// memory is free or the starts in flight finished or timed out.
    Memory,
}

/// A queued start and its 1-based place in line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedStart {
    pub request: StartRequest,
    pub position: usize,
}

/// Point-in-time view of a gate, for `status` and `why`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GateSnapshot {
    /// `adaptive`, `fixed` or `all`.
    pub mode: String,
    /// Starts allowed at once; `None` means unlimited.
    pub window: Option<usize>,
    /// Slots currently held.
    pub in_flight: usize,
    /// Waiting starts in the order they will be granted.
    pub queued: Vec<QueuedStart>,
    /// Set while something is queued.
    pub hold: Option<HoldReason>,
    /// The gate's latest pressure reading, if it reads one.
    pub pressure: Option<PressureSample>,
    /// The gate's latest available-memory reading in bytes, if it reads one.
    pub available_memory: Option<u64>,
}

impl GateSnapshot {
    /// The queue entry of `unit` in `lane`, if it is waiting.
    pub fn find(&self, lane: &str, unit: &str) -> Option<&QueuedStart> {
        self.queued
            .iter()
            .find(|entry| entry.request.lane == lane && entry.request.unit == unit)
    }
}

/// Decides when service starts may spawn. One instance is shared by every lane
/// and project of the machine.
#[async_trait]
pub trait StartGate: Send + Sync {
    /// Queues the units of one start request and returns at once, one ticket
    /// per request in the same order. Batches are served in arrival order, so
    /// a later lane never jumps ahead of an earlier one; within a batch, by
    /// [`StartRequest::order`]. Dropping a ticket before its grant leaves the queue.
    fn enqueue_batch(&self, requests: Vec<StartRequest>) -> Vec<StartTicket>;

    /// Queues a single start as its own batch.
    fn enqueue(&self, request: StartRequest) -> StartTicket {
        self.enqueue_batch(vec![request])
            .pop()
            .unwrap_or_else(|| StartTicket::granted(StartSlot::unmetered()))
    }

    /// Current window, holders and queue.
    fn snapshot(&self) -> GateSnapshot;

    /// Waits for a slot: [`StartGate::enqueue`] and await the ticket.
    async fn acquire(&self, request: StartRequest) -> StartSlot {
        self.enqueue(request).await
    }
}

/// System memory pressure, as the kernel grades it.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPressure {
    #[default]
    Normal,
    Warn,
    Critical,
}

impl MemoryPressure {
    /// Stable lowercase word for clients.
    pub fn word(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Warn => "warn",
            Self::Critical => "critical",
        }
    }
}

/// One pressure reading. Fields a platform cannot provide are `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct PressureSample {
    /// Busy share of all CPUs since the previous reading, 0-100 (not per core).
    /// `None` on the first reading, which has nothing to compare against.
    pub cpu_percent: Option<f64>,
    /// Kernel memory pressure level.
    pub memory: Option<MemoryPressure>,
    /// Share of time runnable tasks waited for a CPU, 0-100 (Linux PSI
    /// `cpu some avg10`); informational.
    pub cpu_stall_percent: Option<f64>,
}

/// Reads machine-wide CPU and memory pressure. Implementations keep the
/// previous CPU counters so each reading covers the time since the last one,
/// and must not block the async executor.
#[async_trait]
pub trait PressureSource: Send + Sync {
    /// Short identifier for diagnostics (`macos`, `linux`).
    fn name(&self) -> &'static str;

    /// Reads CPU and memory pressure.
    async fn sample(&self) -> Result<PressureSample, Error>;

    /// Memory the system could hand to new processes without swapping, in bytes.
    async fn available_memory(&self) -> Result<Option<u64>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts releases so the guard's drop behaviour is observable.
    #[derive(Clone, Default)]
    struct Counting(Arc<AtomicUsize>, Arc<Mutex<Option<Release>>>);

    impl SlotHandle for Counting {
        fn release(&self, reason: Release) {
            let mut first = self.1.lock().unwrap_or_else(PoisonError::into_inner);
            if first.is_none() {
                *first = Some(reason);
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        fn report_cpu(&self, _: f64) {}
        fn released(&self) -> Option<Release> {
            *self.1.lock().unwrap_or_else(PoisonError::into_inner)
        }
    }

    #[test]
    fn last_clone_releases_once_as_dropped() {
        let handle = Counting::default();
        let slot = StartSlot::new(handle.clone());
        let copy = slot.clone();
        drop(slot);
        assert_eq!(handle.0.load(Ordering::SeqCst), 0);
        drop(copy);
        assert_eq!(handle.0.load(Ordering::SeqCst), 1);
        assert_eq!(handle.released(), Some(Release::Dropped));
    }

    #[test]
    fn explicit_release_wins_over_drop() {
        let handle = Counting::default();
        let slot = StartSlot::new(handle.clone());
        slot.release(Release::Ready);
        drop(slot);
        assert_eq!(handle.released(), Some(Release::Ready));
        assert_eq!(handle.0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn abandoned_gate_yields_an_unmetered_slot() {
        let (sender, receiver) = oneshot::channel();
        drop(sender);
        let slot = StartTicket::queued(receiver, ()).await;
        assert_eq!(slot.released(), None);
    }

    #[test]
    fn memory_levels_are_ordered() {
        assert!(MemoryPressure::Normal < MemoryPressure::Warn);
        assert!(MemoryPressure::Warn < MemoryPressure::Critical);
    }
}

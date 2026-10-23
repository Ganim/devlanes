//! Start gates for the daemon: [`AllAtOnce`], [`FixedWindow`] and
//! [`AdaptiveGate`], all implementing [`devlanes_core::gate::StartGate`].
//!
//! One gate is shared by every lane and project on the machine. Start
//! requests are served first come, first served; the units of one request go
//! in declaration order. A slot is held until its holder releases it (ready,
//! failed, settled) or `slot_timeout` passes, so services that wait on each
//! other cannot block the queue. Gates that read a `PressureSource` also hold
//! the head of the queue while its memory need does not fit next to the
//! memory reserve, for at most `slot_timeout`. Gates only schedule starts;
//! they never limit or signal running processes.
//!
//! ```
//! use std::{sync::Arc, time::Duration};
//! use devlanes_core::{clock::TokioClock, gate::{DEFAULT_MEMORY_NEED, Release, StartGate, StartRequest}};
//! use devlanes_start_gate::{FixedWindow, GateOptions};
//!
//! # tokio::runtime::Runtime::new()?.block_on(async {
//! let gate = FixedWindow::new(2, GateOptions::default(), Arc::new(TokioClock::new()));
//! let request = |unit: &str, order| StartRequest {
//!     lane: "acme@1".into(), unit: unit.into(), order, memory_need: DEFAULT_MEMORY_NEED,
//! };
//! let web = gate.acquire(request("web", 0)).await;
//! let _api = gate.acquire(request("api", 1)).await;
//! let worker = gate.enqueue(request("worker", 2));
//! assert_eq!(gate.snapshot().queued.len(), 1);
//! web.release(Release::Ready);
//! let worker = tokio::time::timeout(Duration::from_secs(1), worker).await?;
//! assert_eq!(gate.snapshot().in_flight, 2);
//! # drop(worker);
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! # })?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod controller;
mod engine;

use std::sync::Arc;

use devlanes_core::{
    clock::Clock,
    gate::{Arrival, GateSnapshot, PressureSource, StartGate, StartRequest, StartTicket},
};

pub use controller::{AdaptiveConfig, Controller};
pub use engine::{GateOptions, default_reserve};

use engine::{Engine, Policy};

/// Every start proceeds at once. Slots are still tracked, so `status` can show
/// how many units are starting.
pub struct AllAtOnce(Arc<Engine>);

impl AllAtOnce {
    /// An unlimited gate.
    pub fn new(options: GateOptions, clock: Arc<dyn Clock>) -> Self {
        Self(Engine::new("all", Policy::Unlimited, options, clock, None))
    }
}

/// At most `max` starts at a time.
pub struct FixedWindow(Arc<Engine>);

impl FixedWindow {
    /// A gate with a constant window of `max` (at least 1).
    pub fn new(max: usize, options: GateOptions, clock: Arc<dyn Clock>) -> Self {
        Self(Engine::new(
            "fixed",
            Policy::Fixed(max.max(1)),
            options,
            clock,
            None,
        ))
    }

    /// Same window, but reads `pressure` while busy: snapshots carry CPU and
    /// memory pressure, and the head of the queue waits while its memory need
    /// does not fit. The readings never change the window.
    pub fn with_pressure(
        max: usize,
        options: GateOptions,
        clock: Arc<dyn Clock>,
        pressure: Arc<dyn PressureSource>,
    ) -> Self {
        Self(Engine::new(
            "fixed",
            Policy::Fixed(max.max(1)),
            options,
            clock,
            Some(pressure),
        ))
    }
}

/// A window that grows by one every `grow_every` while CPU is below target
/// and memory pressure is normal, halves after sustained high CPU or at memory
/// warn, and admits nothing at critical memory pressure. See [`Controller`].
pub struct AdaptiveGate(Arc<Engine>);

impl AdaptiveGate {
    /// A gate that reads `pressure` every `config.sample_every` while busy.
    pub fn new(
        config: AdaptiveConfig,
        options: GateOptions,
        clock: Arc<dyn Clock>,
        pressure: Arc<dyn PressureSource>,
    ) -> Self {
        Self(Engine::new(
            "adaptive",
            Policy::Adaptive(Controller::new(config)),
            options,
            clock,
            Some(pressure),
        ))
    }
}

macro_rules! start_gate {
    ($gate:ty) => {
        impl StartGate for $gate {
            /// Takes a place in line; see [`StartGate::arrive`].
            fn arrive(&self) -> Arrival {
                self.0.arrive()
            }

            /// Queues one start request's units; see [`StartGate::enqueue_batch_at`].
            fn enqueue_batch_at(
                &self,
                arrival: Arrival,
                requests: Vec<StartRequest>,
            ) -> Vec<StartTicket> {
                self.0.enqueue_batch_at(arrival, requests)
            }

            /// Window, holders, queue and the latest pressure reading.
            fn snapshot(&self) -> GateSnapshot {
                self.0.snapshot()
            }
        }
    };
}

start_gate!(AllAtOnce);
start_gate!(FixedWindow);
start_gate!(AdaptiveGate);

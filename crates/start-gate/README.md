# devlanes-start-gate

The start gates that pace service starts: `AllAtOnce`, `FixedWindow` and `AdaptiveGate`, all implementing `devlanes_core::gate::StartGate`. The binary builds one gate from the user config's `startup` block and the daemon asks it for a slot before every service spawn. `AdaptiveGate` and the binary's `FixedWindow` read a `PressureSource` from `devlanes-metrics-macos`, `devlanes-metrics-linux` or `devlanes-metrics-windows`.

## Usage

```rust
use std::sync::Arc;
use devlanes_core::{clock::TokioClock, gate::{DEFAULT_MEMORY_NEED, Release, StartGate, StartRequest}};
use devlanes_start_gate::{FixedWindow, GateOptions};

async fn pace() {
    let gate = FixedWindow::new(2, GateOptions::default(), Arc::new(TokioClock::new()));
    let request = |unit: &str, order| StartRequest {
        lane: "acme@1".into(), unit: unit.into(), order, memory_need: DEFAULT_MEMORY_NEED,
    };

    let web = gate.acquire(request("web", 0)).await;
    let _api = gate.acquire(request("api", 1)).await;
    let worker = gate.enqueue(request("worker", 2)); // queued: the window is full
    assert_eq!(gate.snapshot().queued[0].position, 1);

    web.release(Release::Ready); // worker gets web's slot
    let _worker = worker.await;
}
```

An adaptive gate takes the window bounds, the CPU target and a pressure source:

```rust
use devlanes_start_gate::{AdaptiveConfig, AdaptiveGate};

let gate = AdaptiveGate::new(
    AdaptiveConfig::new(2, 8, 75.0),
    GateOptions::default(),
    Arc::new(TokioClock::new()),
    Arc::new(devlanes_metrics_macos::MacosPressure::new()),
);
```

## API

- `AllAtOnce`, `FixedWindow`, `AdaptiveGate`: the gates. `FixedWindow` uses `max` as its constant window; `FixedWindow::with_pressure` also reads CPU and memory, so snapshots show the readings and the memory check below applies, but the window never changes.
- `StartGate::enqueue_batch`: queue the units of one start request together. `arrive` takes the place in line first when the requests need lookups.
- `GateOptions`: `slot_timeout`, `settle_cpu` (percent of one core) and `settle_for`.
- `AdaptiveConfig` / `Controller`: the adaptive window as plain state over explicit times, for replaying pressure traces.
- `GateSnapshot::burst`: what the current or last busy period saw (granted, duration, largest window, peak CPU, worst memory level, memory holds). The same numbers are logged when a busy period ends.

## Slots

Start requests are served first come, first served across every lane and project; the units of one request (one batch) go in declaration order (`StartRequest::order`). A later request never jumps ahead, so a restart of a low-index unit cannot starve others.

Gates that read a `PressureSource` also check memory at the head of the queue. Each start in flight reserves its `memory_need` (the unit's baseline startup peak, or `DEFAULT_MEMORY_NEED`, 512 MiB, without history), capped at the memory available when it was granted and lowered by the footprint the holder reports (`StartSlot::report_memory`), since that part already shows in the available memory. When the head's need exceeds the available memory minus those reservations, it waits (`HoldReason::Memory`). A reading older than one sampling interval, or none yet in this busy period, also holds the head until the driver reads, so the first wave of a busy period is checked too. The check never refuses: with nothing in flight the head goes ahead, so the wait ends when memory frees up or the starts in flight finish or reach `slot_timeout`. When the platform cannot tell available memory, or a read fails, there is no check.

A slot ends at the first of: the holder's `release` (ready or failed), dropping the last `StartSlot` clone, `slot_timeout`, or, for `ready: started` units, CPU reports (`StartSlot::report_cpu`) below `settle_cpu` for `settle_for`. Dropping a ticket before its grant leaves the queue.

`AdaptiveGate` starts each busy period at `initial`, adds 1 every 2 s while system CPU is below the target, memory pressure is normal and the window is what holds starts back, halves (minimum 1) after 3 s above 90% CPU or at memory warn, and grants nothing at critical memory pressure. Readings happen every second while anything is queued or held; the first reading of a busy period carries no CPU, because it would average over the idle time before it.

Gates only delay spawns. They never limit, signal or stop a running process.

## Config example

```yaml
startup:
  mode: adaptive      # adaptive | fixed | all; default fixed
  initial: 2          # default: CPU cores / 4, at least 2
  max: 8              # default: CPU cores
  target_cpu: 75%
  settle_cpu: 10%     # of one core
  slot_timeout: 20s
```

## License

Apache-2.0

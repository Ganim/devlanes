# devlanes-start-gate

The start gates that pace service starts: `AllAtOnce`, `FixedWindow` and `AdaptiveGate`, all implementing `devlanes_core::gate::StartGate`. The binary builds one gate from the user config's `startup` block and the daemon asks it for a slot before every service spawn; `AdaptiveGate` reads a `PressureSource` from `devlanes-metrics-macos` or `devlanes-metrics-linux`.

## Usage

```rust
use std::{sync::Arc, time::Duration};
use devlanes_core::{clock::TokioClock, gate::{Release, StartGate, StartRequest}};
use devlanes_start_gate::{FixedWindow, GateOptions};

let gate = FixedWindow::new(2, GateOptions::default(), Arc::new(TokioClock::new()));
let request = |unit: &str, order| StartRequest { lane: "acme@1".into(), unit: unit.into(), order };

let web = gate.acquire(request("web", 0)).await;
let api = gate.acquire(request("api", 1)).await;
let worker = gate.enqueue(request("worker", 2)); // queued: the window is full
assert_eq!(gate.snapshot().queued[0].position, 1);

web.release(Release::Ready); // worker gets web's slot
let worker = worker.await;
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

- `AllAtOnce`, `FixedWindow`, `AdaptiveGate`: the gates. `FixedWindow::with_pressure` adds CPU and memory readings to snapshots without changing the window.
- `GateOptions`: `slot_timeout`, `settle_cpu` (percent of one core) and `settle_for`.
- `AdaptiveConfig` / `Controller`: the adaptive window as plain state over explicit times, for replaying pressure traces.

## Slots

Starts are granted by declaration order (`StartRequest::order`), then arrival, across every lane and project. A slot ends at the first of: the holder's `release` (ready or failed), dropping the last `StartSlot` clone, `slot_timeout`, or, for `ready: started` units, CPU reports (`StartSlot::report_cpu`) below `settle_cpu` for `settle_for`. Dropping a ticket before its grant leaves the queue.

`AdaptiveGate` starts each busy period at `initial`, adds 1 every 2 s while system CPU is below the target, memory pressure is normal and starts are waiting, halves (minimum 1) after 3 s above 90% CPU or at memory warn, and grants nothing at critical memory pressure. Readings happen every second while anything is queued or held.

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

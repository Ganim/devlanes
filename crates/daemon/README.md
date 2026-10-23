# devlanes-daemon

The per-user daemon behind every `devlanes` command and the TUI. It owns lanes, processes, containers, the proxy routes and `state.db`, and answers `devlanes-proto` requests over a Unix socket; concrete integrations (PTY runner, Compose, Docker, proxy, metrics) are injected by `bins/devlanes`.

## Usage

```rust
use std::{path::PathBuf, sync::Arc};

use devlanes_daemon::{
    Engine, EngineOptions, Handler, Options, Server,
    integrations::{CoreRunner, Integrations},
};
use devlanes_runner_pty::PtyRunner;
use devlanes_state::Store;

async fn serve(home: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let socket = home.join("run/daemon.sock");
    let server = Server::bind(Options { home: home.clone(), socket: socket.clone() }).await?;
    let store = Store::open(home.join("state.db")).await?;
    let integrations = Integrations::with_runner(Arc::new(CoreRunner(PtyRunner::default())));
    let engine = Engine::new(EngineOptions::new(&home, &socket), store, integrations, server.shutdown_token());
    let handler: Arc<dyn Handler> = Arc::new(engine);
    server.run(handler, || Ok(())).await?;
    Ok(())
}
```

`Server::bind` takes the singleton lock first and fails with `Error::AlreadyRunning` when another daemon holds it. `run` reconciles before calling the readiness callback, then serves until the shutdown token is cancelled.

## API

- `Server` / `Options`: socket, lock, peer check, framing and per-connection sessions.
- `Handler` / `Session`: the request, input and shutdown contract a server drives; `Session::local` serves in-process clients and tests.
- `Engine` / `EngineOptions`: the `Handler` that runs projects, lanes, tasks, units, logs, attach and metrics.
- `integrations::Integrations`: everything injected. Its traits are `UnitRunner`/`ManagedUnit`, `InfraProvider`, `RouteSink`, `PortInspector`, `ProcessTable`, `LaunchCheck`, `RemoteConnector` and `RemotePrompter`; `Integrations::with_runner` fills the rest with no-op providers. `start_gate` and `pressure` take the core `StartGate` and `PressureSource`; without a gate every service spawns at once.
- `units::UnitHost`: one running unit with its output pump, screen snapshot, log writer and state watch.
- `process::install_panic_hook`: routes panics into `daemon.log`.

## Config example

```yaml
# ~/.devlanes/config.yaml
ports:
  # Machine-wide pool that every lane's port block is allocated from; the default
  # stays below Linux's usual ephemeral range.
  range: 20000-32767
```

## Behaviour

- The daemon is a headless API: results are structured data with machine codes and data (`not_found`, `needs_env`, `port_in_use`); wording, hints and output modes belong to the clients.
- Processes stop only on an explicit request: `stop`, `lane clean`, `daemon stop` or reset-state. A failed unit stays failed; nothing restarts it. Daemon shutdown stops units and leaves containers running; `daemon.shutdown` (`ShutdownReason::Explicit`) runs the `lane.stop` tasks of lanes with live units first and answers with what it stopped and ran, and a signal (`ShutdownReason::Signal`) skips the tasks. With `EngineOptions::idle_exit` (a daemon a client spawned), `Handler::idle` resolves once the last event subscriber left and nothing ran or was in flight for the grace period; the server then exits with `ShutdownReason::Idle` if no client is connected.
- Client environments, `secrets unlock` values and secret-provider output stay in memory. After a daemon restart, lanes that should run report `needs_env` until a `start` or `lane resume` from a shell.
- At startup, unfinished `unit_runs` whose recorded PID still has the recorded start time stay running and show as `orphaned`; `stop`, `restart`, `lane clean` and reset-state terminate them, checking each identity first. Containers are re-adopted by label and never stopped.
- While a service unit runs, each `services.<name>.sync` entry is mirrored into its `to` folder (`local_sync.rs`, using `devlanes-sync`); `sync.status` lists them with `kind: local`.
- The binary refuses to start the daemon when `config.yaml` has an unknown or invalid key, naming the file and line; `doctor` checks the file again on every call, so an edit that would stop the next start shows up before it.
- Every service start, replicas and restarts included, waits for a slot from the start gate (`pacing.rs`). Waiting units are `queued` in `status` and `debug why`; `stop` withdraws them, and `Selection.all_at_once` skips the gate for one request. A start whose timeout passes while units are still queued returns `timed_out`, and those units start when their slot comes. A `lane.warning` event reports when the units being started usually peak above the available memory.
- Each start and restart gets a `start_id`, on its `lane.phase` events and its `StartResult`; `services:started` lists in `units` the units it spawns. `start`, `restart`, `resume`, `lane.new` and `wait` given `"events": true` stream their lanes' `lane.phase` (own starts only), `task`, `unit.changed` and `lane.changed` events on the request (`stream.rs`), so a client needs no `subscribe` connection.
- While a phase runs, `lane.phase` events with `state: progress` say what it waits on (`phase_progress.rs`): during infra, each container's state from `InfraProvider::progress`, polled once a second and sent only when it changes; during setup and tasks, the entry or task running with its index and count.

Lane discovery and port blocks are in `registry.rs`, the start pipeline and stop/restart/clean in `lifecycle.rs`, method handlers in `control.rs` and `query.rs`, startup reconciliation in `reconcile.rs`. Integration tests with fake runner, infra and process table are in `tests/it/`.

## License

Apache-2.0

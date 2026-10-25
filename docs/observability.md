# Observability

How devlanes reports what it does today, how OpenTelemetry tracing could be added on top of that, and how to benchmark the CLI and daemon. Only the first section is implemented. devlanes sends nothing anywhere; anonymous usage telemetry is not planned and would be a separate decision.

## Current instrumentation

Both processes use `tracing`. Targets are crate paths (`devlanes_client`, `devlanes_daemon::lifecycle`, `devlanes_infra_compose::cli`), so `RUST_LOG`-style filters select them.

Spans in the daemon:

| Span | Where | Fields |
| --- | --- | --- |
| `request` | every request, in `serve_client` | `id`, `method`, `session`, `tap` when verbose |
| `phase` | each start phase (`infra`, `resolve`, `setup`, `tasks`, `services`, `after-services`) | `name`, `lane` |
| `unit` | one service's paced start | `unit`, `lane` |
| `readiness` | a unit's readiness check, a child of `unit` | |

The client has `spawn_daemon` around an auto-spawn, and its events carry `timing` and `elapsed_ms` where a duration matters (connect and hello, spawn until ready, each request).

Outputs:

- `daemon.log`, filtered by `DEVLANES_LOG` (default `info`).
- `-v`/`-vv` on any command. The client prints its own events to stderr. Requests carry `verbose` in their `Context`; the daemon registers a tap for the request (`devlanes_daemon::diagnostics`), and a `tracing` layer sends every record and span close under that request's span back as `diagnostic` events before the response. The tap's filter passes nothing while no tap is active, so idle cost is one atomic load per debug call site.
- The timing table at the end of a `-v` run, built from client `timing` events and the daemon's span closes.

Rules that every new field follows: environment values are never recorded at any level, only variable names (`env_keys`); command lines are recorded for commands devlanes builds (Docker Compose), and only the argument count for a unit's own command, whose arguments can be interpolated secrets. The diagnostics layer also replaces fields named like `secret`, `password`, `token` or `value`.

Spans only follow work across `tokio::spawn` when the spawned future is instrumented. New background work that belongs to a request (a probe, a spawned step) should use `Instrument::instrument(span)` or `.in_current_span()`.

## Adding OpenTelemetry traces

The spans above map one to one onto OpenTelemetry spans through `tracing-opentelemetry`; no call site changes.

Daemon: add a `tracing_opentelemetry::layer()` next to the file and diagnostics layers in `init_logging`, with a tracer from `opentelemetry-otlp`. Build it only when `OTEL_EXPORTER_OTLP_ENDPOINT` (or `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`) is set, and honour `OTEL_SERVICE_NAME`, `OTEL_RESOURCE_ATTRIBUTES` and `OTEL_TRACES_SAMPLER` as the SDK reads them. Use the batch span processor on the daemon's runtime and shut the tracer provider down in the daemon's shutdown path so the last spans are flushed. Resource attributes: `service.name = devlanes-daemon`, `service.version` and the build id. The daemon inherits `OTEL_*` only once they are added to the auto-spawn allowlist in `devlanes-client` (`DAEMON_ENV_PREFIXES`); a daemon installed as a login service needs them in its service definition.

Client: the same layer in `verbose::init`'s subscriber (or a subscriber of its own when `-v` is off), with `service.name = devlanes-cli`. A root span `command` with the subcommand path wraps `commands::dispatch`. The CLI is short-lived, so it must flush before exit with a bounded timeout (200 ms is a reasonable start) to keep commands fast when the collector is slow or down. A simple span processor with an HTTP exporter fits a process that lives for one request better than a batch processor.

Linking client and daemon: add `trace: Option<String>` to `devlanes_proto::Context`, holding a W3C `traceparent` (`00-<trace id>-<span id>-<flags>`). The client injects the current span's context with the `TraceContextPropagator` before `begin_request`. The daemon extracts it in `request_span` and calls `OpenTelemetrySpanExt::set_parent` on the `request` span, so one trace shows `command` (client), `request`, `phase`, `unit` and `readiness`. The field is additive: older daemons ignore it, and a daemon that receives no `trace` starts its own root. `Context` is never logged, and `traceparent` holds no secrets.

Dependency cost: `opentelemetry`, `opentelemetry_sdk`, `opentelemetry-otlp` and `tracing-opentelemetry`. The OTLP exporter brings either `tonic`, `prost` and `hyper` (gRPC) or an HTTP client (`reqwest` or `hyper`) with `prost` (HTTP/protobuf). The daemon already links `hyper` through the proxy, so HTTP/protobuf over the existing `hyper` adds the least. Measure before deciding: `cargo tree -e normal -p devlanes | wc -l` and the release binary size with and without the change. If the cost matters, put it behind a cargo feature (`otel`) that release builds enable, so the runtime switch stays the environment variable.

Redaction does not change: the same fields become span attributes, so the rules in the previous section apply to exported spans. Log records can be exported as span events; keep that off by default, since `debug` records are numerous.

## Benchmarks

The end-to-end harness (`devlanes-e2e`) already gives each test an isolated home, repository and port slot and drives the real binary, so benchmarks belong there as tests named `bench_*`, marked `#[ignore = "benchmark"]` and run with `cargo nextest run --profile e2e --run-ignored only -E 'test(/^bench_/)'`.

Scenarios, each repeated (10 runs, then median and p90):

- Cold CLI: `devlanes --version`, which starts no daemon. Process start and argument parsing.
- Auto-spawn: the first `devlanes status` in a fresh home. The client's `daemon spawn until ready` timing separates the spawn from the request.
- Warm request: `devlanes status` with the daemon up, from the `request status` timing.
- Start to ready: `devlanes start` on `fixtures/two-services` and `fixtures/many-services` (40 services), and `docker_`-prefixed variants on `fixtures/compose-pg-redis`. The daemon's `phase` span durations give infra, setup, tasks and services separately.
- Restart of one unit in a running lane.

To read timings without parsing the human table, `-v` could write them as JSON when `DEVLANES_TIMINGS_FILE` is set: one object per row with `label`, `ms` and `source` (`client` or `daemon`). The benchmark tests set it, read the file and append one JSON line per scenario and run to `target/bench/results.jsonl` with the commit, OS and fixture.

In CI, a job on `main` (and on demand for pull requests) runs the benchmarks on one fixed runner type, uploads `results.jsonl` as an artifact and writes a Markdown summary to `GITHUB_STEP_SUMMARY`. Comparing with the latest `main` artifact and flagging a regression above a threshold (say 20 % on a median) can come later; shared runners are noisy, so it should warn rather than fail until the numbers prove stable. Docker scenarios stay in the manual Docker job, like the other `docker_` tests.

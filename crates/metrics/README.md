# devlanes-metrics

Platform-neutral resource monitoring for devlanes: the `MetricsSource` trait, samples, roll-ups from processes to units, lanes and projects, in-memory history, per-run summaries and baselines. The daemon drives it; `devlanes-metrics-macos`, `-linux` and `-docker` implement the trait, and the binary registers them.

## Usage

```rust
use devlanes_core::{LaneId, ProjectId, UnitRunId};
use devlanes_metrics::{History, RunTracker, Sample, Subject, UnitKey, aggregate};

let unit = UnitKey { project: ProjectId(1), lane: LaneId(2), unit: "web".into() };
let leaf = Sample {
    subject: Subject::Process { unit: unit.clone(), pid: 42, start_time: 7 },
    cpu_percent: 12.5,
    footprint_bytes: 300 << 20,
    rss_bytes: 80 << 20,
    ts_ms: 61_000,
};

let totals = aggregate(&[leaf]); // unit, lane and project totals
let mut history = History::default();
history.record_all(&totals);

let mut runs = RunTracker::default();
runs.start(UnitRunId(1), unit, 60_000);
runs.observe_all(&totals);

let rollups = history.drain_rollups(120_000); // closed minutes, ready for SQLite
let summary = runs.finish(UnitRunId(1), 120_000); // p50/p95/peak for the run
```

## API

- `MetricsSource`: async collector trait; takes a `SampleRequest` of `UnitTarget`s and `ContainerTarget`s.
- `Sample`, `Subject`, `UnitKey`: one reading and what it measures. `Subject::key()` is a stable persistence key.
- `aggregate`: sums process and container samples into unit, lane and project totals.
- `History`: 1 s ring (15 minutes) and 10 s ring (6 hours) per subject, plus one-minute `RollupRecord`s from `drain_rollups`.
- `RunTracker`, `RunSummary`: per-run p50, p95 and peak of footprint and CPU.
- `baseline`: median of the p95 values of a unit's recent runs (last 10 runs of at least 2 minutes by default).

## Config example

```yaml
# ~/.devlanes/config.yaml: how long CPU and memory history is kept.
metrics:
  retention: 30d
```

The daemon deletes one-minute roll-ups older than this once a day; finished unit and task runs are kept for 30 days regardless.

## Behaviour

CPU is a percentage of one core, so two busy cores read 200. `footprint_bytes` is the memory number to show: `phys_footprint` on macOS, PSS on Linux, and the docker CLI's cache-free usage for containers. RSS is kept for detail views.

This crate measures and summarises. Nothing in it limits, throttles or signals processes.

## License

Apache-2.0

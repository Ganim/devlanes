# devlanes-metrics-macos

macOS process collector for `devlanes-metrics`, built on libproc. The binary registers it as a `MetricsSource`; the daemon passes in each unit's session ids and tracked processes. On other platforms the crate is empty.

## Usage

```rust
use devlanes_core::{LaneId, ProjectId};
use devlanes_metrics::{MetricsSource, SampleRequest, UnitKey, UnitTarget};
use devlanes_metrics_macos::{MacosSource, process_identity};

let source = MacosSource::new()?;
let leader = process_identity(std::process::id()).expect("process is running");
let request = SampleRequest {
    units: vec![UnitTarget {
        unit: UnitKey { project: ProjectId(1), lane: LaneId(1), unit: "web".into() },
        sessions: vec![leader.sid],
        processes: vec![leader],
    }],
    containers: Vec::new(),
};
let samples = source.sample(&request).await?; // one Subject::Process sample per member
```

`MacosSource::sample_blocking` does the same on the calling thread.

## Membership

A process belongs to a unit when it is in one of the unit's sessions, is a tracked process whose start time still matches, or descends from either by parent PID. Only processes owned by the daemon's user are read. A tracked PID with a different start time was reused and is skipped. Start times use the same token as `devlanes-sys`.

## Memory and CPU

Memory is `phys_footprint` from `proc_pid_rusage(RUSAGE_INFO_V6)`, the number Activity Monitor shows. RSS is reported too, but it drops sharply once idle pages are compressed. CPU times are mach ticks converted with `mach_timebase_info`. A process seen for the first time reports its lifetime average.

The collector only reads process data and never limits or signals anything.

## License

Apache-2.0

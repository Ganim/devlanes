# devlanes-metrics-linux

Linux process collector for `devlanes-metrics`, reading procfs. The binary registers it as a `MetricsSource`; the daemon passes in each unit's session ids and tracked processes. The procfs parsers build on every platform, so their fixture tests also run on macOS.

## Usage

```rust
use devlanes_core::{LaneId, ProjectId};
use devlanes_metrics::{MetricsSource, SampleRequest, UnitKey, UnitTarget};
use devlanes_metrics_linux::{LinuxSource, process_identity};

async fn sample_self() -> Result<(), Box<dyn std::error::Error>> {
    let source = LinuxSource::new();
    let leader = process_identity(std::process::id()).expect("process is running");
    let request = SampleRequest {
        units: vec![UnitTarget {
            unit: UnitKey { project: ProjectId(1), lane: LaneId(1), unit: "web".into() },
            sessions: vec![leader.sid],
            processes: vec![leader],
        }],
        ..SampleRequest::default()
    };
    let samples = source.sample(&request).await?;
    println!("{} samples", samples.len());
    Ok(())
}
```

Parsers work on text from anywhere:

```rust
fn parse() -> Result<(), Box<dyn std::error::Error>> {
    let stat = devlanes_metrics_linux::procfs::parse_stat(&std::fs::read_to_string("/proc/self/stat")?)?;
    let psi = devlanes_metrics_linux::pressure::parse_psi(&std::fs::read_to_string("/proc/pressure/memory")?)?;
    println!("{stat:?} {psi:?}");
    Ok(())
}
```

`LinuxPressure` is the start gate's `PressureSource`:

```rust
use devlanes_core::gate::PressureSource;

async fn pressure() -> Result<(), Box<dyn std::error::Error>> {
    let pressure = devlanes_metrics_linux::LinuxPressure::new();
    let sample = pressure.sample().await?; // cpu_percent is None on the first call
    println!("{:?}", sample.cpu_percent);
    Ok(())
}
```

## Behaviour

Membership is a unit's sessions, tracked processes whose `starttime` still matches, and their descendants by parent PID. CPU comes from `utime + stime` in clock ticks, RSS from `statm`. The memory number is PSS from `smaps_rollup`, which splits shared pages between processes so a tree's sum is meaningful. Reading it walks page tables, so it is refreshed every 10 seconds by default (`LinuxSource::with_pss_interval`), and RSS is used until the first read or where the file is unreadable.

When the request names the daemon's own process, it is read by PID alone and reported as one `Subject::Daemon` sample, also when there are no units. Its children are not included, and it never counts for a unit.

For the start gate, system CPU is the busy share of all cores between two `/proc/stat` reads, and available memory is `MemAvailable` from `/proc/meminfo`. Memory pressure comes from `/proc/pressure/memory`: warn at `some avg10` of 10% or more, critical at `full avg10` of 10% or more. When the daemon runs in a cgroup v2 group (a container or a systemd unit), that group's `memory.pressure` is used instead, and available memory is capped at `memory.max - memory.current`, because the services it starts share the limit. Kernels or containers without PSI report no level, which is logged once; the gate then treats memory pressure as normal.

The collector only reads procfs and never limits or signals anything.

## License

Apache-2.0

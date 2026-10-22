# devlanes-metrics-windows

Windows process collector for `devlanes-metrics`, built on the `devlanes-sys` Win32 wrappers. The binary registers it as a `MetricsSource`; the daemon passes in each unit's tracked processes (its job's members). On other platforms the crate is empty.

## Usage

```rust
use devlanes_core::{LaneId, ProjectId};
use devlanes_metrics::{MetricsSource, SampleRequest, UnitKey, UnitTarget};
use devlanes_metrics_windows::{WindowsSource, process_identity};

async fn sample_self() -> Result<(), Box<dyn std::error::Error>> {
    let source = WindowsSource::new();
    let leader = process_identity(std::process::id()).expect("process is running");
    let request = SampleRequest {
        units: vec![UnitTarget {
            unit: UnitKey { project: ProjectId(1), lane: LaneId(1), unit: "web".into() },
            sessions: Vec::new(),
            processes: vec![leader],
        }],
        containers: Vec::new(),
    };
    let samples = source.sample(&request).await?; // one Subject::Process sample per member
    println!("{} samples", samples.len());
    Ok(())
}
```

`WindowsSource::sample_blocking` does the same on the calling thread.

`WindowsPressure` is the start gate's `PressureSource`:

```rust
use devlanes_core::gate::PressureSource;
use devlanes_metrics_windows::WindowsPressure;

async fn pressure() -> Result<(), Box<dyn std::error::Error>> {
    let pressure = WindowsPressure::new();
    let sample = pressure.sample().await?; // cpu_percent is None on the first call
    let available = pressure.available_memory().await?;
    println!("{:?} {available:?}", sample.cpu_percent);
    Ok(())
}
```

## Behaviour

A unit's processes are exactly the identities the daemon passes in: the runner lists its job's members every half second, so the collector does no tree walk of its own and ignores session ids. A PID whose creation time no longer matches was reused and is skipped, as are exited processes and processes that refuse `PROCESS_QUERY_LIMITED_INFORMATION`.

CPU is kernel plus user time from `GetProcessTimes`; a process seen for the first time reports its lifetime average. `footprint_bytes` is the private working set (`PrivateWorkingSetSize` from `PROCESS_MEMORY_COUNTERS_EX2`), the resident memory no other process shares and the number Task Manager's Memory column shows. Windows older than build 20348 does not report it, so private bytes (`PrivateUsage`, the commit charge) stand in there; they also count paged-out memory and read higher. `rss_bytes` is the working set, which includes shared DLL pages.

The collector only reads process data and never limits or signals anything.

## Pressure

System CPU is the busy share of all CPUs between two `GetSystemTimes` calls (kernel time includes idle time). Windows has no pressure stall information, so the memory level comes from `GlobalMemoryStatusEx`'s memory load: warn at 85% of physical memory in use, critical at 95%. Available memory is its available physical memory (free plus standby pages).

## License

Apache-2.0

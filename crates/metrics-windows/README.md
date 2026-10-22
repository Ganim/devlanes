# devlanes-metrics-windows

Windows process collector for `devlanes-metrics`, built on the `devlanes-sys` Win32 wrappers. The binary registers it as a `MetricsSource`; the daemon passes in each unit's tracked processes (its job's members plus their descendants by parent PID). On other platforms the crate is empty.

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

A unit's processes are exactly the identities the daemon passes in: every half second the runner snapshots its job's members and their descendants by parent PID (including ones outside the job), so the collector does no tree walk of its own and ignores session ids. A PID whose creation time no longer matches was reused and is skipped, as are exited processes and processes that refuse `PROCESS_QUERY_LIMITED_INFORMATION`. A child that starts and exits between two snapshots is never seen, so its CPU is missing from the unit.

CPU is kernel plus user time from `GetProcessTimes`; a process seen for the first time reports its lifetime average, or 0 when it is younger than a second, since startup bursts over a few milliseconds of age would read as several cores. `footprint_bytes` is the private working set (`PrivateWorkingSetSize` from `PROCESS_MEMORY_COUNTERS_EX2`), the resident memory no other process shares and the number Task Manager's Memory column shows. It needs Windows 11 22H2 with the September 2023 update; on 21H2 and unpatched systems, or when it reads 0, private bytes (`PrivateUsage`, the commit charge) stand in. They also count paged-out memory and read higher. `rss_bytes` is the working set, which includes shared DLL pages.

The collector only reads process data and never limits or signals anything.

## Pressure

System CPU is the busy share of all CPUs between two `GetSystemTimes` calls (kernel time includes idle time). Windows has no pressure stall information. The memory level is critical while Windows signals low memory (`QueryMemoryResourceNotification` on a `LowMemoryResourceNotification` object, set near exhaustion: about 32 MB free per 4 GB of RAM, at most 64 MB). It is warn when available physical memory is under both 10% of RAM and 2 GiB, so a large machine still has room for a dev server or two and a small one is judged by its share. The memory load percentage is not used, because Docker Desktop's VM and the file cache keep it above 85% on ordinary dev machines. Available memory is `GlobalMemoryStatusEx`'s available physical memory (free plus standby pages).

## License

Apache-2.0

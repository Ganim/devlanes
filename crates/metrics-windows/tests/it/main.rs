//! Live collector and pressure checks against real processes. They exist on
//! every platform so the test list is stable, but only run on Windows.

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn busy_child_reports_cpu_after_two_samples() {
    #[cfg(windows)]
    windows::busy_child_reports_cpu_after_two_samples();
}

/// Entry point re-executed by `busy_child_reports_cpu_after_two_samples`.
/// Ignored in normal runs and inert without the marker variable.
#[test]
#[ignore = "helper process entry point, started by busy_child_reports_cpu_after_two_samples"]
fn helper_busy_loop() {
    #[cfg(windows)]
    windows::helper_busy_loop();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn own_process_reports_memory() {
    #[cfg(windows)]
    windows::own_process_reports_memory();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn unknown_stale_and_exited_processes_are_skipped() {
    #[cfg(windows)]
    windows::unknown_stale_and_exited_processes_are_skipped();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn shared_process_counts_for_the_first_unit() {
    #[cfg(windows)]
    windows::shared_process_counts_for_the_first_unit();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn daemon_is_sampled_alone() {
    #[cfg(windows)]
    windows::daemon_is_sampled_alone();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn pressure_readings_are_in_range() {
    #[cfg(windows)]
    windows::pressure_readings_are_in_range();
}

#[cfg(windows)]
mod windows {
    use std::{
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use devlanes_core::{LaneId, ProcessIdentity, ProjectId, gate::PressureSource};
    use devlanes_metrics::{
        ContainerTarget, MetricsSource, Sample, SampleRequest, Subject, UnitKey, UnitTarget,
    };
    use devlanes_metrics_windows::{WindowsPressure, WindowsSource, process_identity};

    /// Upper bound for every wait so a stuck reading cannot hang the suite.
    const TIMEOUT: Duration = Duration::from_secs(10);
    /// Marks the self-reexec helper process; see [`helper_busy_loop`].
    const HELPER_ENV: &str = "DEVLANES_METRICS_WINDOWS_HELPER";
    /// How long the helper spins at most, so it ends on its own even when the
    /// test that started it is killed before its cleanup runs.
    const HELPER_LIFETIME: Duration = Duration::from_secs(30);

    /// Kills and reaps a test-owned child when the test ends, even on panic.
    struct Owned(Child);

    impl Drop for Owned {
        /// Best-effort cleanup; the child may already have exited.
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Starts this test binary as [`helper_busy_loop`], spinning one core.
    fn busy() -> Owned {
        let exe = std::env::current_exe().expect("test binary path");
        Owned(
            Command::new(exe)
                .args(["--exact", "helper_busy_loop", "--ignored", "--nocapture"])
                .env(HELPER_ENV, "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn helper"),
        )
    }

    /// Builds a unit target named `name`; sessions mean nothing on Windows.
    fn target(name: &str, processes: Vec<ProcessIdentity>) -> UnitTarget {
        UnitTarget {
            unit: UnitKey {
                project: ProjectId(1),
                lane: LaneId(1),
                unit: name.into(),
            },
            sessions: Vec::new(),
            processes,
        }
    }

    /// Finds the sample for `pid`.
    fn sample_for(samples: &[Sample], pid: u32) -> Option<&Sample> {
        samples
            .iter()
            .find(|sample| matches!(sample.subject, Subject::Process { pid: p, .. } if p == pid))
    }

    /// A current-thread runtime for the async trait methods.
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
    }

    pub fn helper_busy_loop() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let started = Instant::now();
        let mut counter: u64 = 0;
        while started.elapsed() < HELPER_LIFETIME {
            counter = std::hint::black_box(counter.wrapping_add(1));
        }
    }

    pub fn busy_child_reports_cpu_after_two_samples() {
        let child = busy();
        let identity = process_identity(child.0.id()).expect("child identity");
        let source = WindowsSource::new();
        let units = [target("busy", vec![identity])];
        let first = source.sample_blocking(&units, None);
        let sample = sample_for(&first, identity.pid).expect("child sampled");
        assert_eq!(sample.cpu_percent, 0.0, "first sight reads no CPU");
        assert!(matches!(
            sample.subject,
            Subject::Process { start_time, .. } if start_time == identity.start_time
        ));
        // A loaded machine can starve the child for a tick; wait (bounded) for a
        // delta reading that shows it spinning.
        let deadline = Instant::now() + TIMEOUT;
        let cpu = loop {
            thread::sleep(Duration::from_millis(300));
            let samples = source.sample_blocking(&units, None);
            let cpu = sample_for(&samples, identity.pid)
                .expect("child sampled")
                .cpu_percent;
            if cpu > 20.0 {
                break cpu;
            }
            assert!(
                Instant::now() < deadline,
                "no CPU within the timeout: {cpu}%"
            );
        };
        // One spinning thread: a unit error (100 ns ticks as ns) would read 100x off.
        assert!(cpu <= 150.0, "cpu {cpu}%");
    }

    pub fn own_process_reports_memory() {
        let identity = process_identity(std::process::id()).expect("own identity");
        assert_eq!(identity.sid, 0);
        let source = WindowsSource::new();
        let request = SampleRequest {
            units: vec![target("self", vec![identity])],
            containers: vec![ContainerTarget {
                project: ProjectId(1),
                lane: None,
                id: "acme-db".into(),
            }],
            daemon: None,
        };
        let samples = runtime()
            .block_on(async { tokio::time::timeout(TIMEOUT, source.sample(&request)).await })
            .expect("sample in time")
            .expect("sample");
        // Containers belong to the Docker source.
        assert_eq!(samples.len(), 1, "{samples:?}");
        let sample = sample_for(&samples, identity.pid).expect("own process sampled");
        assert!(sample.footprint_bytes > 0, "{sample:?}");
        // The working set holds the private pages plus shared ones.
        assert!(sample.rss_bytes >= sample.footprint_bytes, "{sample:?}");
        assert!(sample.ts_ms > 0);
        assert_eq!(source.name(), "windows");
    }

    pub fn unknown_stale_and_exited_processes_are_skipped() {
        let source = WindowsSource::new();
        let me = process_identity(std::process::id()).expect("own identity");
        let stale = ProcessIdentity {
            start_time: me.start_time + 1,
            ..me
        };
        // PID 0 is the idle process, which nobody can open; Windows PIDs are
        // multiples of four, so this one never names a process.
        let unknown = [0, u32::MAX - 2].map(|pid| ProcessIdentity {
            pid,
            sid: 0,
            start_time: 1,
        });
        assert!(
            source
                .sample_blocking(&[target("gone", vec![stale, unknown[0], unknown[1]])], None)
                .is_empty()
        );

        let mut child = busy();
        let identity = process_identity(child.0.id()).expect("child identity");
        child.0.kill().expect("kill child");
        // `wait` reaps, but `child` keeps its handle, so the exited process
        // object and its PID remain.
        child.0.wait().expect("wait child");
        let samples = source.sample_blocking(&[target("exited", vec![identity, me])], None);
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(sample_for(&samples, me.pid).is_some());
        assert!(source.sample_blocking(&[], None).is_empty());
    }

    pub fn shared_process_counts_for_the_first_unit() {
        let me = process_identity(std::process::id()).expect("own identity");
        let source = WindowsSource::new();
        let samples = source.sample_blocking(
            &[target("first", vec![me, me]), target("second", vec![me])],
            None,
        );
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(matches!(
            &samples[0].subject,
            Subject::Process { unit, .. } if unit.unit == "first"
        ));
    }

    pub fn daemon_is_sampled_alone() {
        // The test process plays the daemon; the busy child stands in for a unit
        // process.
        let child = busy();
        let child_identity = process_identity(child.0.id()).expect("child identity");
        let me = process_identity(std::process::id()).expect("own identity");
        let source = WindowsSource::new();

        for _ in 0..2 {
            let samples = source.sample_blocking(&[], Some(me));
            assert_eq!(
                samples.len(),
                1,
                "only the daemon, no children: {samples:?}"
            );
            assert_eq!(samples[0].subject, Subject::Daemon);
            assert!(samples[0].footprint_bytes > 0, "{samples:?}");
        }

        // A unit that lists the daemon's PID does not count it.
        let samples = source.sample_blocking(&[target("unit", vec![me, child_identity])], Some(me));
        let daemons = samples
            .iter()
            .filter(|sample| sample.subject == Subject::Daemon)
            .count();
        assert_eq!(daemons, 1, "{samples:?}");
        assert!(sample_for(&samples, me.pid).is_none(), "{samples:?}");
        assert!(sample_for(&samples, child_identity.pid).is_some());

        let stale = ProcessIdentity {
            start_time: me.start_time + 1,
            ..me
        };
        assert!(source.sample_blocking(&[], Some(stale)).is_empty());

        let request = SampleRequest {
            daemon: Some(me),
            ..SampleRequest::default()
        };
        let samples = runtime()
            .block_on(async { tokio::time::timeout(TIMEOUT, source.sample(&request)).await })
            .expect("sample in time")
            .expect("sample");
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert_eq!(samples[0].subject, Subject::Daemon);
    }

    pub fn pressure_readings_are_in_range() {
        let pressure = WindowsPressure::new();
        let runtime = runtime();
        let first = runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, pressure.sample()).await })
            .expect("sample in time")
            .expect("first reading");
        assert_eq!(first.cpu_percent, None);
        assert!(first.memory.is_some(), "memory level");
        assert_eq!(first.cpu_stall_percent, None);
        let deadline = Instant::now() + TIMEOUT;
        let cpu = loop {
            thread::sleep(Duration::from_millis(100));
            let reading = pressure.sample_blocking().expect("reading");
            if let Some(cpu) = reading.cpu_percent {
                break cpu;
            }
            assert!(
                Instant::now() < deadline,
                "no CPU reading within the timeout"
            );
        };
        assert!((0.0..=100.0).contains(&cpu), "{cpu}");
        let available = runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, pressure.available_memory()).await })
            .expect("read in time")
            .expect("memory status")
            .expect("reported");
        assert!(available > 0);
        let total = runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, pressure.total_memory()).await })
            .expect("read in time")
            .expect("memory status")
            .expect("reported");
        assert!(total > 0 && total >= available, "{total} {available}");
        assert_eq!(pressure.name(), "windows");
    }
}

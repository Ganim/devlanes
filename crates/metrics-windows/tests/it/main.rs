//! Live collector and pressure checks against real child processes. They exist on
//! every platform so the test list is stable, but only run on Windows.

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn busy_child_reports_cpu_and_memory() {
    #[cfg(windows)]
    windows::busy_child_reports_cpu_and_memory();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn stale_shared_and_exited_processes() {
    #[cfg(windows)]
    windows::stale_shared_and_exited_processes();
}

#[test]
#[cfg_attr(not(windows), ignore = "needs Windows")]
fn async_sample_ignores_containers() {
    #[cfg(windows)]
    windows::async_sample_ignores_containers();
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
        os::windows::process::CommandExt,
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

    /// Kills and reaps a test-owned child when the test ends, even on panic.
    struct Owned(Child);

    impl Drop for Owned {
        /// Best-effort cleanup; the child may already have exited.
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// A `cmd` spinning one core until killed.
    fn busy() -> Owned {
        let mut command = Command::new("cmd");
        command.raw_arg("/d /c \"for /l %i in (0,0,1) do @rem\"");
        spawn(command)
    }

    /// A `ping` waiting about 30 seconds without using CPU. Started directly, not
    /// through `cmd`, so the process the test samples and kills is the one waiting.
    fn idle() -> Owned {
        let mut command = Command::new("ping");
        command.args(["-n", "30", "127.0.0.1"]);
        spawn(command)
    }

    /// Starts `command` with no inherited input or output.
    fn spawn(mut command: Command) -> Owned {
        Owned(
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn test child"),
        )
    }

    /// Builds a unit target named `name` (sessions are ignored on Windows).
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

    pub fn busy_child_reports_cpu_and_memory() {
        let child = busy();
        let identity = process_identity(child.0.id()).expect("child identity");
        let source = WindowsSource::new();
        let units = [target("busy", vec![identity])];
        let first = source.sample_blocking(&units);
        let sample = sample_for(&first, identity.pid).expect("child sampled");
        assert!(sample.footprint_bytes > 0, "{sample:?}");
        assert!(sample.rss_bytes > 0, "{sample:?}");
        assert!(matches!(
            sample.subject,
            Subject::Process { start_time, .. } if start_time == identity.start_time
        ));
        // A loaded machine can starve the child for a tick; wait (bounded) for a
        // delta reading that shows it spinning.
        let deadline = Instant::now() + TIMEOUT;
        let cpu = loop {
            thread::sleep(Duration::from_millis(300));
            let samples = source.sample_blocking(&units);
            let cpu = sample_for(&samples, identity.pid)
                .expect("child sampled")
                .cpu_percent;
            if cpu > 20.0 {
                break cpu;
            }
            assert!(Instant::now() < deadline, "no CPU in 10 s: {cpu}%");
        };
        // One spinning thread: a unit error (100 ns ticks as ns) would read 100x off.
        assert!(cpu <= 150.0, "cpu {cpu}%");
    }

    pub fn stale_shared_and_exited_processes() {
        let child = idle();
        let identity = process_identity(child.0.id()).expect("child identity");
        let source = WindowsSource::new();
        let stale = ProcessIdentity {
            start_time: identity.start_time + 1,
            ..identity
        };
        assert!(
            source
                .sample_blocking(&[target("stale", vec![stale])])
                .is_empty()
        );

        // A process listed by two units counts for the first one only.
        let samples = source.sample_blocking(&[
            target("first", vec![identity]),
            target("second", vec![identity]),
        ]);
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(matches!(
            &samples[0].subject,
            Subject::Process { unit, .. } if unit.unit == "first"
        ));

        let mut child = child;
        child.0.kill().expect("kill child");
        // `wait` reaps, but `child` keeps its handle, so the exited process remains.
        child.0.wait().expect("wait child");
        assert!(
            source
                .sample_blocking(&[target("exited", vec![identity])])
                .is_empty()
        );
    }

    pub fn async_sample_ignores_containers() {
        let child = idle();
        let identity = process_identity(child.0.id()).expect("child identity");
        let source = WindowsSource::new();
        let request = SampleRequest {
            units: vec![target("idle", vec![identity])],
            containers: vec![ContainerTarget {
                project: ProjectId(1),
                lane: None,
                id: "acme-db".into(),
            }],
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let samples = runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, source.sample(&request)).await })
            .expect("sample in time")
            .expect("sample");
        assert_eq!(samples.len(), 1, "{samples:?}");
        assert!(sample_for(&samples, identity.pid).is_some());
        assert_eq!(source.name(), "windows");
    }

    pub fn pressure_readings_are_in_range() {
        let pressure = WindowsPressure::new();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let first = runtime
            .block_on(async { tokio::time::timeout(TIMEOUT, pressure.sample()).await })
            .expect("sample in time")
            .expect("first reading");
        assert_eq!(first.cpu_percent, None);
        assert!(first.memory.is_some(), "memory level");
        let deadline = Instant::now() + TIMEOUT;
        let cpu = loop {
            thread::sleep(Duration::from_millis(100));
            let reading = pressure.sample_blocking().expect("reading");
            if let Some(cpu) = reading.cpu_percent {
                break cpu;
            }
            assert!(Instant::now() < deadline, "no CPU reading in 10 s");
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
        assert!(total >= available, "{total} {available}");
        assert_eq!(pressure.name(), "windows");
    }
}

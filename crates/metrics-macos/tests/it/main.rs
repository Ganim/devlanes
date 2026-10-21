//! Live collector checks against `ps`. They exist on every platform so the test
//! list is stable, but only run on macOS.

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "needs macOS")]
fn rss_matches_ps_and_footprint_is_reported() {
    #[cfg(target_os = "macos")]
    macos::rss_matches_ps_and_footprint_is_reported();
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "needs macOS")]
fn session_members_and_descendants_are_sampled() {
    #[cfg(target_os = "macos")]
    macos::session_members_and_descendants_are_sampled();
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "needs macOS")]
fn reused_pid_is_rejected_by_start_time() {
    #[cfg(target_os = "macos")]
    macos::reused_pid_is_rejected_by_start_time();
}

#[test]
#[cfg_attr(not(target_os = "macos"), ignore = "needs macOS")]
fn busy_process_cpu_uses_converted_ticks() {
    #[cfg(target_os = "macos")]
    macos::busy_process_cpu_uses_converted_ticks();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use devlanes_core::{LaneId, ProjectId};
    use devlanes_metrics::{MetricsSource, Sample, SampleRequest, Subject, UnitKey, UnitTarget};
    use devlanes_metrics_macos::{MacosSource, process_identity};

    /// Upper bound for helper commands so a wedged `ps` cannot hang the suite.
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

    /// Kills and reaps a test-owned child when the test ends, even on panic.
    struct Owned(Child);

    impl Drop for Owned {
        /// Best-effort cleanup; the child may already have exited.
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// Spawns a test-owned child with no inherited output.
    fn spawn(program: &str, args: &[&str]) -> Owned {
        Owned(
            Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn child"),
        )
    }

    /// Runs `ps -o rss= -p <pid>` with a timeout and returns RSS in bytes.
    fn ps_rss_bytes(pid: u32) -> u64 {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let output = Command::new("/bin/ps")
                .args(["-o", "rss=", "-p", &pid.to_string()])
                .output();
            let _ = send.send(output);
        });
        let output = receive
            .recv_timeout(COMMAND_TIMEOUT)
            .expect("ps finished in time")
            .expect("run ps");
        assert!(output.status.success(), "ps failed: {output:?}");
        let kib: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("ps rss is a number");
        kib * 1024
    }

    /// Builds a unit target named `name`.
    fn target(
        name: &str,
        sessions: Vec<u32>,
        processes: Vec<devlanes_core::ProcessIdentity>,
    ) -> UnitTarget {
        UnitTarget {
            unit: UnitKey {
                project: ProjectId(1),
                lane: LaneId(1),
                unit: name.into(),
            },
            sessions,
            processes,
        }
    }

    /// Finds the sample for `pid`.
    fn sample_for(samples: &[Sample], pid: u32) -> Option<&Sample> {
        samples
            .iter()
            .find(|sample| matches!(sample.subject, Subject::Process { pid: p, .. } if p == pid))
    }

    pub fn rss_matches_ps_and_footprint_is_reported() {
        let child = spawn("/bin/sleep", &["30"]);
        let pid = child.0.id();
        let identity = process_identity(pid).expect("child identity");
        let source = MacosSource::new().expect("source");
        // Let exec and dyld finish mapping pages so both reads see a settled process.
        thread::sleep(Duration::from_millis(300));
        let samples = source
            .sample_blocking(&[target("sleep", Vec::new(), vec![identity])])
            .expect("sample");
        let sample = sample_for(&samples, pid).expect("child sampled");
        let ps = ps_rss_bytes(pid);
        let difference = sample.rss_bytes.abs_diff(ps);
        // An idle `sleep` barely changes; allow page-granularity drift between reads.
        assert!(
            difference <= (ps / 4).max(256 * 1024),
            "collector rss {} vs ps {ps}",
            sample.rss_bytes
        );
        assert!(sample.footprint_bytes > 0, "phys_footprint reported");
        assert!(matches!(
            sample.subject,
            Subject::Process { start_time, .. } if start_time == identity.start_time
        ));
    }

    pub fn session_members_and_descendants_are_sampled() {
        let own = process_identity(std::process::id()).expect("own identity");
        assert!(own.sid > 1, "test runs in a user session");
        let child = spawn("/bin/sleep", &["30"]);
        let source = MacosSource::new().expect("source");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let request = SampleRequest {
            units: vec![target("self", vec![own.sid], Vec::new())],
            containers: Vec::new(),
        };
        let samples = runtime
            .block_on(async {
                tokio::time::timeout(COMMAND_TIMEOUT, source.sample(&request)).await
            })
            .expect("sample in time")
            .expect("sample");
        assert!(
            sample_for(&samples, std::process::id()).is_some(),
            "own process"
        );
        assert!(
            sample_for(&samples, child.0.id()).is_some(),
            "child via session"
        );

        // Tracking only this process still finds its child through the parent link.
        let samples = source
            .sample_blocking(&[target("tree", Vec::new(), vec![own])])
            .expect("sample");
        assert!(
            sample_for(&samples, child.0.id()).is_some(),
            "child via ppid"
        );
        assert_eq!(source.name(), "macos");
    }

    pub fn reused_pid_is_rejected_by_start_time() {
        let child = spawn("/bin/sleep", &["30"]);
        let mut identity = process_identity(child.0.id()).expect("child identity");
        identity.start_time += 1;
        let source = MacosSource::new().expect("source");
        let samples = source
            .sample_blocking(&[target("stale", Vec::new(), vec![identity])])
            .expect("sample");
        assert!(samples.is_empty(), "stale identity sampled: {samples:?}");
    }

    pub fn busy_process_cpu_uses_converted_ticks() {
        let child = spawn("/usr/bin/yes", &[]);
        let identity = process_identity(child.0.id()).expect("child identity");
        let source = MacosSource::new().expect("source");
        let units = [target("busy", Vec::new(), vec![identity])];
        source.sample_blocking(&units).expect("first sample");
        thread::sleep(Duration::from_millis(500));
        let samples = source.sample_blocking(&units).expect("second sample");
        let cpu = sample_for(&samples, identity.pid)
            .expect("busy sampled")
            .cpu_percent;
        // One spinning thread: unconverted ticks would read about 41x too low or high.
        assert!((10.0..=150.0).contains(&cpu), "cpu {cpu}%");
    }
}

//! procfs parser fixtures (every platform) and live collector checks (Linux).
//!
//! `stat.txt`, `statm.txt`, `smaps_rollup.txt` and `uptime.txt` were captured in
//! a Linux 7.0 container (a `redis-server` PID 1 and a `cat` reading itself).
//! `stat_odd_comm.txt` is the `cat` line with comm and state edited to cover a
//! name containing `) (` and a zombie.

use devlanes_metrics::Error;
use devlanes_metrics_linux::procfs::{
    ProcStat, cpu_percent, lifetime_cpu_percent, parse_smaps_rollup_pss, parse_stat, parse_statm,
    parse_uptime,
};

#[test]
fn stat_fixture_parses_tree_and_cpu_fields() {
    let stat = parse_stat(include_str!("../../fixtures/stat.txt")).expect("stat");
    assert_eq!(
        stat,
        ProcStat {
            pid: 1,
            comm: "redis-server".into(),
            state: 'S',
            ppid: 0,
            pgrp: 1,
            session: 1,
            utime: 20_915,
            stime: 2_515,
            starttime: 3_565,
            rss_pages: 1_362,
        }
    );
    assert!(!stat.is_exited());
}

#[test]
fn stat_comm_with_parentheses_and_zombie_state() {
    let stat = parse_stat(include_str!("../../fixtures/stat_odd_comm.txt")).expect("stat");
    assert_eq!(stat.comm, "node ) (x");
    assert_eq!((stat.pid, stat.ppid, stat.session), (4242, 4000, 4001));
    assert_eq!((stat.utime, stat.stime, stat.starttime), (7, 3, 3_672_748));
    assert!(stat.is_exited());
}

#[test]
fn malformed_stat_is_a_typed_error() {
    for text in ["", "12 redis S 1", "12 (redis) S 1 2"] {
        assert!(
            matches!(parse_stat(text), Err(Error::Malformed { .. })),
            "accepted {text:?}"
        );
    }
}

#[test]
fn statm_smaps_and_uptime_fixtures() {
    let statm = parse_statm(include_str!("../../fixtures/statm.txt")).expect("statm");
    assert_eq!(
        (statm.size, statm.resident, statm.shared),
        (11_660, 1_535, 429)
    );
    let pss = parse_smaps_rollup_pss(include_str!("../../fixtures/smaps_rollup.txt")).expect("pss");
    assert_eq!(pss, 474 * 1024, "Pss total, not Pss_Anon/Pss_File");
    let uptime = parse_uptime(include_str!("../../fixtures/uptime.txt")).expect("uptime");
    assert!((uptime - 36_722.23).abs() < 1e-9);
    assert!(parse_smaps_rollup_pss("Pss_Anon: 3 kB\n").is_err());
    assert!(parse_uptime("nan 1").is_err());
}

#[test]
fn cpu_math_uses_clock_ticks() {
    // 50 ticks at 100 Hz over one second is half a core.
    assert!((cpu_percent(50, 100, 1.0) - 50.0).abs() < 1e-9);
    assert!((cpu_percent(300, 100, 1.5) - 200.0).abs() < 1e-9);
    assert_eq!(cpu_percent(10, 0, 1.0), 0.0);
    assert_eq!(cpu_percent(10, 100, 0.0), 0.0);
    let stat = parse_stat(include_str!("../../fixtures/stat.txt")).expect("stat");
    // 23_430 ticks over (36_722.23 - 35.65) s at 100 Hz.
    let lifetime = lifetime_cpu_percent(&stat, 100, 36_722.23);
    assert!(
        (lifetime - 234.30 / 36_686.58 * 100.0).abs() < 1e-9,
        "{lifetime}"
    );
}

#[test]
#[cfg_attr(not(target_os = "linux"), ignore = "needs Linux")]
fn live_rss_matches_ps_and_reuse_is_rejected() {
    #[cfg(target_os = "linux")]
    linux::live_rss_matches_ps_and_reuse_is_rejected();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{
        process::{Child, Command, Stdio},
        sync::mpsc,
        thread,
        time::Duration,
    };

    use devlanes_core::{LaneId, ProjectId};
    use devlanes_metrics::{Sample, Subject, UnitKey, UnitTarget};
    use devlanes_metrics_linux::{LinuxSource, process_identity};

    /// Upper bound for `ps` so a wedged helper cannot hang the suite.
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

    /// Runs `ps -o rss= -p <pid>` with a timeout and returns RSS in bytes.
    fn ps_rss_bytes(pid: u32) -> u64 {
        let (send, receive) = mpsc::channel();
        thread::spawn(move || {
            let _ = send.send(
                Command::new("ps")
                    .args(["-o", "rss=", "-p", &pid.to_string()])
                    .output(),
            );
        });
        let output = receive
            .recv_timeout(COMMAND_TIMEOUT)
            .expect("ps finished in time")
            .expect("run ps");
        let kib: u64 = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .expect("ps rss is a number");
        kib * 1024
    }

    /// Builds a unit target tracking `processes`.
    fn target(processes: Vec<devlanes_core::ProcessIdentity>) -> UnitTarget {
        UnitTarget {
            unit: UnitKey {
                project: ProjectId(1),
                lane: LaneId(1),
                unit: "sleep".into(),
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

    pub fn live_rss_matches_ps_and_reuse_is_rejected() {
        let child = Owned(
            Command::new("sleep")
                .arg("30")
                .stdin(Stdio::null())
                .spawn()
                .expect("spawn sleep"),
        );
        let pid = child.0.id();
        thread::sleep(Duration::from_millis(300));
        let identity = process_identity(pid).expect("identity");
        let source = LinuxSource::with_pss_interval(Duration::ZERO);
        let samples = source
            .sample_blocking(&[target(vec![identity])])
            .expect("sample");
        let sample = sample_for(&samples, pid).expect("child sampled");
        let ps = ps_rss_bytes(pid);
        assert!(
            sample.rss_bytes.abs_diff(ps) <= (ps / 4).max(256 * 1024),
            "collector rss {} vs ps {ps}",
            sample.rss_bytes
        );
        assert!(sample.footprint_bytes > 0);

        let own = process_identity(std::process::id()).expect("own identity");
        let samples = source
            .sample_blocking(&[target(vec![own])])
            .expect("sample");
        assert!(sample_for(&samples, pid).is_some(), "child via ppid");

        let mut stale = identity;
        stale.start_time += 1;
        let samples = source
            .sample_blocking(&[target(vec![stale])])
            .expect("sample");
        assert!(samples.is_empty(), "stale identity sampled: {samples:?}");
    }
}

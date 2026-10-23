use std::io;

use devlanes_sys_windows::usage::{low_memory, memory_status, process_usage, system_times};

use crate::{PATIENCE, eventually, spawn_cmd, test_job};

/// A spinning child accumulates CPU time and has resident private memory; a
/// stale start time and an exited child read as `None`.
#[test]
fn busy_child_uses_cpu_and_memory() -> io::Result<()> {
    let job = test_job()?;
    let child = spawn_cmd("for /l %i in (0,0,1) do @rem", &job)?;
    let (pid, start_time) = (child.pid(), child.start_time());
    let first = process_usage(pid, start_time)?.ok_or_else(|| io::Error::other("unread"))?;
    assert!(first.working_set > 0, "{first:?}");
    assert!(
        first.private_working_set > 0 && first.private_working_set <= first.working_set,
        "{first:?}"
    );
    // Half a second of CPU, which a spinning loop reaches.
    let ran = eventually(|| {
        let later = process_usage(pid, start_time)?.ok_or_else(|| io::Error::other("unread"))?;
        Ok(later.cpu_time >= first.cpu_time + 5_000_000)
    })?;
    assert!(ran, "the child used no CPU time");
    assert_eq!(process_usage(pid, start_time + 1)?, None);
    child.terminate(1)?;
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(1));
    assert_eq!(process_usage(pid, start_time)?, None);
    Ok(())
}

/// System counters move forward, kernel time includes idle time, available
/// memory fits in the total, and the low-memory query answers repeatedly.
#[test]
fn system_readings_are_consistent() -> io::Result<()> {
    let first = system_times()?;
    assert!(first.kernel >= first.idle, "{first:?}");
    let later = system_times()?;
    assert!(
        later.idle >= first.idle && later.kernel >= first.kernel && later.user >= first.user,
        "{first:?} {later:?}"
    );
    let memory = memory_status()?;
    assert!(memory.total_physical > 0, "{memory:?}");
    assert!(
        memory.available_physical <= memory.total_physical,
        "{memory:?}"
    );
    low_memory()?;
    low_memory()?;
    Ok(())
}

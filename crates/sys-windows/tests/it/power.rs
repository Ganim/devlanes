use std::io;
use std::time::Duration;

use devlanes_sys_windows::power::{
    self, PowerAccess, PowerHandle, PowerThrottling, PriorityClass, Resumed,
};

use crate::{PATIENCE, long_child, spawn_cmd, test_job};

/// `PROCESS_POWER_THROTTLING_IGNORE_TIMER_RESOLUTION`, a choice of the
/// process's own that a restore must keep.
const IGNORE_TIMER_RESOLUTION: u32 = 4;

/// Opens a test child for `access`, failing when it is not running.
fn open(pid: u32, start_time: u64, access: PowerAccess) -> io::Result<PowerHandle> {
    PowerHandle::open(pid, start_time, access)?.ok_or_else(|| io::Error::other("child not opened"))
}

/// A child that would end within about a second does not end while
/// suspended, and ends once resumed; suspending twice holds one suspension.
/// Dropping a handle that holds a suspension resumes the process.
#[test]
fn suspended_children_wait_until_resumed() -> io::Result<()> {
    let job = test_job()?;
    let child = spawn_cmd("ping -n 2 127.0.0.1 >NUL", &job)?;
    let (pid, start_time) = (child.pid(), child.start_time());
    assert!(PowerHandle::open(pid, start_time - 1, PowerAccess::Suspend)?.is_none());
    let mut handle = open(pid, start_time, PowerAccess::Suspend)?;
    assert_eq!((handle.pid(), handle.start_time()), (pid, start_time));
    handle.suspend()?;
    handle.suspend()?;
    assert!(handle.suspended());
    assert_eq!(child.wait_timeout(Duration::from_secs(3))?, None);
    handle.resume()?;
    assert!(!handle.suspended());
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(0));
    assert!(handle.exited());

    let child = spawn_cmd("ping -n 2 127.0.0.1 >NUL", &job)?;
    let mut handle = open(child.pid(), child.start_time(), PowerAccess::Suspend)?;
    handle.suspend()?;
    assert_eq!(child.wait_timeout(Duration::from_secs(3))?, None);
    drop(handle);
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(0));
    Ok(())
}

/// Efficiency mode is the idle class plus EcoQoS on top of the process's own
/// throttling choices, and both are put back exactly. A suspend handle cannot
/// change them.
#[test]
fn efficiency_mode_sets_and_restores() -> io::Result<()> {
    let job = test_job()?;
    let child = long_child(&job)?;
    let handle = open(child.pid(), child.start_time(), PowerAccess::Throttle)?;
    let class = handle.priority_class()?;
    assert_ne!(class, PriorityClass::IDLE);
    assert_eq!(handle.power_throttling()?, PowerThrottling::DEFAULT);

    let own = PowerThrottling {
        control_mask: IGNORE_TIMER_RESOLUTION,
        state_mask: IGNORE_TIMER_RESOLUTION,
    };
    handle.set_power_throttling(own)?;
    handle.set_priority_class(PriorityClass::IDLE)?;
    handle.set_power_throttling(own.with_eco_qos())?;
    assert_eq!(handle.priority_class()?, PriorityClass::IDLE);
    let throttled = handle.power_throttling()?;
    assert!(throttled.eco_qos(), "{throttled:?}");
    assert_eq!(throttled, own.with_eco_qos());
    handle.set_priority_class(class)?;
    handle.set_power_throttling(own)?;
    assert_eq!(handle.priority_class()?, class);
    assert_eq!(handle.power_throttling()?, own);
    assert!(!own.eco_qos());

    let suspend_only = open(child.pid(), child.start_time(), PowerAccess::Suspend)?;
    assert!(
        suspend_only
            .set_priority_class(PriorityClass::IDLE)
            .is_err()
    );
    assert_eq!(suspend_only.priority_class()?, class);
    Ok(())
}

/// A verified resume reaches only the recorded process and takes back a
/// suspension a lost handle left.
#[test]
fn verified_resume() -> io::Result<()> {
    let job = test_job()?;
    let child = spawn_cmd("ping -n 2 127.0.0.1 >NUL", &job)?;
    let (pid, start_time) = (child.pid(), child.start_time());
    let mut handle = open(pid, start_time, PowerAccess::Suspend)?;
    handle.suspend()?;
    // Leaks the suspension the way a crashed daemon would.
    std::mem::forget(handle);
    assert_eq!(child.wait_timeout(Duration::from_secs(3))?, None);
    assert_eq!(
        power::resume_verified(pid, start_time - 1)?,
        Resumed::Mismatch
    );
    assert_eq!(child.wait_timeout(Duration::from_millis(500))?, None);
    assert_eq!(power::resume_verified(pid, start_time)?, Resumed::Sent);
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(0));
    assert_eq!(power::resume_verified(0, 0)?, Resumed::Gone);
    Ok(())
}

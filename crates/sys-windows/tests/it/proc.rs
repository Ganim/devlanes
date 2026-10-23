use std::io;
use std::net::TcpListener;
use std::time::Duration;

use devlanes_sys_windows::proc::{self, Held, Terminated};
use devlanes_sys_windows::window;

use crate::{PATIENCE, long_child, test_job};

/// This process is in the snapshot with the identity a lookup gives, and the
/// lean queries agree with it.
#[test]
fn snapshot_and_lookup_agree() -> io::Result<()> {
    let me = std::process::id();
    let listed = proc::processes()?
        .into_iter()
        .find(|process| process.pid == me)
        .ok_or_else(|| io::Error::other("own process not listed"))?;
    let looked_up = proc::process(me)?.ok_or_else(|| io::Error::other("own lookup missing"))?;
    assert_eq!(listed, looked_up);
    assert!(!looked_up.exited);
    assert!(
        looked_up.name.to_ascii_lowercase().ends_with(".exe"),
        "{looked_up:?}"
    );
    assert_eq!(proc::start_time(me)?, Some(looked_up.start_time));
    assert_eq!(proc::parent_pid(me)?, Some(looked_up.ppid));
    assert!(proc::matches(me, looked_up.start_time)?);
    assert!(!proc::matches(me, looked_up.start_time + 1)?);
    assert_eq!(proc::process(0)?, None);
    assert!(Held::open(0, 0)?.is_none());
    Ok(())
}

/// The boot id is the same on every call during a boot.
#[test]
fn boot_id_is_stable() -> io::Result<()> {
    let first = proc::boot_id()?;
    assert!(!first.is_empty());
    assert_eq!(proc::boot_id()?, first);
    Ok(())
}

/// Listeners on both families are reported with this process's PID and
/// their port; other PIDs get none of them.
#[test]
fn listeners_of_this_process() -> io::Result<()> {
    let v4 = TcpListener::bind("127.0.0.1:0")?;
    let v6 = TcpListener::bind("[::1]:0")?;
    let me = std::process::id();
    let found = proc::listening_tcp(&[me])?;
    for listener in [&v4, &v6] {
        let addr = listener.local_addr()?;
        assert!(found.contains(&(me, addr)), "{addr} in {found:?}");
    }
    assert!(proc::listening_tcp(&[])?.is_empty());
    Ok(())
}

/// A child is held and terminated only through its recorded start time; a
/// stale one is a mismatch, and an exited child that is still held is
/// reported as exited, then gone.
#[test]
fn verified_termination() -> io::Result<()> {
    let job = test_job()?;
    let child = long_child(&job)?;
    let (pid, start_time) = (child.pid(), child.start_time());
    assert_eq!(proc::start_time(pid)?, Some(start_time));
    let held = Held::open(pid, start_time)?.ok_or_else(|| io::Error::other("child not held"))?;
    assert_eq!((held.pid(), held.start_time()), (pid, start_time));
    assert!(!held.exited());
    assert!(Held::open(pid, start_time - 1)?.is_none());
    assert_eq!(
        proc::terminate_verified(pid, start_time - 1, 1)?,
        Terminated::Mismatch
    );
    assert_eq!(
        proc::terminate_verified(pid, start_time, 9)?,
        Terminated::Sent
    );
    assert!(held.wait(PATIENCE)?, "the child outlived its termination");
    assert_eq!(child.wait_timeout(PATIENCE)?, Some(9));
    let zombie = proc::process(pid)?.ok_or_else(|| io::Error::other("held child missing"))?;
    assert!(zombie.exited, "{zombie:?}");
    assert!(!proc::matches(pid, start_time)?);
    assert!(Held::open(pid, start_time)?.is_none());
    assert_eq!(
        proc::terminate_verified(pid, start_time, 9)?,
        Terminated::Gone
    );
    assert!(proc::wait_exit(pid, Duration::ZERO)?);
    Ok(())
}

/// Waiting on a PID no process has returns at once; this process does not
/// exit within the wait.
#[test]
fn waits_on_processes() -> io::Result<()> {
    assert!(proc::wait_exit(u32::MAX - 3, Duration::ZERO)?);
    assert!(!proc::wait_exit(
        std::process::id(),
        Duration::from_millis(10)
    )?);
    Ok(())
}

/// Closing windows reaches nothing without targets, for a stale identity or
/// for a process without windows.
#[test]
fn close_windows_needs_verified_targets() -> io::Result<()> {
    let me = std::process::id();
    let start_time = proc::start_time(me)?.ok_or_else(|| io::Error::other("no start time"))?;
    assert_eq!(window::close_windows(&[])?, 0);
    assert_eq!(window::close_windows(&[(me, start_time + 1)])?, 0);
    assert_eq!(window::close_windows(&[(me, start_time)])?, 0);
    Ok(())
}

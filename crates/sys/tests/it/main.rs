//! Live process-table checks against this test process and short-lived children.
//!
//! Unix only for now: they signal children and inspect PTYs and pipes.
#![cfg(unix)]

use std::{
    net::TcpListener,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

use devlanes_sys::{ProcessIdentity, Signal, SignalOutcome};

/// Kills and reaps a test child even when an assertion fails first.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns `sleep` with a bounded lifetime in case cleanup is skipped.
fn sleeper() -> Reaped {
    Reaped(
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn sleep"),
    )
}

/// Polls `condition` for up to five seconds.
fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if condition() {
            return true;
        }
        sleep(Duration::from_millis(20));
    }
    false
}

/// The table contains this process with the same identity a direct lookup returns.
#[test]
fn enumeration_includes_self() -> Result<(), devlanes_sys::Error> {
    let pid = std::process::id();
    let me = devlanes_sys::process(pid)?.expect("own process");
    assert!(!me.zombie);
    assert!(me.sid.is_some());
    let listed = devlanes_sys::processes()?;
    let entry = listed.iter().find(|p| p.pid == pid).expect("listed");
    assert_eq!(entry.start_time, me.start_time);
    assert_eq!(devlanes_sys::start_time(pid)?, Some(me.start_time));
    assert_eq!(devlanes_sys::process(0)?, None);
    Ok(())
}

/// The boot ID is a nonempty token that stays the same while the machine runs.
#[test]
fn boot_id_is_stable() -> Result<(), devlanes_sys::Error> {
    let first = devlanes_sys::boot_id()?;
    assert!(!first.trim().is_empty());
    assert_eq!(first.trim(), first);
    assert_eq!(devlanes_sys::boot_id()?, first);
    Ok(())
}

/// A wrong start time is a mismatch and must not deliver the signal.
#[test]
fn verified_signals_reject_recycled_identity() -> Result<(), devlanes_sys::Error> {
    let child = sleeper();
    let pid = child.0.id();
    let real = devlanes_sys::identity(pid)?.expect("child identity");
    let forged = ProcessIdentity {
        start_time: real.start_time + 1,
        ..real
    };
    assert!(devlanes_sys::matches(&real)?);
    assert!(!devlanes_sys::matches(&forged)?);
    assert_eq!(
        devlanes_sys::signal_verified(&forged, Signal::KILL)?,
        SignalOutcome::Mismatch
    );
    assert!(devlanes_sys::matches(&real)?);
    assert_eq!(
        devlanes_sys::signal_verified(&real, Signal::KILL)?,
        SignalOutcome::Sent
    );
    assert!(eventually(|| !devlanes_sys::matches(&real).unwrap_or(true)));
    Ok(())
}

/// An exited, unreaped child still has a readable identity marked as a zombie.
#[test]
fn zombies_keep_identity_until_reaped() -> Result<(), devlanes_sys::Error> {
    let mut child = Reaped(Command::new("true").spawn().expect("spawn true"));
    let pid = child.0.id();
    assert!(eventually(|| {
        devlanes_sys::process(pid)
            .ok()
            .flatten()
            .is_some_and(|p| p.zombie)
    }));
    let identity = devlanes_sys::identity(pid)?.expect("zombie identity");
    assert!(!devlanes_sys::matches(&identity)?);
    assert_eq!(
        devlanes_sys::signal_verified(&identity, Signal::TERM)?,
        SignalOutcome::Gone
    );
    child.0.wait().expect("reap");
    assert!(eventually(|| devlanes_sys::process(pid)
        .ok()
        .flatten()
        .is_none_or(|p| p.start_time != identity.start_time)));
    Ok(())
}

/// Listener diagnostics find a socket bound by the queried pid.
#[test]
fn listening_ports_of_pid_set() -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let sockets = devlanes_sys::listening_tcp(&[std::process::id()])?;
    assert!(
        sockets
            .iter()
            .any(|s| s.addr == addr && s.pid == std::process::id()),
        "{sockets:?} lacks {addr}"
    );
    assert!(devlanes_sys::listening_tcp(&[])?.is_empty());
    Ok(())
}

/// A child holding a pipe's write end or a PTY slave is found by its open file;
/// unrelated processes are not.
#[test]
fn holders_of_pipes_and_terminals() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsFd;

    // Started before the pipe and terminal exist. A child spawned while they are
    // open can still hold them for a moment after `spawn` returns: on Linux the
    // vfork-style spawn resumes the parent before the kernel closes close-on-exec
    // descriptors, and `holders` then rightly reports that child too.
    let bystander = sleeper();

    let (reader, writer) = std::io::pipe()?;
    let master =
        rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)?;
    // So the holder does not keep the master after exec.
    rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NOCTTY | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?;
    let pipe = devlanes_sys::open_file(reader.as_fd())?;
    let tty = devlanes_sys::open_file(slave.as_fd())?;
    assert!(matches!(pipe, devlanes_sys::OpenFile::Pipe(_)));
    assert!(matches!(tty, devlanes_sys::OpenFile::Device(_)));

    let holder = Reaped(
        Command::new("sleep")
            .arg("30")
            .stdin(Stdio::from(std::fs::File::from(slave)))
            .stdout(writer)
            .spawn()?,
    );
    let pids = [holder.0.id(), bystander.0.id()];
    assert_eq!(devlanes_sys::holders(pipe, &pids)?, [holder.0.id()]);
    assert_eq!(devlanes_sys::holders(tty, &pids)?, [holder.0.id()]);
    Ok(())
}

/// Raising records the original limit for children and never lowers the soft limit.
#[test]
fn nofile_raise_records_original() -> Result<(), devlanes_sys::Error> {
    let raised = devlanes_sys::raise_nofile_limit()?;
    let original = devlanes_sys::child_nofile_limit().expect("original recorded");
    assert_eq!(original.hard, raised.hard);
    assert!(match (original.soft, raised.soft) {
        (Some(before), Some(after)) => after >= before,
        (_, None) => true,
        (None, Some(_)) => false,
    });
    Ok(())
}

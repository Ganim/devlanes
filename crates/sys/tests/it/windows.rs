//! Windows versions of the Unix-only checks: verified termination and resume
//! instead of signals, exited-but-held processes as zombies, a spawned
//! listener, and descendant tracking by parent links alone.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

use devlanes_sys::{DescendantTracker, ProcessIdentity, SignalOutcome};

use crate::eventually;

/// How long a test waits for a child's output before failing.
const PATIENCE: Duration = Duration::from_secs(20);

/// Kills a test child and waits for it, even when an assertion fails first.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Terminates the recorded processes on drop, for descendants the test did not
/// spawn itself. Identities are verified, so a reused PID is never reached.
#[derive(Default)]
struct Terminate(Vec<ProcessIdentity>);

impl Drop for Terminate {
    fn drop(&mut self) {
        for identity in &self.0 {
            let _ = devlanes_sys::terminate_verified(identity, 1);
        }
    }
}

/// A program from System32, so `PATH` does not matter.
fn system32(program: &str) -> PathBuf {
    let root = std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(root).join("System32").join(program)
}

/// `ping` for about 30 seconds: a console program that ignores its input and
/// outlives every test, in case cleanup is skipped.
fn sleeper() -> Reaped {
    Reaped(
        Command::new(system32("PING.EXE"))
            .args(["-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn ping"),
    )
}

/// Every identity carries session 0 on Windows.
#[test]
fn identities_have_no_session() -> Result<(), devlanes_sys::Error> {
    let me = devlanes_sys::identity(std::process::id())?.expect("own identity");
    assert_eq!(me.sid, 0);
    assert!(devlanes_sys::matches(&me)?);
    Ok(())
}

/// A wrong start time is a mismatch and must not terminate the process; the
/// right one does.
#[test]
fn verified_termination_rejects_recycled_identity() -> Result<(), devlanes_sys::Error> {
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
        devlanes_sys::terminate_verified(&forged, 1)?,
        SignalOutcome::Mismatch
    );
    assert!(devlanes_sys::matches(&real)?);
    assert_eq!(
        devlanes_sys::terminate_verified(&real, 1)?,
        SignalOutcome::Sent
    );
    assert!(eventually(|| !devlanes_sys::matches(&real).unwrap_or(true)));
    Ok(())
}

/// A child that exited while its `Child` still holds the handle keeps a
/// readable identity marked as a zombie; once the handle is closed it is gone.
#[test]
fn exited_processes_keep_identity_while_held() -> Result<(), devlanes_sys::Error> {
    let child = Reaped(
        Command::new(system32("cmd.exe"))
            .args(["/d", "/c", "exit 3"])
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn cmd"),
    );
    let pid = child.0.id();
    assert!(eventually(|| {
        devlanes_sys::process(pid)
            .ok()
            .flatten()
            .is_some_and(|p| p.zombie)
    }));
    let identity = devlanes_sys::identity(pid)?.expect("exited identity");
    assert!(!devlanes_sys::matches(&identity)?);
    assert_eq!(
        devlanes_sys::terminate_verified(&identity, 1)?,
        SignalOutcome::Gone
    );
    // Closing the last handle lets Windows free the process and its PID.
    drop(child);
    assert!(eventually(|| devlanes_sys::process(pid)
        .ok()
        .flatten()
        .is_none_or(|p| p.start_time != identity.start_time)));
    Ok(())
}

/// Listener diagnostics find a socket bound by a spawned process.
#[test]
fn listening_ports_of_a_child() -> Result<(), Box<dyn std::error::Error>> {
    // Windows PowerShell ships with every supported Windows; it prints the port
    // once the listener is up.
    let script = "$l = New-Object System.Net.Sockets.TcpListener([System.Net.IPAddress]::Loopback, 0); \
                  $l.Start(); [Console]::Out.WriteLine($l.LocalEndpoint.Port); [Console]::Out.Flush(); \
                  Start-Sleep -Seconds 30";
    let mut child = Reaped(
        Command::new(system32(r"WindowsPowerShell\v1.0\powershell.exe"))
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                script,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let _ = BufReader::new(stdout).read_line(&mut line);
        let _ = sender.send(line);
    });
    let port: u16 = receiver.recv_timeout(PATIENCE)?.trim().parse()?;
    let pid = child.0.id();
    let sockets = devlanes_sys::listening_tcp(&[pid])?;
    assert!(
        sockets
            .iter()
            .any(|s| s.pid == pid && s.addr.port() == port && s.addr.ip().is_loopback()),
        "{sockets:?} lacks port {port}"
    );
    assert!(
        !devlanes_sys::listening_tcp(&[std::process::id()])?
            .iter()
            .any(|s| s.addr.port() == port)
    );
    Ok(())
}

/// Without sessions the tracker follows parent links: a grandchild started by
/// `cmd.exe` joins once a snapshot shows it under the tracked root.
#[test]
fn tracker_follows_parent_links() -> Result<(), devlanes_sys::Error> {
    let root = Reaped(
        Command::new(system32("cmd.exe"))
            .args(["/d", "/c", "PING.EXE -n 30 127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .spawn()
            .expect("spawn cmd"),
    );
    let identity = devlanes_sys::identity(root.0.id())?.expect("root identity");
    assert_eq!(identity.sid, 0);
    let mut cleanup = Terminate::default();
    let mut tracker = DescendantTracker::new(identity);
    assert!(eventually(|| {
        tracker.update(&devlanes_sys::processes().unwrap_or_default());
        cleanup.0 = tracker.live().map(|p| p.identity()).collect();
        tracker
            .live()
            .any(|p| p.ppid == identity.pid && p.name.eq_ignore_ascii_case("PING.EXE"))
    }));
    assert!(tracker.live().any(|p| p.pid == identity.pid));
    Ok(())
}

/// A verified resume reaches only the recorded process; one that is not
/// suspended keeps running.
#[test]
fn verified_resume_rejects_recycled_identity() -> Result<(), devlanes_sys::Error> {
    let child = sleeper();
    let real = devlanes_sys::identity(child.0.id())?.expect("child identity");
    let forged = ProcessIdentity {
        start_time: real.start_time + 1,
        ..real
    };
    assert_eq!(
        devlanes_sys::resume_verified(&forged)?,
        SignalOutcome::Mismatch
    );
    assert_eq!(devlanes_sys::resume_verified(&real)?, SignalOutcome::Sent);
    assert!(devlanes_sys::matches(&real)?);
    Ok(())
}

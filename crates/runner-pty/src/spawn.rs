//! Blocking fork/exec of unit leaders, including the `unsafe` pre-exec hook.

use std::{
    io,
    os::{
        fd::{AsFd, OwnedFd},
        unix::process::CommandExt,
    },
    process::{Command, Stdio},
    sync::{Mutex, PoisonError},
};

use devlanes_core::{Error, LaunchSpec, OutputStream, ProcessIdentity, TerminalSize};
use devlanes_sys::NofileLimit;
use rustix::{
    fs::{Mode, OFlags},
    io::FdFlags,
    process::{Resource, Rlimit, WaitOptions},
    pty::OpenptFlags,
    termios::Winsize,
};

use crate::StderrMode;

/// Serializes PTY/pipe creation with spawning. macOS cannot create the master or
/// `std::io::pipe` ends atomically close-on-exec, so a concurrent unit spawn could
/// otherwise fork in that window and leak another unit's descriptors into its child.
///
/// Helper commands (`tokio::process`, git, docker) do not take this lock, so on macOS
/// one of them forking inside that window inherits the fresh descriptor until it
/// execs or exits. The window is a single `fcntl`, a leaked copy only delays the
/// unit's EOF until the helper ends, and routing every helper through this lock
/// would serialize all process spawns in the daemon; it is accepted as is.
static SPAWN_LOCK: Mutex<()> = Mutex::new(());

/// A started leader and the parent's ends of its output channels.
pub(crate) struct Launched {
    pub identity: ProcessIdentity,
    pub master: Option<OwnedFd>,
    pub pipes: Vec<(OutputStream, OwnedFd)>,
    /// The slave or pipe write ends given to the child, for finding processes that
    /// still hold them after the unit ended.
    pub held: Vec<devlanes_sys::OpenFile>,
}

/// Forks and execs the leader, then records its identity before anything can reap it.
///
/// Blocking: call from `spawn_blocking`. The parent's slave and pipe write ends are
/// closed before returning so readers see EOF once the unit's processes exit.
pub(crate) fn launch(
    spec: LaunchSpec,
    stderr: StderrMode,
    nofile: Option<NofileLimit>,
) -> Result<Launched, Error> {
    if spec.program.is_empty() {
        return Err(Error::InvalidInput("program must not be empty".into()));
    }
    let mut command = Command::new(&spec.program);
    command
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(&spec.env);
    let nofile = nofile.map(|limit| Rlimit {
        current: limit.soft,
        maximum: limit.hard,
    });

    let guard = SPAWN_LOCK.lock().unwrap_or_else(PoisonError::into_inner);
    let mut held = Vec::new();
    let (master, mut pipes) = if spec.pty {
        let (master, slave) = open_pty(spec.size)?;
        held.extend(devlanes_sys::open_file(slave.as_fd()).ok());
        command
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        (Some(master), Vec::new())
    } else {
        command.stdin(Stdio::null());
        match stderr {
            StderrMode::Merged => {
                let (reader, writer) = io::pipe()?;
                command.stdout(writer.try_clone()?).stderr(writer);
                (None, vec![(OutputStream::Stdout, OwnedFd::from(reader))])
            }
            StderrMode::Separate => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
                (None, Vec::new())
            }
        }
    };
    install_pre_exec(&mut command, spec.pty, nofile);
    let mut child = command.spawn()?;
    // Dropping the command closes the parent's copies of the slave and write ends.
    drop(command);
    drop(guard);

    if let Some(stdout) = child.stdout.take() {
        pipes.push((OutputStream::Stdout, stdout.into()));
    }
    if let Some(stderr) = child.stderr.take() {
        pipes.push((OutputStream::Stderr, stderr.into()));
    }
    // A pipe's read end identifies the pipe; holders of the write end match it.
    held.extend(
        pipes
            .iter()
            .filter_map(|(_, fd)| devlanes_sys::open_file(fd.as_fd()).ok()),
    );
    let pid = child.id();
    // The std handle is dropped without waiting: the supervisor reaps by PID instead.
    drop(child);

    match devlanes_sys::process(pid) {
        Ok(Some(info)) => Ok(Launched {
            identity: ProcessIdentity {
                pid,
                // setsid ran before exec, so the leader's session is its own PID.
                sid: info.sid.unwrap_or(pid),
                start_time: info.start_time,
            },
            master,
            pipes,
            held,
        }),
        failure => {
            // Without an identity the unit cannot be verified later; do not leave it running.
            abandon(pid);
            Err(match failure {
                Err(error) => error.into(),
                _ => Error::Integration(format!("spawned pid {pid} vanished before inspection")),
            })
        }
    }
}

/// Kills the session's process group and reaps a leader that cannot be supervised.
///
/// Blocking. Only for leaders this process spawned and has not reaped, so the PID
/// and group ID cannot have been recycled.
pub(crate) fn abandon(pid: u32) {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    let _ = rustix::process::waitpid(Some(pid), WaitOptions::empty());
}

/// Opens a master/slave pair sized for the first frame; both ends are close-on-exec.
pub(crate) fn open_pty(size: TerminalSize) -> io::Result<(OwnedFd, OwnedFd)> {
    #[cfg(target_os = "linux")]
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY;
    let master = rustix::pty::openpt(flags)?;
    // Required on macOS, where posix_openpt ignores O_CLOEXEC; harmless on Linux.
    rustix::io::fcntl_setfd(&master, FdFlags::CLOEXEC)?;
    rustix::pty::grantpt(&master)?;
    rustix::pty::unlockpt(&master)?;
    let name = rustix::pty::ptsname(&master, Vec::new())?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    rustix::termios::tcsetwinsize(&master, winsize(size))?;
    Ok((master, slave))
}

/// Converts cell dimensions; pixel sizes are unknown to a headless daemon.
pub(crate) fn winsize(size: TerminalSize) -> Winsize {
    Winsize {
        ws_row: size.rows,
        ws_col: size.columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

/// Makes the child a session leader, attaches the PTY as its controlling terminal and
/// restores the daemon's original open-file limit.
///
/// Never combined with `process_group`: setpgid would make the child a group leader,
/// and a group leader cannot call setsid.
fn install_pre_exec(command: &mut Command, controlling_tty: bool, nofile: Option<Rlimit>) {
    let hook = move || -> io::Result<()> {
        rustix::process::setsid()?;
        if controlling_tty {
            // std has already installed the slave as fd 0.
            rustix::process::ioctl_tiocsctty(rustix::stdio::stdin())?;
        }
        if let Some(limit) = nofile {
            rustix::process::setrlimit(Resource::Nofile, limit)?;
        }
        Ok(())
    };
    // SAFETY: the hook runs in the forked child before exec, where only
    // async-signal-safe operations are allowed. It issues only the setsid, ioctl and
    // setrlimit system calls, captures a Copy value, converts errors without
    // allocating, and touches no locks or state shared with other threads.
    unsafe {
        command.pre_exec(hook);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both PTY ends are close-on-exec so other children never inherit them.
    #[test]
    fn pty_ends_are_cloexec() -> io::Result<()> {
        let (master, slave) = open_pty(TerminalSize::default())?;
        assert!(rustix::io::fcntl_getfd(&master)?.contains(FdFlags::CLOEXEC));
        assert!(rustix::io::fcntl_getfd(&slave)?.contains(FdFlags::CLOEXEC));
        let size = rustix::termios::tcgetwinsize(&slave)?;
        assert_eq!((size.ws_col, size.ws_row), (120, 40));
        Ok(())
    }
}

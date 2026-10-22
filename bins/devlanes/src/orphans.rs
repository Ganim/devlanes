//! How the daemon stops processes a previous daemon left running (orphans).
//!
//! Unix sends SIGTERM, then SIGKILL after the grace period
//! ([`SysProcesses`]). Windows has no signals, so an orphan is first asked the
//! way a graceful stop asks a unit: Ctrl-Break to its console, through the
//! console-control helper, and `WM_CLOSE` to its visible windows. What still
//! runs after the grace period is terminated. The helper lives in
//! `runner-windows`, which the daemon crate cannot use, so this wrapper lives
//! in the composition root.

use std::sync::Arc;

use devlanes_daemon::integrations::{ProcessTable, SysProcesses};

/// The process table the daemon reconciles and stops orphans with.
#[cfg(unix)]
pub fn process_table() -> Arc<dyn ProcessTable> {
    Arc::new(SysProcesses)
}

/// The process table the daemon reconciles and stops orphans with: asks first
/// when this binary can be the console-control helper, else terminates at once.
#[cfg(windows)]
pub fn process_table() -> Arc<dyn ProcessTable> {
    match devlanes_runner_windows::ConsoleHelper::current_exe() {
        Ok(helper) => Arc::new(windows::AskFirst { helper }),
        Err(error) => {
            tracing::warn!(%error, "no console helper; orphans are terminated without a graceful stop");
            Arc::new(SysProcesses)
        }
    }
}

#[cfg(windows)]
mod windows {
    use std::time::Duration;

    use async_trait::async_trait;
    use devlanes_core::ProcessIdentity;
    use devlanes_daemon::integrations::{ProcessTable, SysProcesses};
    use devlanes_runner_windows::ConsoleHelper;

    /// How often a waiting stop checks whether the orphan is gone.
    const POLL: Duration = Duration::from_millis(100);

    /// [`SysProcesses`] with a graceful phase before termination.
    pub(super) struct AskFirst {
        pub(super) helper: ConsoleHelper,
    }

    #[async_trait]
    impl ProcessTable for AskFirst {
        /// Compares PID and start time.
        async fn alive(&self, identity: ProcessIdentity) -> bool {
            SysProcesses.alive(identity).await
        }

        /// Asks the orphan to stop, waits up to `grace` for its identity to stop
        /// verifying, then terminates what is left of it and its descendants.
        ///
        /// The tree is followed from the ask until the end of the grace period,
        /// so descendants that ignore the request are still ended after the
        /// root exited, when a fresh look from the root would find none.
        async fn terminate(&self, identity: ProcessIdentity, grace: Duration) {
            let helper = self.helper.clone();
            let asked = tokio::task::spawn_blocking(move || ask_to_stop(&helper, &identity))
                .await
                .ok();
            if let Some(Asked {
                sent: true,
                mut tree,
            }) = asked
            {
                let deadline = tokio::time::Instant::now() + grace;
                while tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(POLL).await;
                    let mut next = tree.clone();
                    let followed = tokio::task::spawn_blocking(move || {
                        // A failed snapshot keeps the last known tree.
                        if let Ok(snapshot) = devlanes_sys::processes() {
                            next.update(&snapshot);
                        }
                        next
                    })
                    .await;
                    let Ok(followed) = followed else {
                        break;
                    };
                    tree = followed;
                    // As on Unix, the wait ends with the root; what it leaves
                    // behind is terminated below.
                    let root_runs = tree.live().any(|process| {
                        process.pid == identity.pid && process.start_time == identity.start_time
                    });
                    if !root_runs {
                        break;
                    }
                }
                let _ = tokio::task::spawn_blocking(move || terminate_tree(&tree)).await;
            }
            SysProcesses.terminate(identity, grace).await;
        }
    }

    /// What [`ask_to_stop`] did: whether a request went out, and the tree it
    /// asked (the root and its descendants by parent), empty when the root no
    /// longer verified.
    pub(super) struct Asked {
        pub(super) sent: bool,
        pub(super) tree: devlanes_sys::DescendantTracker,
    }

    /// Sends Ctrl-Break to the console `identity` is attached to and `WM_CLOSE`
    /// to the visible windows of it and its descendants, while its identity still
    /// verifies. Blocking.
    ///
    /// The root is held open from the check until the helper returns, so the
    /// helper, which attaches by bare PID, cannot reach a process that took the
    /// PID over in between.
    ///
    /// Each recorded member of an orphan arrives on its own, so members sharing a
    /// console each send the event; a process that exits on the first ignores
    /// the rest.
    pub(super) fn ask_to_stop(helper: &ConsoleHelper, identity: &ProcessIdentity) -> Asked {
        let mut tree = devlanes_sys::DescendantTracker::new(*identity);
        let not_sent = |tree| Asked { sent: false, tree };
        let Ok(Some(held)) = devlanes_sys::windows::proc::Held::open(identity) else {
            return not_sent(tree);
        };
        let Ok(snapshot) = devlanes_sys::processes() else {
            return not_sent(tree);
        };
        // The held root cannot be reused, so children it started just before
        // exiting are still its own.
        tree.hold_root(true);
        tree.update(&snapshot);
        let pids: Vec<u32> = tree.live().map(|process| process.pid).collect();
        let signalled = helper.ask_to_stop(identity.pid);
        drop(held);
        tree.hold_root(false);
        let closed = devlanes_sys::windows::window::close_windows(&pids);
        Asked {
            sent: signalled || closed > 0,
            tree,
        }
    }

    /// Terminates every process of `tree` whose identity still verifies.
    /// Blocking.
    fn terminate_tree(tree: &devlanes_sys::DescendantTracker) {
        for process in tree.live() {
            let member = process.identity();
            if let Err(error) = devlanes_sys::windows::proc::terminate_verified(&member, 1) {
                tracing::warn!(%error, pid = member.pid, "terminating an orphan failed");
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::Instant;

        use super::*;

        /// Set in the environment of [`sleeper`] runs.
        const SLEEP: &str = "DEVLANES_TEST_ORPHAN_SLEEP";
        /// `STATUS_CONTROL_C_EXIT`: how a process the console event ended exits.
        const ENDED_BY_EVENT: i32 = 0xC000_013A_u32.cast_signed();

        /// Not a test of its own: this binary as the console-control helper, when
        /// started with a request. Without one it does nothing.
        #[test]
        fn console_ctrl_helper() {
            if std::env::var_os(devlanes_runner_windows::ctrl::REQUEST_VAR).is_some() {
                std::process::exit(i32::from(devlanes_runner_windows::console_ctrl_main()));
            }
        }

        /// Not a test of its own: sleeps when [`SLEEP`] is set, with Windows'
        /// default console handling, which ends the process on Ctrl-Break. It
        /// prints [`SLEEPER_READY`] first. With [`SPAWN_IGNORER`] also set, it
        /// then starts [`ignorer`] on its console.
        #[test]
        fn sleeper() -> std::io::Result<()> {
            if std::env::var_os(SLEEP).is_none() {
                return Ok(());
            }
            println!("{SLEEPER_READY}");
            if std::env::var_os(SPAWN_IGNORER).is_some() {
                // Shares this console and standard output, where it reports
                // when its handler is in place.
                this_test("orphans::windows::tests::ignorer")?
                    .env_remove(SLEEP)
                    .env_remove(SPAWN_IGNORER)
                    .env(IGNORE, "1")
                    .stdin(Stdio::null())
                    .spawn()?;
            }
            std::thread::sleep(Duration::from_secs(60));
            Ok(())
        }

        /// Not a test of its own: when [`IGNORE`] is set, ignores Ctrl-Break
        /// (a handler that returns TRUE), says so on standard output and sleeps.
        #[test]
        fn ignorer() -> std::io::Result<()> {
            if std::env::var_os(IGNORE).is_none() {
                return Ok(());
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            let _ignored = runtime.block_on(async { tokio::signal::windows::ctrl_break() })?;
            println!("{IGNORER_READY}");
            std::thread::sleep(Duration::from_secs(60));
            Ok(())
        }

        /// Set in the environment of [`sleeper`] runs that start an [`ignorer`].
        const SPAWN_IGNORER: &str = "DEVLANES_TEST_ORPHAN_SPAWN_IGNORER";
        /// Set in the environment of [`ignorer`] runs.
        const IGNORE: &str = "DEVLANES_TEST_ORPHAN_IGNORE";
        /// The line an [`ignorer`] prints once Ctrl-Break no longer ends it.
        const IGNORER_READY: &str = "ignorer-ready";
        /// The line a [`sleeper`] prints once it runs. Its console is set up by
        /// then: Windows creates it while the process starts, before `main`.
        const SLEEPER_READY: &str = "sleeper-ready";
        /// Longest a test waits for a process it started to change state.
        const WAIT: Duration = Duration::from_secs(20);

        /// This test binary, running only the test `name`.
        fn this_test(name: &str) -> std::io::Result<Command> {
            let mut command = Command::new(std::env::current_exe()?);
            command.args(["--exact", name, "--nocapture", "--test-threads=1"]);
            Ok(command)
        }

        /// This binary as the console-control helper.
        fn helper() -> std::io::Result<ConsoleHelper> {
            Ok(ConsoleHelper {
                program: std::env::current_exe()?,
                args: [
                    "--exact",
                    "orphans::windows::tests::console_ctrl_helper",
                    "--nocapture",
                    "--test-threads=1",
                ]
                .map(Into::into)
                .into(),
            })
        }

        /// A started test process, killed with its descendants when dropped so a
        /// failing test leaves nothing running.
        struct Started(std::process::Child);

        impl Drop for Started {
            fn drop(&mut self) {
                // The unwaited child's handle keeps its PID from being reused.
                if let (Ok(Some(info)), Ok(snapshot)) = (
                    devlanes_sys::process(self.0.id()),
                    devlanes_sys::processes(),
                ) {
                    let mut tree = devlanes_sys::DescendantTracker::new(info.identity());
                    tree.hold_root(true);
                    tree.update(&snapshot);
                    terminate_tree(&tree);
                }
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        /// Processes known by identity, terminated (if still theirs) when
        /// dropped.
        struct Owned(Vec<ProcessIdentity>);

        impl Drop for Owned {
            fn drop(&mut self) {
                for identity in &self.0 {
                    let _ = devlanes_sys::windows::proc::terminate_verified(identity, 1);
                }
            }
        }

        /// The exit status of `child` once it exits within [`WAIT`].
        async fn exit_of(child: &mut Started) -> std::io::Result<std::process::ExitStatus> {
            let deadline = Instant::now() + WAIT;
            loop {
                if let Some(status) = child.0.try_wait()? {
                    return Ok(status);
                }
                if Instant::now() >= deadline {
                    return Err(std::io::Error::other("the process did not exit in time"));
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }

        /// The identity of process `pid`, which must be running.
        async fn identity_of(pid: u32) -> std::io::Result<ProcessIdentity> {
            tokio::task::spawn_blocking(move || devlanes_sys::identity(pid))
                .await
                .map_err(std::io::Error::other)?
                .map_err(std::io::Error::other)?
                .ok_or_else(|| std::io::Error::other(format!("process {pid} vanished")))
        }

        /// Starts a [`sleeper`] with a console of its own and no window, as pipe
        /// units have, and waits until standard output, shared with an
        /// [`ignorer`] it starts, shows `ready`.
        ///
        /// Asking right after the start can find no console yet (the helper
        /// exits with `NoConsole`), and orphans are asked without waiting for
        /// one, so the tests wait until the process runs.
        async fn start_sleeper(
            spawn_ignorer: bool,
            ready: &'static str,
        ) -> std::io::Result<Started> {
            use std::io::BufRead;
            let mut command = this_test("orphans::windows::tests::sleeper")?;
            command
                .env(SLEEP, "1")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .creation_flags(devlanes_sys::windows::process::CREATE_NO_WINDOW);
            if spawn_ignorer {
                command.env(SPAWN_IGNORER, "1");
            }
            let mut child = Started(command.spawn()?);
            let stdout = child
                .0
                .stdout
                .take()
                .ok_or_else(|| std::io::Error::other("no pipe"))?;
            // The read ends at the latest when the guards end the processes.
            let seen = tokio::task::spawn_blocking(move || {
                std::io::BufReader::new(stdout)
                    .lines()
                    // libtest may put the test's name before it on the line.
                    .any(|line| line.is_ok_and(|line| line.trim_end().ends_with(ready)))
            });
            let seen = tokio::time::timeout(WAIT, seen)
                .await
                .map_err(|_| std::io::Error::other(format!("no {ready} in time")))?
                .map_err(std::io::Error::other)?;
            if !seen {
                return Err(std::io::Error::other(format!("exited before {ready}")));
            }
            Ok(child)
        }

        /// An orphan with a console of its own, as pipe units have, ends on the
        /// Ctrl-Break within the grace period instead of being terminated; a
        /// process that no longer verifies is not asked.
        #[tokio::test]
        async fn orphans_are_asked_before_they_are_terminated() -> std::io::Result<()> {
            let mut child = start_sleeper(false, SLEEPER_READY).await?;
            let identity = identity_of(child.0.id()).await?;
            let table = AskFirst { helper: helper()? };
            let begun = Instant::now();
            table.terminate(identity, Duration::from_secs(10)).await;
            let status = exit_of(&mut child).await?;
            assert_eq!(status.code(), Some(ENDED_BY_EVENT), "{status:?}");
            assert!(
                begun.elapsed() < Duration::from_secs(10),
                "waited the full grace"
            );
            assert!(!table.alive(identity).await);
            let helper = table.helper.clone();
            let asked = tokio::task::spawn_blocking(move || ask_to_stop(&helper, &identity))
                .await
                .map_err(std::io::Error::other)?;
            assert!(!asked.sent, "a gone process was asked to stop");
            assert_eq!(asked.tree.processes().count(), 0);
            Ok(())
        }

        /// A descendant that ignores Ctrl-Break, which no recorded identity
        /// names, is still terminated after the root ended on the event: a
        /// fresh look from the gone root would no longer find it.
        #[tokio::test]
        async fn descendants_that_ignore_the_request_are_terminated() -> std::io::Result<()> {
            let mut child = start_sleeper(true, IGNORER_READY).await?;
            let root = identity_of(child.0.id()).await?;
            let exe = std::env::current_exe()?;
            let exe_name = exe
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default()
                .to_owned();
            // The sleeper's other child is its console host.
            let ignorer = tokio::task::spawn_blocking(move || {
                devlanes_sys::processes().map(|snapshot| {
                    snapshot
                        .iter()
                        .find(|p| {
                            p.ppid == root.pid
                                && p.start_time >= root.start_time
                                && p.name.eq_ignore_ascii_case(&exe_name)
                        })
                        .map(devlanes_sys::ProcessInfo::identity)
                })
            })
            .await
            .map_err(std::io::Error::other)?
            .map_err(std::io::Error::other)?
            .ok_or_else(|| std::io::Error::other("the ignorer is not the sleeper's child"))?;
            let _owned = Owned(vec![ignorer]);
            let table = AskFirst { helper: helper()? };
            table.terminate(root, Duration::from_secs(10)).await;
            let status = exit_of(&mut child).await?;
            assert_eq!(status.code(), Some(ENDED_BY_EVENT), "{status:?}");
            // Termination is asynchronous; it only has to finish soon.
            let deadline = Instant::now() + WAIT;
            while table.alive(ignorer).await && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            assert!(
                !table.alive(ignorer).await,
                "the descendant outlived the stop"
            );
            Ok(())
        }
    }
}

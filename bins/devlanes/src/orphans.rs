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
        async fn terminate(&self, identity: ProcessIdentity, grace: Duration) {
            let helper = self.helper.clone();
            let asked = tokio::task::spawn_blocking(move || ask_to_stop(&helper, &identity))
                .await
                .unwrap_or(false);
            if asked {
                let deadline = tokio::time::Instant::now() + grace;
                while tokio::time::Instant::now() < deadline && self.alive(identity).await {
                    tokio::time::sleep(POLL).await;
                }
            }
            SysProcesses.terminate(identity, grace).await;
        }
    }

    /// Sends Ctrl-Break to the console `identity` is attached to and `WM_CLOSE`
    /// to the visible windows of it and its descendants, while its identity still
    /// verifies; `true` when either went out. Blocking.
    ///
    /// Each recorded member of an orphan arrives on its own, so members sharing a
    /// console each send the event; a process that exits on the first ignores
    /// the rest.
    pub(super) fn ask_to_stop(helper: &ConsoleHelper, identity: &ProcessIdentity) -> bool {
        let Ok(snapshot) = devlanes_sys::processes() else {
            return false;
        };
        // Empty unless the root still verifies, so a reused PID is never asked.
        let mut tree = devlanes_sys::DescendantTracker::new(*identity);
        tree.update(&snapshot);
        let pids: Vec<u32> = tree.live().map(|process| process.pid).collect();
        if !pids.contains(&identity.pid) {
            return false;
        }
        let signalled = helper.ask_to_stop(identity.pid);
        let closed = devlanes_sys::windows::window::close_windows(&pids);
        signalled || closed > 0
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
        /// default console handling, which ends the process on Ctrl-Break.
        #[test]
        fn sleeper() {
            if std::env::var_os(SLEEP).is_some() {
                std::thread::sleep(Duration::from_secs(60));
            }
        }

        /// This test binary, running only the test `name`.
        fn this_test(name: &str) -> std::io::Result<Command> {
            let mut command = Command::new(std::env::current_exe()?);
            command.args(["--exact", name, "--nocapture", "--test-threads=1"]);
            Ok(command)
        }

        /// An orphan with a console of its own, as pipe units have, ends on the
        /// Ctrl-Break within the grace period instead of being terminated; a
        /// process that no longer verifies is not asked.
        #[tokio::test]
        async fn orphans_are_asked_before_they_are_terminated() -> std::io::Result<()> {
            let helper = ConsoleHelper {
                program: std::env::current_exe()?,
                args: [
                    "--exact",
                    "orphans::windows::tests::console_ctrl_helper",
                    "--nocapture",
                    "--test-threads=1",
                ]
                .map(Into::into)
                .into(),
            };
            let mut child = this_test("orphans::windows::tests::sleeper")?
                .env(SLEEP, "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .creation_flags(devlanes_sys::windows::process::CREATE_NO_WINDOW)
                .spawn()?;
            let pid = child.id();
            let identity = tokio::task::spawn_blocking(move || devlanes_sys::identity(pid))
                .await
                .map_err(std::io::Error::other)?
                .map_err(std::io::Error::other)?
                .ok_or_else(|| std::io::Error::other("the sleeper vanished"))?;
            let table = AskFirst { helper };
            let begun = Instant::now();
            table.terminate(identity, Duration::from_secs(10)).await;
            let status = child.wait()?;
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
            assert!(!asked, "a gone process was asked to stop");
            Ok(())
        }
    }
}

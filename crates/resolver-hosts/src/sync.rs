//! Debounced, coalesced hosts updates for the daemon.
//!
//! Route changes come in bursts (a lane start registers every service at once), and
//! each helper run rewrites `/etc/hosts` and flushes the OS caches. The engine calls
//! [`HostsSync::update`] with the full desired set on every change; one task waits
//! for a quiet period and applies only the latest set, skipping it when it equals
//! what was last applied.

use std::collections::BTreeSet;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::client::{HostsClient, HostsError};

/// Timing for [`HostsSync`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncConfig {
    /// Quiet period after the last change before applying.
    pub debounce: Duration,
    /// Longest an update waits under a constant stream of changes.
    pub max_delay: Duration,
    /// Pause after `sudo_denied` or `helper_outdated` before trying sudo again, so a
    /// missing rule does not fill the auth log on every route change.
    pub denied_backoff: Duration,
}

impl Default for SyncConfig {
    /// 250 ms debounce, 2 s cap, 5 min backoff.
    fn default() -> Self {
        Self {
            debounce: Duration::from_millis(250),
            max_delay: Duration::from_secs(2),
            denied_backoff: Duration::from_secs(300),
        }
    }
}

/// Outcome of the most recent attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SyncState {
    /// Nothing requested yet.
    Idle,
    /// The block holds the last requested names.
    Applied {
        /// Names in the block.
        names: usize,
        /// Whether the last run rewrote the file.
        changed: bool,
    },
    /// The last attempt failed; [`HostsError::code`] gives `helper_missing`,
    /// `sudo_denied` and the rest.
    Failed(HostsError),
}

/// What the engine wants; `retry` bumps clear the backoff.
#[derive(Debug, Clone, Default)]
struct Desired {
    names: Option<BTreeSet<String>>,
    retry: u64,
}

/// Background task applying the latest desired names.
#[derive(Debug)]
pub struct HostsSync {
    desired: watch::Sender<Desired>,
    state: watch::Receiver<SyncState>,
    task: JoinHandle<()>,
}

impl HostsSync {
    /// Spawns the task on the current runtime.
    pub fn spawn(client: HostsClient, config: SyncConfig) -> Self {
        let (desired, desired_rx) = watch::channel(Desired::default());
        let (state_tx, state) = watch::channel(SyncState::Idle);
        let task = tokio::spawn(run(client, config, desired_rx, state_tx));
        Self {
            desired,
            state,
            task,
        }
    }

    /// Replaces the desired set; an identical set is ignored.
    pub fn update(&self, names: BTreeSet<String>) {
        self.desired.send_if_modified(|desired| {
            if desired.names.as_ref() == Some(&names) {
                false
            } else {
                desired.names = Some(names);
                true
            }
        });
    }

    /// Forgets the backoff and the last applied set, and reapplies.
    ///
    /// Call after `devlanes certs trust --hosts` installs or repairs the helper.
    pub fn retry(&self) {
        self.desired.send_modify(|desired| desired.retry += 1);
    }

    /// Watches the outcome of each attempt.
    pub fn state(&self) -> watch::Receiver<SyncState> {
        self.state.clone()
    }

    /// Applies any pending change, then stops. Entries stay in `/etc/hosts`.
    pub async fn shutdown(self) {
        drop(self.desired);
        // A panic inside the task was already reported by tokio; nothing to undo.
        let _ = self.task.await;
    }
}

/// The task: wait for changes, coalesce, apply.
async fn run(
    client: HostsClient,
    config: SyncConfig,
    mut desired: watch::Receiver<Desired>,
    state: watch::Sender<SyncState>,
) {
    let mut applied: Option<BTreeSet<String>> = None;
    let mut seen_retry = 0;
    let mut blocked_until: Option<Instant> = None;
    let mut version_checked = false;
    loop {
        let pending = {
            let current = desired.borrow();
            current.names.is_some() && current.names != applied
        };
        let mut open = match blocked_until {
            // A pending set waits out the backoff even if nothing else changes.
            Some(until) if pending => {
                match tokio::time::timeout_at(until, desired.changed()).await {
                    Ok(result) => result.is_ok(),
                    Err(_) => true,
                }
            }
            _ => desired.changed().await.is_ok(),
        };
        if open {
            let hard = Instant::now() + config.max_delay;
            loop {
                let deadline = (Instant::now() + config.debounce).min(hard);
                match tokio::time::timeout_at(deadline, desired.changed()).await {
                    Ok(Ok(())) => {}
                    Ok(Err(_)) => {
                        open = false;
                        break;
                    }
                    Err(_) => break,
                }
            }
        }
        let current = desired.borrow_and_update().clone();
        if current.retry != seen_retry {
            seen_retry = current.retry;
            blocked_until = None;
            applied = None;
            version_checked = false;
        }
        let blocked = blocked_until.is_some_and(|until| Instant::now() < until);
        if let Some(names) = current
            .names
            .filter(|names| !blocked && applied.as_ref() != Some(names))
        {
            match attempt(&client, &names, &mut version_checked).await {
                Ok(next) => {
                    blocked_until = None;
                    applied = Some(names);
                    state.send_replace(next);
                }
                Err(error) => {
                    tracing::warn!(code = error.code(), %error, "updating /etc/hosts failed");
                    if matches!(
                        error,
                        HostsError::SudoDenied { .. }
                            | HostsError::Outdated { .. }
                            | HostsError::Unsupported
                    ) {
                        blocked_until = Some(Instant::now() + config.denied_backoff);
                    }
                    state.send_replace(SyncState::Failed(error));
                }
            }
        }
        if !open {
            break;
        }
    }
}

/// One apply, preceded by a protocol check the first time.
async fn attempt(
    client: &HostsClient,
    names: &BTreeSet<String>,
    version_checked: &mut bool,
) -> Result<SyncState, HostsError> {
    if !*version_checked {
        client.version().await?;
        *version_checked = true;
    }
    let applied = client.apply(names).await?;
    Ok(SyncState::Applied {
        names: applied.names,
        changed: applied.changed,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use devlanes_hosts_helper::{INSTALL_PATH, PROTOCOL};

    use super::*;
    use crate::exec::{CommandOutput, FileKind};
    use crate::fake::{FakeExecutor, FakeFs};

    /// Answers `version` and `apply`; `deny` makes sudo refuse.
    fn executor(deny: Arc<AtomicBool>) -> Arc<FakeExecutor> {
        Arc::new(FakeExecutor::new(move |command| {
            if command.args.iter().any(|arg| arg == "version") {
                return Ok(CommandOutput::ok(format!(
                    "devlanes-hosts 0.1.0 protocol={PROTOCOL} build=release\n"
                )));
            }
            if deny.load(Ordering::SeqCst) {
                return Ok(CommandOutput::failed(1, "sudo: a password is required\n"));
            }
            let count = command
                .stdin
                .as_deref()
                .map_or(0, |stdin| stdin.iter().filter(|&&b| b == b'\n').count());
            Ok(CommandOutput::ok(format!("changed names={count}\n")))
        }))
    }

    fn sync(executor: Arc<FakeExecutor>) -> HostsSync {
        let fs = FakeFs::standard();
        fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
        let client = HostsClient::new(executor, Arc::new(fs), None).expect("client");
        HostsSync::spawn(client, SyncConfig::default())
    }

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|name| (*name).to_owned()).collect()
    }

    /// stdin of every apply, as text.
    fn applies(executor: &FakeExecutor) -> Vec<String> {
        executor
            .calls()
            .into_iter()
            .filter(|command| command.args.iter().any(|arg| arg == "apply"))
            .map(|command| String::from_utf8(command.stdin.unwrap_or_default()).expect("utf-8"))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn bursts_coalesce_into_one_apply() {
        let executor = executor(Arc::default());
        let sync = sync(executor.clone());
        sync.update(names(&["a.localhost"]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        sync.update(names(&["a.localhost", "b.localhost"]));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            applies(&executor).is_empty(),
            "still inside the quiet period"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor), ["a.localhost\nb.localhost\n"]);
        assert_eq!(
            *sync.state().borrow(),
            SyncState::Applied {
                names: 2,
                changed: true
            }
        );

        // The same set again is not reapplied.
        sync.update(names(&["a.localhost", "b.localhost"]));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor).len(), 1);
        sync.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_constant_stream_still_applies_by_the_cap() {
        let executor = executor(Arc::default());
        let sync = sync(executor.clone());
        for i in 0..30 {
            sync.update(names(&[&format!("n{i}.localhost")]));
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // 3 s of changes 100 ms apart: the 2 s cap forces one apply mid-stream.
        assert_eq!(applies(&executor).len(), 1);
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor).len(), 2);
        assert_eq!(
            applies(&executor).last().map(String::as_str),
            Some("n29.localhost\n")
        );
        sync.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn sudo_denied_backs_off_until_retry() {
        let deny = Arc::new(AtomicBool::new(true));
        let executor = executor(deny.clone());
        let sync = sync(executor.clone());
        sync.update(names(&["a.localhost"]));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor).len(), 1);
        let state = sync.state().borrow().clone();
        assert!(
            matches!(&state, SyncState::Failed(error) if error.code() == "sudo_denied"),
            "{state:?}"
        );

        sync.update(names(&["b.localhost"]));
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor).len(), 1, "no sudo during the backoff");

        deny.store(false, Ordering::SeqCst);
        sync.retry();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor), ["a.localhost\n", "b.localhost\n"]);
        assert_eq!(
            *sync.state().borrow(),
            SyncState::Applied {
                names: 1,
                changed: true
            }
        );
        sync.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn backoff_expiry_applies_the_pending_set() {
        let deny = Arc::new(AtomicBool::new(true));
        let executor = executor(deny.clone());
        let sync = sync(executor.clone());
        sync.update(names(&["a.localhost"]));
        tokio::time::sleep(Duration::from_secs(1)).await;
        deny.store(false, Ordering::SeqCst);
        tokio::time::sleep(SyncConfig::default().denied_backoff).await;
        assert_eq!(applies(&executor).len(), 2);
        assert_eq!(
            *sync.state().borrow(),
            SyncState::Applied {
                names: 1,
                changed: true
            }
        );
        sync.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_flushes_a_pending_change() {
        let executor = executor(Arc::default());
        let sync = sync(executor.clone());
        sync.update(names(&["a.localhost"]));
        sync.shutdown().await;
        assert_eq!(applies(&executor), ["a.localhost\n"]);
    }

    #[tokio::test(start_paused = true)]
    async fn version_is_checked_once() {
        let executor = executor(Arc::default());
        let sync = sync(executor.clone());
        for set in [&["a.localhost"][..], &["b.localhost"]] {
            sync.update(names(set));
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let versions = executor
            .argvs()
            .into_iter()
            .filter(|argv| argv.last().map(String::as_str) == Some("version"))
            .count();
        assert_eq!(versions, 1);
        sync.shutdown().await;
    }
}

//! Draws a start's progress in the terminal tab while a command follows it.
//!
//! The start request streams its own lane events (`"events": true`); the
//! command hands each one to [`Follower::event`], which feeds a [`StartTracker`]
//! and writes [`Progress`] to this process's stdout. The bar is resent every
//! second (Ghostty drops a stale one) and cleared on success, after a short red
//! error on failure, on Ctrl-C and on drop. A daemon too old to stream them
//! sends none, and the bar stays indeterminate until the command ends.

use std::{
    io::Write,
    sync::atomic::{AtomicU8, Ordering},
    time::Duration,
};

use devlanes_cli::{ERROR_HOLD, Runtime, start_tracker};
use devlanes_client::Paths;
use devlanes_proto::{Event, Request};
use devlanes_term::{Progress, ProgressBar, ProgressMode, StartTracker, Wrap};
use tokio::{
    sync::{mpsc, oneshot},
    task::JoinHandle,
};

/// The wrap of a bar on screen (0 none, 1 bare, 2 tmux), for the panic hook.
static SHOWN: AtomicU8 = AtomicU8::new(0);
/// The longest `finish` waits for the follower to clear before giving up on it.
const FINISH_WAIT: Duration = Duration::from_secs(3);

/// The `terminal.progress` setting from the user config; auto when unset or unreadable.
pub fn configured_mode(paths: &Paths) -> ProgressMode {
    let config = devlanes_config::load_user(paths.home.join("config.yaml")).ok();
    match config
        .and_then(|config| config.terminal)
        .map(|t| t.progress())
    {
        Some(devlanes_config::TerminalProgress::Always) => ProgressMode::Always,
        Some(devlanes_config::TerminalProgress::Never) => ProgressMode::Never,
        _ => ProgressMode::Auto,
    }
}

/// Whether this invocation draws the bar at all.
pub fn wrap(paths: &Paths, runtime: &Runtime) -> Option<Wrap> {
    runtime.progress_wrap(configured_mode(paths), |key| std::env::var(key).ok())
}

/// A running follower; [`Follower::finish`] ends it and clears the bar.
pub struct Follower {
    events: Option<mpsc::UnboundedSender<Event>>,
    done: Option<oneshot::Sender<bool>>,
    task: Option<JoinHandle<()>>,
}

impl Follower {
    /// Follows `request`'s start when it is one and this invocation draws the
    /// bar; otherwise does nothing. The config is read only for a start.
    pub fn start(paths: &Paths, runtime: &Runtime, request: &Request) -> Self {
        if start_tracker(request).is_none() {
            return Self::idle();
        }
        Self::start_with(wrap(paths, runtime), request)
    }

    /// Like [`Follower::start`] with the bar's [`wrap`] already resolved.
    pub fn start_with(wrap: Option<Wrap>, request: &Request) -> Self {
        let (Some(tracker), Some(wrap)) = (start_tracker(request), wrap) else {
            return Self::idle();
        };
        install_panic_hook();
        let (events, received) = mpsc::unbounded_channel();
        let (done, finished) = oneshot::channel();
        let task = tokio::spawn(follow(tracker, wrap, received, finished));
        Self {
            events: Some(events),
            done: Some(done),
            task: Some(task),
        }
    }

    /// A follower that draws nothing.
    fn idle() -> Self {
        Self {
            events: None,
            done: None,
            task: None,
        }
    }

    /// One event of the followed request; a no-op when nothing is drawn.
    pub fn event(&self, event: &Event) {
        if let Some(events) = &self.events {
            let _ = events.send(event.clone());
        }
    }

    /// Ends following: `ok` clears the bar, a failure shows it red for
    /// [`ERROR_HOLD`] first. Waits until it is cleared, so no bar outlives the command.
    pub async fn finish(mut self, ok: bool) {
        if let Some(done) = self.done.take() {
            let _ = done.send(ok);
        }
        if let Some(mut task) = self.task.take()
            && tokio::time::timeout(FINISH_WAIT, &mut task).await.is_err()
        {
            task.abort();
            clear_now();
        }
    }
}

impl Drop for Follower {
    /// A follower dropped without `finish` (an early return) stops at once;
    /// its bar clears when the task drops it.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Applies events until `finished` fires, keeping the bar current.
async fn follow(
    mut tracker: StartTracker,
    wrap: Wrap,
    mut events: mpsc::UnboundedReceiver<Event>,
    mut finished: oneshot::Receiver<bool>,
) {
    let mut bar = ProgressBar::new(std::io::stdout(), Some(wrap));
    show(&mut bar, tracker.progress(), wrap);
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            result = &mut finished => {
                // Events sent before the end still count: the last failure turns it red.
                while let Ok(event) = events.try_recv() {
                    tracker.event(&event);
                }
                if matches!(result, Ok(false)) {
                    show(&mut bar, failed(tracker.progress()), wrap);
                    tokio::time::sleep(ERROR_HOLD).await;
                }
                break;
            }
            Some(event) = events.recv() => {
                tracker.event(&event);
                show(&mut bar, tracker.progress(), wrap);
            }
            _ = tick.tick() => show(&mut bar, tracker.progress(), wrap),
        }
    }
    let _ = bar.clear();
    SHOWN.store(0, Ordering::Relaxed);
}

/// Writes `progress` and records it for the panic hook.
fn show(bar: &mut ProgressBar<std::io::Stdout>, progress: Progress, wrap: Wrap) {
    if bar.set(progress).is_ok() {
        let shown = match (progress, wrap) {
            (Progress::Clear, _) => 0,
            (_, Wrap::Bare) => 1,
            (_, Wrap::Tmux) => 2,
        };
        SHOWN.store(shown, Ordering::Relaxed);
    }
}

/// The bar a failed start ends with: red at the share it reached.
fn failed(progress: Progress) -> Progress {
    match progress {
        Progress::Percent(p) | Progress::Paused(p) | Progress::Error(p) => Progress::Error(p),
        Progress::Clear | Progress::Indeterminate => Progress::Error(0),
    }
}

/// Writes the clear sequence straight to stdout.
fn clear_now() {
    let wrap = match SHOWN.swap(0, Ordering::Relaxed) {
        1 => Wrap::Bare,
        2 => Wrap::Tmux,
        _ => return,
    };
    let mut stdout = std::io::stdout();
    let _ = stdout.write_all(wrap.apply(&Progress::Clear.sequence()).as_bytes());
    let _ = stdout.flush();
}

/// Clears a shown bar when anything panics, before the default message.
fn install_panic_hook() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            clear_now();
            previous(info);
        }));
    });
}

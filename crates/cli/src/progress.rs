//! Which commands drive the terminal's progress bar, and for which lanes. The
//! encoding, detection and start computation live in `devlanes-term`; the
//! binary follows events and writes the bar.

use devlanes_proto::{Method, Request};
use devlanes_term::{ProgressMode, StartTracker, Wrap, progress_support};
use serde_json::Value;

use crate::{OutputMode, Runtime};

/// How long a failed start's red bar stays before the command exits and clears it.
pub const ERROR_HOLD: std::time::Duration = std::time::Duration::from_secs(1);

/// The tracker for a request that follows a start: `start`, `restart`,
/// `lane resume`, `lane new --start` and `wait`. `None` for everything else.
/// It reads the request's own lane events, which the daemon limits to the
/// request's lanes and starts, so it needs no lane of its own.
pub fn start_tracker(request: &Request) -> Option<StartTracker> {
    let units = || -> Vec<String> {
        request
            .params
            .get("units")
            .and_then(Value::as_array)
            .map(|units| {
                units
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    match request.method {
        Method::Start | Method::Restart => Some(StartTracker::command().with_units(units())),
        Method::Resume => Some(StartTracker::command()),
        Method::LaneNew if request.params.get("start").and_then(Value::as_bool) == Some(true) => {
            Some(StartTracker::command())
        }
        // The daemon streams only the unit a unit target names.
        Method::Wait => request
            .params
            .get("target")
            .and_then(Value::as_str)
            .map(|_| StartTracker::command()),
        _ => None,
    }
}

impl Runtime {
    /// Whether this invocation draws the progress bar: only human output on a
    /// terminal, then as `mode`, [`devlanes_term::PROGRESS_ENV`] and the terminal
    /// allow. Agent, JSON, plain, CI and piped output never get it.
    pub fn progress_wrap(
        &self,
        mode: ProgressMode,
        env: impl Fn(&str) -> Option<String>,
    ) -> Option<Wrap> {
        if self.mode != OutputMode::Human || self.ci.is_some() {
            return None;
        }
        progress_support(mode, self.stdout_tty, env)
    }
}

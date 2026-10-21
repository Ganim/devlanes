//! Which commands drive the terminal's progress bar, and for which lanes. The
//! encoding, detection and start computation live in `devlanes-term`; the
//! binary follows events and writes the bar.

use devlanes_proto::{Method, Request};
use devlanes_term::{ProgressMode, StartScope, StartTracker, Wrap, progress_support};
use serde_json::Value;

use crate::{OutputMode, Runtime};

/// How long a failed start's red bar stays before the command exits and clears it.
pub const ERROR_HOLD: std::time::Duration = std::time::Duration::from_secs(1);

/// The tracker for a request that follows a start: `start`, `restart`,
/// `lane resume`, `lane new --start` and `wait`. `None` for everything else.
pub fn start_tracker(request: &Request) -> Option<StartTracker> {
    let context = &request.context;
    let scope = || match &context.lane {
        Some(lane) => StartScope::Lane(lane.clone()),
        None => StartScope::Cwd(context.cwd.clone()),
    };
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
        Method::Start | Method::Restart => Some(StartTracker::command(scope()).with_units(units())),
        // Every lane that should be running, or a lane that does not exist yet.
        Method::Resume => Some(StartTracker::command(StartScope::Any)),
        Method::LaneNew if request.params.get("start").and_then(Value::as_bool) == Some(true) => {
            Some(StartTracker::command(StartScope::Any))
        }
        Method::Wait => {
            let target = request.params.get("target").and_then(Value::as_str)?;
            Some(match target.split_once(':') {
                Some((lane, _)) => StartTracker::command(StartScope::Lane(lane.to_owned()))
                    .with_units(vec![target.to_owned()]),
                None if target.contains('@') => {
                    StartTracker::command(StartScope::Lane(target.to_owned()))
                }
                None => StartTracker::command(scope()).with_units(vec![target.to_owned()]),
            })
        }
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

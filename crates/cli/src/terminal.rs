//! What commands write to the terminal besides their text: which features
//! this invocation gets, the tab title of long commands and the notification
//! at the end of a long start. Encoding and detection live in
//! `devlanes-term`; the binary writes them.

use std::time::Duration;

use devlanes_proto::{Event, Method, Request};
use devlanes_term::{Features, LONG_START, Settings, StartTracker};
use serde_json::Value;

use crate::{OutputMode, Runtime, WithPlan, start_tracker};

impl Runtime {
    /// Terminal features for this invocation: none for agent, JSON, plain,
    /// CI or piped output, else as `settings`, their overrides and the
    /// terminal in `env` allow.
    pub fn detect_terminal(
        &self,
        settings: &Settings,
        env: impl Fn(&str) -> Option<String>,
    ) -> Features {
        let human_tty = self.mode == OutputMode::Human && self.stdout_tty && self.ci.is_none();
        Features::detect(settings, human_tty, env)
    }
}

/// The tab title of a long command (`devlanes start acme@0 · 3/8`,
/// `devlanes logs -f api`), kept current from the request's own events.
pub struct CommandTitle {
    verb: &'static str,
    /// Units or the target the command names.
    subject: Vec<String>,
    /// The lane from `--lane`, until events name the lanes.
    lane: Option<String>,
    tracker: Option<StartTracker>,
}

/// The title for `request` when it runs long: `start`, `restart`,
/// `lane resume`, `lane new --start`, `wait`, `logs -f` and `attach`. `None`
/// for everything else, which ends too soon for a title.
pub fn command_title(request: &Request) -> Option<CommandTitle> {
    let strings = |key: &str| -> Vec<String> {
        match request.params.get(key) {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            Some(Value::String(one)) if !one.is_empty() => vec![one.clone()],
            _ => Vec::new(),
        }
    };
    let flag = |key: &str| request.params.get(key).and_then(Value::as_bool) == Some(true);
    let (verb, subject) = match request.method {
        Method::Start => ("start", strings("units")),
        Method::Restart => ("restart", strings("units")),
        Method::Resume => ("lane resume", Vec::new()),
        Method::LaneNew if flag("start") => ("lane new", strings("branch")),
        Method::Wait => ("wait", strings("target")),
        Method::Logs if flag("follow") => ("logs -f", strings("units")),
        Method::Attach => ("attach", strings("unit")),
        _ => return None,
    };
    Some(CommandTitle {
        verb,
        subject,
        lane: request.context.lane.clone(),
        tracker: start_tracker(request),
    })
}

impl CommandTitle {
    /// The title of `devlanes with`, following its start.
    pub fn with(plan: &WithPlan) -> Self {
        let units = plan
            .start
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
            .unwrap_or_default();
        Self {
            verb: "with",
            subject: units,
            lane: plan.start.context.lane.clone(),
            tracker: Some(StartTracker::command()),
        }
    }

    /// One event of the request; only start events change the title.
    pub fn event(&mut self, event: &Event) {
        if let Some(tracker) = &mut self.tracker {
            tracker.event(event);
        }
    }

    /// `devlanes <verb> [lane] [units] [· progress]`.
    pub fn text(&self) -> String {
        let counts = self.tracker.as_ref().map(StartTracker::counts);
        let mut words = vec![format!("devlanes {}", self.verb)];
        words.extend(self.subject_words(counts.as_ref().map(|c| c.lanes.as_slice())));
        let mut text = words.join(" ");
        if let Some(counts) = counts {
            let state = if counts.failed > 0 {
                Some(format!("{} failed", counts.failed))
            } else if counts.phase_failed {
                Some("failed".into())
            } else if counts.total > 0 {
                Some(format!("{}/{}", counts.ready, counts.total))
            } else {
                counts.phase.clone()
            };
            if let Some(state) = state {
                text.push_str(&format!(" · {state}"));
            }
        }
        text
    }

    /// The lane (or how many) and up to three named units.
    fn subject_words(&self, lanes: Option<&[String]>) -> Vec<String> {
        let mut words = Vec::new();
        match lanes.unwrap_or_default() {
            [] => words.extend(self.lane.clone()),
            [one] => words.push(one.clone()),
            many => words.push(format!("{} lanes", many.len())),
        }
        words.extend(self.subject.iter().take(3).cloned());
        if self.subject.len() > 3 {
            words.push(format!("+{}", self.subject.len() - 3));
        }
        words
    }

    /// The notification at the end of a start that ran at least
    /// [`LONG_START`]: `(title, body)`, as `acme@0 is up` / `8 of 8 ready` or
    /// `start of acme@0 failed` / `2 failed`. `None` for commands that follow
    /// no start (`logs -f`, `attach`) and for quick starts.
    pub fn finished(&self, ok: bool, elapsed: Duration) -> Option<(String, String)> {
        let tracker = self.tracker.as_ref().filter(|_| elapsed >= LONG_START)?;
        let counts = tracker.counts();
        let words = self.subject_words(Some(&counts.lanes));
        let subject = if words.is_empty() {
            "the lane".to_owned()
        } else {
            words.join(" ")
        };
        Some(if ok {
            let body = if counts.total > 0 {
                format!("{} of {} ready", counts.ready, counts.total)
            } else {
                String::new()
            };
            (format!("{subject} is up"), body)
        } else {
            let body = if counts.failed > 0 {
                format!("{} failed", counts.failed)
            } else {
                String::new()
            };
            (format!("{} of {subject} failed", self.verb), body)
        })
    }
}

//! Arguments the CLI can list choices for (units, tasks, lanes). When one is
//! missing, a terminal gets a picker; everything else gets the options and the
//! command to run, never a bare "argument required".

use devlanes_proto::{LaneSummary, UnitSummary};
use serde::Serialize;
use serde_json::{Value, json};

use crate::{Hint, OutputMode, Rendered, Runtime, shell_quote};

/// Most options listed when no one can pick interactively.
pub const LISTED: usize = 20;

/// What a missing argument chooses from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChoiceKind {
    /// Any unit of the lane (logs, debug why, restart).
    Unit,
    /// Services with a URL (url print, url open).
    Url,
    /// Units whose process runs (attach).
    Running,
    /// A unit or the lane itself (wait).
    Target,
    /// Tasks of the lane's config (run).
    Task,
    /// Lanes devlanes knows (lane clean).
    Lane,
}

impl ChoiceKind {
    /// The noun for one option: `unit`, `service`, `task`, `lane`.
    pub fn noun(self) -> &'static str {
        match self {
            Self::Unit | Self::Running | Self::Target => "unit",
            Self::Url => "service",
            Self::Task => "task",
            Self::Lane => "lane",
        }
    }
}

/// A command run without the argument it needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    /// Subcommand path, `["url", "open"]`.
    pub command: Vec<&'static str>,
    /// The command's one-line description.
    pub about: String,
    pub kind: ChoiceKind,
    /// Whether several options may be chosen (logs, restart).
    pub multi: bool,
}

impl Choice {
    /// `url open`.
    pub fn name(&self) -> String {
        self.command.join(" ")
    }
}

/// One thing to choose, with what tells it apart.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChoiceOption {
    pub value: String,
    pub detail: String,
}

/// Why nothing can be chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoOptions {
    /// No devlanes.yaml in the directory or its parents (exit 3).
    NoConfig { cwd: String },
    /// The directory belongs to no lane the daemon knows (exit 3).
    NoLane { cwd: String },
    /// The lane exists but has nothing of this kind (exit 2).
    Empty { lane: String },
}

/// The lane's options for `kind` (not for tasks or lanes, which come from
/// their own lists).
pub fn unit_options(kind: ChoiceKind, lane: &LaneSummary) -> Vec<ChoiceOption> {
    let mut options = Vec::new();
    if kind == ChoiceKind::Target {
        options.push(ChoiceOption {
            value: lane.reference.clone(),
            detail: format!("the whole lane · {}", lane.state),
        });
    }
    for unit in &lane.units {
        let url = crate::render::display_url(&unit.urls).cloned();
        let keep = match kind {
            ChoiceKind::Url => (unit.kind.is_empty() || unit.kind == "service") && url.is_some(),
            ChoiceKind::Running => matches!(unit.state.as_str(), "ready" | "starting" | "running"),
            _ => true,
        };
        if keep {
            options.push(ChoiceOption {
                value: unit.name.clone(),
                detail: unit_detail(unit, url.as_deref()),
            });
        }
    }
    options
}

/// `ready · https://api.acme.localhost` or `idle · task`.
fn unit_detail(unit: &UnitSummary, url: Option<&str>) -> String {
    let mut detail = unit.state.clone();
    match url {
        Some(url) => detail.push_str(&format!(" · {url}")),
        None if !unit.kind.is_empty() && unit.kind != "service" => {
            detail.push_str(&format!(" · {}", unit.kind));
        }
        None => {}
    }
    detail
}

/// Tasks from a `task.list` result.
pub fn task_options(data: &Value) -> Vec<ChoiceOption> {
    items(data)
        .filter_map(|task| {
            let name = task.get("name")?.as_str()?.to_owned();
            let detail = match task.get("when").and_then(Value::as_str) {
                Some(when) => format!("runs on {when}"),
                None => "on demand".into(),
            };
            Some(ChoiceOption {
                value: name,
                detail,
            })
        })
        .collect()
}

/// Lanes from a `lane.list` or `status` page.
pub fn lane_options(data: &Value) -> Vec<ChoiceOption> {
    items(data)
        .filter_map(|lane| serde_json::from_value::<LaneSummary>(lane.clone()).ok())
        .filter(|lane| lane.index.is_some())
        .map(|lane| ChoiceOption {
            detail: match &lane.branch {
                Some(branch) => format!("{branch} · {}", lane.state),
                None => lane.state.clone(),
            },
            value: lane.reference,
        })
        .collect()
}

/// `items` of a page.
fn items(data: &Value) -> impl Iterator<Item = &Value> {
    data.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

/// The lane of `lanes` whose worktree holds `cwd` (the deepest one).
pub fn lane_for_cwd<'a>(
    lanes: &'a [LaneSummary],
    cwd: &std::path::Path,
) -> Option<&'a LaneSummary> {
    lanes
        .iter()
        .filter(|lane| !lane.path.as_os_str().is_empty() && cwd.starts_with(&lane.path))
        .max_by_key(|lane| lane.path.as_os_str().len())
}

/// `args` (the command line without the program) with `values` added before a
/// `--`, as a runnable command.
pub fn completed_args(args: &[String], values: &[String]) -> Vec<String> {
    let end = args
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(args.len());
    let mut completed = args[..end].to_vec();
    completed.extend(values.iter().cloned());
    completed.extend(args[end..].iter().cloned());
    completed
}

/// The command `args` with the first option filled in, for the hint.
fn example(args: &[String], options: &[ChoiceOption]) -> String {
    let values: Vec<String> = options
        .first()
        .map(|o| o.value.clone())
        .into_iter()
        .collect();
    std::iter::once("devlanes".to_owned())
        .chain(
            completed_args(args, &values)
                .iter()
                .map(|arg| shell_quote(arg)),
        )
        .collect::<Vec<_>>()
        .join(" ")
}

/// No picker can run: the description, the options and a runnable command, exit 2.
pub fn missing_argument(
    choice: &Choice,
    options: &[ChoiceOption],
    args: &[String],
    runtime: &Runtime,
) -> Rendered {
    let noun = choice.kind.noun();
    let example = example(args, options);
    let stderr = if runtime.mode == OutputMode::Json {
        let listed: Vec<&ChoiceOption> = options.iter().take(LISTED).collect();
        format!(
            "{}\n",
            json!({"error": {"code": "argument_missing", "exit_code": 2, "data": {
                "command": choice.name(),
                "description": choice.about,
                "argument": noun,
                "multiple": choice.multi,
                "options": listed,
                "total": options.len(),
                "example": example,
            }}})
        )
    } else {
        let mut text = format!("error: devlanes {} needs a {noun}\n", choice.name());
        if !choice.about.is_empty() {
            text.push_str(&format!("  {}\n", choice.about));
        }
        let width = options
            .iter()
            .take(LISTED)
            .map(|option| option.value.chars().count())
            .max()
            .unwrap_or(0);
        text.push_str(&format!("{}s:\n", noun));
        for option in options.iter().take(LISTED) {
            text.push_str(&format!(
                "  {:width$}  {}\n",
                crate::render::clean(&option.value),
                crate::render::clean(&option.detail)
            ));
        }
        if options.len() > LISTED {
            text.push_str(&format!("  and {} more\n", options.len() - LISTED));
        }
        text.push_str(&format!("hint: {example}\n"));
        text
    };
    Rendered {
        stderr,
        exit_code: crate::exit::USAGE,
        ..Rendered::default()
    }
}

/// Nothing to choose from: why, and exit 3 when the project or lane is missing.
pub fn no_options(choice: &Choice, why: &NoOptions, runtime: &Runtime) -> Rendered {
    let (message, hint, code) = match why {
        NoOptions::NoConfig { cwd } => (
            format!("no devlanes.yaml in {cwd} or its parents"),
            Some(Hint(format!(
                "create devlanes.yaml at the repository root; a minimal one: {}",
                crate::render::EXAMPLE_CONFIG
            ))),
            crate::exit::NOT_FOUND,
        ),
        NoOptions::NoLane { cwd } => (
            format!("{cwd} is not in a lane devlanes knows"),
            Some(Hint::command(["start"])),
            crate::exit::NOT_FOUND,
        ),
        NoOptions::Empty { lane } => {
            let what = match choice.kind {
                ChoiceKind::Url => "has no services with URLs",
                ChoiceKind::Running => "has no running units",
                ChoiceKind::Task => "has no tasks",
                ChoiceKind::Lane => "has no lanes",
                ChoiceKind::Unit | ChoiceKind::Target => "has no units",
            };
            // Only a stopped lane has an obvious next step; the others need a
            // config change.
            let hint = (choice.kind == ChoiceKind::Running).then(|| Hint::command(["start"]));
            let subject = if choice.kind == ChoiceKind::Lane {
                "devlanes".to_owned()
            } else {
                lane.clone()
            };
            (format!("{subject} {what}"), hint, crate::exit::USAGE)
        }
    };
    let reason = match why {
        NoOptions::NoConfig { .. } => "no_config",
        NoOptions::NoLane { .. } => "no_lane",
        NoOptions::Empty { .. } => "no_options",
    };
    let stderr = if runtime.mode == OutputMode::Json {
        format!(
            "{}\n",
            json!({"error": {"code": "argument_missing", "exit_code": code, "data": {
                "command": choice.name(),
                "description": choice.about,
                "argument": choice.kind.noun(),
                "options": [],
                "total": 0,
                "reason": reason,
                "message": message,
            }}})
        )
    } else {
        let mut text = format!("error: {message}\n");
        if let Some(hint) = hint {
            text.push_str(&format!("hint: {}\n", hint.0));
        }
        text
    };
    Rendered {
        stderr,
        exit_code: code,
        ..Rendered::default()
    }
}

/// A key the picker understands, mapped from the terminal by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKey {
    Up,
    Down,
    /// Space: toggles an option when several may be chosen.
    Toggle,
    Enter,
    /// Esc or Ctrl-C.
    Cancel,
    Backspace,
    /// Typed text filters the options.
    Char(char),
}

/// What a key did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerStep {
    Continue,
    Done(Vec<String>),
    Cancel,
}

/// Selection state of the interactive picker; the caller draws [`Picker::lines`].
#[derive(Debug, Clone)]
pub struct Picker {
    pub title: String,
    options: Vec<ChoiceOption>,
    multi: bool,
    cursor: usize,
    chosen: Vec<bool>,
    filter: String,
}

impl Picker {
    /// A picker over `options`.
    pub fn new(title: String, options: Vec<ChoiceOption>, multi: bool) -> Self {
        let chosen = vec![false; options.len()];
        Self {
            title,
            options,
            multi,
            cursor: 0,
            chosen,
            filter: String::new(),
        }
    }

    /// Indexes of the options matching the typed filter, in order.
    fn visible(&self) -> Vec<usize> {
        let filter = self.filter.to_lowercase();
        (0..self.options.len())
            .filter(|&index| {
                filter.is_empty() || self.options[index].value.to_lowercase().contains(&filter)
            })
            .collect()
    }

    /// Applies one key.
    pub fn key(&mut self, key: PickerKey) -> PickerStep {
        let visible = self.visible();
        match key {
            PickerKey::Cancel => return PickerStep::Cancel,
            PickerKey::Up => self.cursor = self.cursor.saturating_sub(1),
            PickerKey::Down => {
                self.cursor = (self.cursor + 1).min(visible.len().saturating_sub(1));
            }
            PickerKey::Toggle if self.multi => {
                if let Some(&index) = visible.get(self.cursor) {
                    self.chosen[index] = !self.chosen[index];
                }
            }
            PickerKey::Toggle => self.type_char(' '),
            PickerKey::Char(c) => self.type_char(c),
            PickerKey::Backspace => {
                self.filter.pop();
                self.cursor = 0;
            }
            PickerKey::Enter => {
                let mut values: Vec<String> = (0..self.options.len())
                    .filter(|&index| self.multi && self.chosen[index])
                    .map(|index| self.options[index].value.clone())
                    .collect();
                if values.is_empty()
                    && let Some(&index) = visible.get(self.cursor)
                {
                    values.push(self.options[index].value.clone());
                }
                if !values.is_empty() {
                    return PickerStep::Done(values);
                }
            }
        }
        PickerStep::Continue
    }

    /// Adds `c` to the filter and goes back to the first match.
    fn type_char(&mut self, c: char) {
        self.filter.push(c);
        self.cursor = 0;
    }

    /// The picker as at most `height` lines: title, the options around the
    /// cursor (`>` marks it, `[x]` chosen ones), then the keys.
    pub fn lines(&self, height: usize) -> Vec<String> {
        let visible = self.visible();
        let room = height.saturating_sub(2).max(1);
        let first = self
            .cursor
            .saturating_sub(room - 1)
            .min(visible.len().saturating_sub(room));
        let width = visible
            .iter()
            .map(|&index| self.options[index].value.chars().count())
            .max()
            .unwrap_or(0);
        let mut lines = vec![if self.filter.is_empty() {
            self.title.clone()
        } else {
            format!("{} {}", self.title, self.filter)
        }];
        if visible.is_empty() {
            lines.push("  no match".into());
        }
        for (row, &index) in visible.iter().enumerate().skip(first).take(room) {
            let option = &self.options[index];
            let cursor = if row == self.cursor { ">" } else { " " };
            let check = match (self.multi, self.chosen[index]) {
                (false, _) => "",
                (true, true) => "[x] ",
                (true, false) => "[ ] ",
            };
            lines.push(format!(
                "{cursor} {check}{:width$}  {}",
                crate::render::clean(&option.value),
                crate::render::clean(&option.detail)
            ));
        }
        lines.push(if self.multi {
            "up/down move · space choose · enter confirm · type to filter · esc cancel".into()
        } else {
            "up/down move · enter choose · type to filter · esc cancel".into()
        });
        lines
    }
}

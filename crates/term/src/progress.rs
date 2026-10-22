//! The terminal's own progress bar: the `OSC 9;4` sequence ConEmu introduced
//! and cargo uses. This module encodes it, decides whether this terminal draws
//! it, writes it with a guard that always clears it, and works out start
//! progress from daemon events.
//!
//! Clients draw the bar; the daemon never touches a terminal.

use std::{
    collections::{BTreeMap, BTreeSet},
    io::{self, Write},
};

use devlanes_proto::{
    ContainerPhase, Event, LaneSummary, PhaseContainer, PhaseStep, UnitSummary, events,
};
use serde_json::Value;

/// Environment override for the config's `terminal.progress`: `0` turns the
/// bar off, `1` turns it on in any terminal.
pub const PROGRESS_ENV: &str = "DEVLANES_TERM_PROGRESS";

/// One state of the bar, as `OSC 9;4;<state>;<percent>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Progress {
    /// No bar (state 0).
    Clear,
    /// Share done, 0 to 100 (state 1).
    Percent(u8),
    /// Failed at this share (state 2).
    Error(u8),
    /// Busy with no share to show (state 3).
    Indeterminate,
    /// Waiting, drawn as a warning (state 4).
    Paused(u8),
}

impl Progress {
    /// `ESC ] 9 ; 4 ; <state> ; <percent> ESC \`. The percent is always
    /// written (0 where it means nothing), which every terminal that reads the
    /// sequence accepts.
    pub fn sequence(self) -> String {
        let (state, percent) = match self {
            Self::Clear => (0, 0),
            Self::Percent(percent) => (1, percent),
            Self::Error(percent) => (2, percent),
            Self::Indeterminate => (3, 0),
            Self::Paused(percent) => (4, percent),
        };
        format!("\x1b]9;4;{state};{}\x1b\\", percent.min(100))
    }
}

/// The user's `terminal.progress` setting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ProgressMode {
    /// Only in terminals known to draw the bar.
    #[default]
    Auto,
    /// In any terminal.
    Always,
    /// Never.
    Never,
}

/// How the sequence reaches the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wrap {
    /// Written as is.
    Bare,
    /// Inside tmux DCS passthrough, which tmux forwards only with
    /// `allow-passthrough on`; otherwise it drops it silently.
    Tmux,
}

impl Wrap {
    /// `sequence` ready to write.
    pub fn apply(self, sequence: &str) -> String {
        match self {
            Self::Bare => sequence.to_owned(),
            Self::Tmux => format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b")),
        }
    }
}

/// Whether to draw the bar, and how, for a client writing to a terminal
/// (`tty`). [`PROGRESS_ENV`] overrides `mode`. Auto mirrors cargo's
/// `term.progress` detection: Windows Terminal, ConEmu, WezTerm, Ghostty,
/// Ptyxis 48+, Konsole 26.04+, and iTerm2 3.6.6+ (older iTerm2 shows OSC 9 as a
/// notification). It is off inside tmux, screen, CI and dumb terminals.
pub fn progress_support(
    mode: ProgressMode,
    tty: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Option<Wrap> {
    if !tty {
        return None;
    }
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    let mode = match env(PROGRESS_ENV).as_deref().map(str::trim) {
        Some("0" | "false" | "off" | "never") => ProgressMode::Never,
        Some("1" | "true" | "on" | "always") => ProgressMode::Always,
        _ => mode,
    };
    let tmux = set("TMUX");
    match mode {
        ProgressMode::Never => None,
        ProgressMode::Always if tmux => Some(Wrap::Tmux),
        ProgressMode::Always => Some(Wrap::Bare),
        ProgressMode::Auto => {
            let dumb = env("TERM").as_deref() == Some("dumb");
            if tmux || set("STY") || dumb || in_ci(&env) {
                return None;
            }
            draws_progress(&env).then_some(Wrap::Bare)
        }
    }
}

/// [`progress_support`] with the process environment.
pub fn progress_support_from_env(mode: ProgressMode, tty: bool) -> Option<Wrap> {
    progress_support(mode, tty, |key| std::env::var(key).ok())
}

/// CI runners that set a marker; their logs are no place for escape sequences.
fn in_ci(env: &impl Fn(&str) -> Option<String>) -> bool {
    let ci = env("CI").is_some_and(|value| !matches!(value.as_str(), "" | "0" | "false"));
    ci || [
        "GITHUB_ACTIONS",
        "GITLAB_CI",
        "BUILDKITE",
        "CIRCLECI",
        "TF_BUILD",
        "JENKINS_URL",
        "TEAMCITY_VERSION",
    ]
    .iter()
    .any(|key| env(key).is_some_and(|value| !value.is_empty()))
}

/// Terminals known to draw `OSC 9;4` rather than ignore it or show it as text
/// or a notification.
fn draws_progress(env: &impl Fn(&str) -> Option<String>) -> bool {
    let program = env("TERM_PROGRAM").unwrap_or_default();
    let features = env("TERM_FEATURES").is_some_and(|value| features_have_progress(&value));
    if program == "iTerm.app" {
        let version = env("TERM_PROGRAM_VERSION").unwrap_or_default();
        return features || version_at_least(&version, &[3, 6, 6]);
    }
    let ptyxis = env("PTYXIS_VERSION")
        .and_then(|version| version.split('.').next()?.parse::<u32>().ok())
        .is_some_and(|major| major >= 48);
    let konsole = env("KONSOLE_VERSION")
        .and_then(|version| version.parse::<u32>().ok())
        .is_some_and(|version| version >= 260_400);
    features
        || matches!(program.as_str(), "WezTerm" | "ghostty")
        || env("WT_SESSION").is_some()
        || env("ConEmuANSI").as_deref() == Some("ON")
        || ptyxis
        || konsole
}

/// iTerm2's `TERM_FEATURES` lists one capitalized code per feature; `P` is
/// progress (<https://iterm2.com/feature-reporting/>).
fn features_have_progress(value: &str) -> bool {
    let mut current = String::new();
    for ch in value.chars() {
        if !ch.is_ascii_alphanumeric() {
            break;
        }
        if ch.is_ascii_uppercase() {
            if current == "P" {
                return true;
            }
            current.clear();
        }
        current.push(ch);
    }
    current == "P"
}

/// `3.6.6`, `3.6.10beta2` and the like compared component by component on
/// their leading digits.
fn version_at_least(version: &str, wanted: &[u32]) -> bool {
    let parts: Vec<u32> = version
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse().unwrap_or(0)
        })
        .collect();
    if parts.is_empty() || version.is_empty() {
        return false;
    }
    for (index, want) in wanted.iter().enumerate() {
        let have = parts.get(index).copied().unwrap_or(0);
        if have != *want {
            return have > *want;
        }
    }
    true
}

/// Writes the bar to a terminal and clears it when dropped, so an early
/// return, Ctrl-C or an unwinding panic never leaves it in the tab. Disabled
/// (`wrap` `None`) it writes nothing, so callers need no special case.
pub struct ProgressBar<W: Write> {
    out: W,
    wrap: Option<Wrap>,
    shown: Option<Progress>,
}

impl<W: Write> ProgressBar<W> {
    /// A bar on `out`; `wrap` comes from [`progress_support`].
    pub fn new(out: W, wrap: Option<Wrap>) -> Self {
        Self {
            out,
            wrap,
            shown: None,
        }
    }

    /// Whether anything is ever written.
    pub fn enabled(&self) -> bool {
        self.wrap.is_some()
    }

    /// The state on screen, if any.
    pub fn shown(&self) -> Option<Progress> {
        self.shown
    }

    /// Shows `progress`. It is written even when unchanged: some terminals
    /// (Ghostty) drop a bar that is not repeated within about 15 seconds, so
    /// callers resend it every second.
    pub fn set(&mut self, progress: Progress) -> io::Result<()> {
        if progress == Progress::Clear {
            return self.clear();
        }
        self.write(progress)?;
        self.shown = Some(progress);
        Ok(())
    }

    /// Removes the bar if one is shown.
    pub fn clear(&mut self) -> io::Result<()> {
        if self.shown.take().is_some() {
            self.write(Progress::Clear)?;
        }
        Ok(())
    }

    /// The writer, for other output that shares it (OSC 52 copies in the TUI).
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }

    /// Writes one sequence and flushes, so it is never left half-buffered.
    fn write(&mut self, progress: Progress) -> io::Result<()> {
        let Some(wrap) = self.wrap else {
            return Ok(());
        };
        self.out
            .write_all(wrap.apply(&progress.sequence()).as_bytes())?;
        self.out.flush()
    }
}

impl<W: Write> Drop for ProgressBar<W> {
    /// Clears a bar still shown; a write error has nowhere to go here.
    fn drop(&mut self) {
        let _ = self.clear();
    }
}

/// What the tracker knows about one lane.
#[derive(Debug, Default)]
struct Followed {
    summary: Option<LaneSummary>,
    /// A start phase that began and has not ended (`infra`, `setup`, `tasks`, `services`).
    phase: Option<String>,
    /// A phase or a unit failed during this start.
    failed: bool,
    /// Units whose state changed while the tracker ran; older failures do not count.
    fresh: BTreeSet<String>,
    /// The start the phase events belong to (`start_id`), from daemons that send one.
    start: Option<u64>,
    /// The units the start's services phase spawns (`services:started` `units`):
    /// exactly what the start waits for, without the units already running.
    planned: Option<Vec<String>>,
}

impl Followed {
    /// Forgets the previous start: its failures, changes and plan.
    fn restart(&mut self) {
        self.failed = false;
        self.fresh.clear();
        self.planned = None;
    }
}

/// Start progress from lane events: ready units over the units in the start,
/// indeterminate while infra, setup or tasks run or before any unit is queued,
/// a warning while every waiting unit is held for memory, and an error once a
/// phase or a unit fails.
#[derive(Debug)]
pub struct StartTracker {
    /// A command follows every lane its request streams events for, also once
    /// ready; the TUI follows only lanes that are starting now.
    keep: bool,
    units: Vec<String>,
    lanes: BTreeMap<String, Followed>,
}

impl StartTracker {
    /// For a command following the lane events of its own request (`start`,
    /// `restart`, `wait`, ...), which carry only its lanes and its own starts.
    pub fn command() -> Self {
        Self {
            keep: true,
            units: Vec::new(),
            lanes: BTreeMap::new(),
        }
    }

    /// For a viewer (the TUI): every lane while it is starting.
    pub fn viewer() -> Self {
        Self {
            keep: false,
            ..Self::command()
        }
    }

    /// Counts only these units (`unit` or `lane:unit`), as for `restart api`.
    /// Selectors that name no unit (tags) leave every unit counted.
    #[must_use]
    pub fn with_units(mut self, units: Vec<String>) -> Self {
        self.units = units;
        self
    }

    /// Takes a status snapshot, as after subscribing or a `resync`. A lane
    /// still starting keeps its start (id, plan, phase): the snapshot comes
    /// from the same daemon, and dropping the id would make the start's next
    /// event look like a new start and forget the plan. Other lanes forget
    /// theirs. After reconnecting to a possibly new daemon, which numbers its
    /// starts from 1 again, call [`StartTracker::reset`] first.
    pub fn seed(&mut self, lanes: &[LaneSummary]) {
        for lane in lanes {
            let followed = self.lanes.entry(lane.reference.clone()).or_default();
            let starting = lane.state == "starting" || followed.phase.is_some();
            followed.summary = Some(lane.clone());
            if !starting {
                followed.start = None;
            }
        }
    }

    /// Forgets every lane and start, for a connection to a daemon that may
    /// have restarted.
    pub fn reset(&mut self) {
        self.lanes.clear();
    }

    /// Applies one event; others are ignored.
    pub fn event(&mut self, event: &Event) {
        match event.event.as_str() {
            events::LANE_CHANGED => {
                let Some(lane) = parse::<LaneSummary>(event.data.get("lane")) else {
                    return;
                };
                let followed = self.lanes.entry(lane.reference.clone()).or_default();
                if let Some(old) = &followed.summary {
                    for unit in &lane.units {
                        let before = old.units.iter().find(|u| u.name == unit.name);
                        if before.is_none_or(|before| before.state != unit.state) {
                            followed.fresh.insert(unit.name.clone());
                        }
                    }
                }
                followed.summary = Some(lane);
            }
            events::UNIT_CHANGED => {
                let Some(unit) = parse::<UnitSummary>(event.data.get("unit")) else {
                    return;
                };
                let followed = self.lanes.entry(unit.lane.clone()).or_default();
                followed.fresh.insert(unit.name.clone());
                // A restart's stream may carry no `lane.changed` before its units change.
                let summary = followed.summary.get_or_insert_with(|| LaneSummary {
                    reference: unit.lane.clone(),
                    ..LaneSummary::default()
                });
                match summary.units.iter_mut().find(|u| u.name == unit.name) {
                    Some(existing) => *existing = unit,
                    None => summary.units.push(unit),
                }
            }
            events::PHASE => {
                let text = |key: &str| event.data.get(key).and_then(Value::as_str);
                let (Some(lane), Some(phase), Some(state)) =
                    (text("lane"), text("phase"), text("state"))
                else {
                    return;
                };
                let followed = self.lanes.entry(lane.to_owned()).or_default();
                match event.data.get("start_id").and_then(Value::as_u64) {
                    // An earlier start still finishing must not undo the newer one.
                    Some(id) if followed.start.is_some_and(|current| id < current) => return,
                    Some(id) if followed.start != Some(id) => {
                        followed.start = Some(id);
                        followed.restart();
                    }
                    Some(_) => {}
                    // Daemons without start ids: a start always begins with infra.
                    None if state == "started" && phase == "infra" => followed.restart(),
                    None => {}
                }
                match state {
                    "started" => {
                        if let Some(units) = event.data.get("units").and_then(Value::as_array) {
                            followed.planned = Some(
                                units
                                    .iter()
                                    .filter_map(Value::as_str)
                                    .map(str::to_owned)
                                    .collect(),
                            );
                        }
                        followed.phase = Some(phase.to_owned());
                    }
                    // Detail on a phase that still runs.
                    "progress" => followed.phase = Some(phase.to_owned()),
                    "failed" => {
                        followed.failed = true;
                        followed.phase = None;
                    }
                    _ => {
                        if followed.phase.as_deref() == Some(phase) {
                            followed.phase = None;
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Drops failures already shown, so a viewer's error bar can end.
    pub fn forget_failures(&mut self) {
        for followed in self.lanes.values_mut() {
            followed.failed = false;
            followed.fresh.clear();
        }
    }

    /// The bar for the lanes followed now.
    pub fn progress(&self) -> Progress {
        let lanes = self.followed();
        if lanes.is_empty() {
            return if self.keep {
                Progress::Indeterminate
            } else {
                Progress::Clear
            };
        }
        let filter = self.unit_filter(&lanes);
        let mut tally = Tally::default();
        let mut busy = false;
        for (reference, followed) in &lanes {
            tally.failed |= followed.failed;
            busy |= followed.phase.as_deref().is_some_and(|p| p != "services");
            let wanted = |name: &str| filter.is_empty() || named(&filter, reference, name);
            let unit = |name: &str| {
                followed
                    .summary
                    .as_ref()
                    .and_then(|summary| summary.units.iter().find(|unit| unit.name == name))
            };
            match &followed.planned {
                Some(planned) => {
                    for name in planned.iter().filter(|name| wanted(name)) {
                        tally.add(unit(name), followed.fresh.contains(name), true);
                    }
                }
                // Before services the start's units are unknown; what runs already is not its share.
                None if matches!(followed.phase.as_deref(), Some("infra" | "setup" | "tasks")) => {}
                None => {
                    let units = followed.summary.iter().flat_map(|s| &s.units);
                    for unit in units.filter(|unit| counted(unit) && wanted(&unit.name)) {
                        tally.add(Some(unit), followed.fresh.contains(&unit.name), false);
                    }
                }
            }
        }
        let Tally {
            ready,
            total,
            held,
            failed,
        } = tally;
        let percent = (ready * 100)
            .checked_div(total)
            .map_or(0, |share| u8::try_from(share).unwrap_or(100));
        if failed {
            Progress::Error(percent)
        } else if total == 0 || busy && ready == 0 {
            Progress::Indeterminate
        } else if held > 0 && held == total - ready {
            Progress::Paused(percent)
        } else {
            Progress::Percent(percent)
        }
    }

    /// The lanes the bar is about, by reference.
    fn followed(&self) -> Vec<(&String, &Followed)> {
        self.lanes
            .iter()
            .filter(|(_, f)| {
                self.keep
                    || f.phase.is_some()
                    || f.failed
                    || f.summary.as_ref().is_some_and(|s| s.state == "starting")
            })
            .collect()
    }

    /// The unit filter, or nothing when it names no unit of the followed lanes.
    fn unit_filter(&self, lanes: &[(&String, &Followed)]) -> Vec<String> {
        let matches = lanes.iter().any(|(reference, f)| {
            let summary = f
                .summary
                .iter()
                .flat_map(|s| s.units.iter().map(|u| &u.name));
            f.planned
                .iter()
                .flatten()
                .chain(summary)
                .any(|name| named(&self.units, reference, name))
        });
        if matches {
            self.units.clone()
        } else {
            Vec::new()
        }
    }
}

/// Unit counts behind the bar.
#[derive(Debug, Default)]
struct Tally {
    ready: usize,
    total: usize,
    /// Queued units waiting for memory.
    held: usize,
    failed: bool,
}

impl Tally {
    /// Counts one unit in its current state (`None` before its first change). A
    /// `fresh` failure happened during this start; a `planned` unit is waited for
    /// whatever its state, so one not yet queued still counts as pending.
    fn add(&mut self, unit: Option<&UnitSummary>, fresh: bool, planned: bool) {
        match unit.map_or("", |unit| unit.state.as_str()) {
            "ready" | "running" => {
                self.ready += 1;
                self.total += 1;
            }
            "queued" => {
                self.total += 1;
                self.held += usize::from(unit.is_some_and(memory_hold));
            }
            "starting" => self.total += 1,
            "failed" if fresh => {
                self.total += 1;
                self.failed = true;
            }
            _ if planned => self.total += 1,
            _ => {}
        }
    }
}

/// Services (and units from daemons before `kind`); tasks and execs are not
/// what a start waits for.
fn counted(unit: &UnitSummary) -> bool {
    matches!(unit.kind.as_str(), "service" | "")
}

/// `api` or `acme@1:api` in `names`.
fn named(names: &[String], lane: &str, unit: &str) -> bool {
    names
        .iter()
        .any(|name| name == unit || name.split_once(':') == Some((lane, unit)))
}

/// A queued unit waiting for memory rather than for its turn.
fn memory_hold(unit: &UnitSummary) -> bool {
    unit.reason.as_ref().is_some_and(|reason| {
        reason.code == "queued"
            && matches!(
                reason.data.get("hold").and_then(Value::as_str),
                Some("memory" | "memory_critical")
            )
    })
}

/// What a running phase is doing, from the `containers` or `step` of a
/// `lane.phase` `progress` event: `waiting for postgres (1 of 2 healthy)`,
/// `migrate (2 of 3)`. The CLI's phase lines and the TUI's header both say it
/// this way. `None` when the event carries neither.
pub fn phase_detail(data: &Value) -> Option<String> {
    if let Some(containers) = parse::<Vec<PhaseContainer>>(data.get("containers"))
        && !containers.is_empty()
    {
        return Some(containers_detail(&containers));
    }
    let step = parse::<PhaseStep>(data.get("step"))?;
    let name = printable(&step.name);
    Some(if step.count > 1 {
        format!("{name} ({} of {})", step.index, step.count)
    } else {
        name
    })
}

/// The infra line: the first failed container, else the first one not healthy
/// yet with what it waits for, and how many are healthy.
fn containers_detail(containers: &[PhaseContainer]) -> String {
    if let Some(failed) = containers
        .iter()
        .find(|c| c.state == ContainerPhase::Failed)
    {
        let health = failed.health.as_deref().map(printable);
        return match health {
            Some(health) => format!("{} failed ({health})", printable(&failed.name)),
            None => format!("{} failed", printable(&failed.name)),
        };
    }
    let total = containers.len();
    let healthy = containers
        .iter()
        .filter(|c| c.state == ContainerPhase::Healthy)
        .count();
    let waiting: Vec<&PhaseContainer> = containers
        .iter()
        .filter(|c| c.state != ContainerPhase::Healthy)
        .collect();
    let Some(first) = waiting.first() else {
        return format!("{healthy} of {total} healthy");
    };
    let name = printable(&first.name);
    let mut line = match first.state {
        ContainerPhase::Creating => format!("creating {name}"),
        ContainerPhase::Starting => format!("starting {name}"),
        _ => format!("waiting for {name}"),
    };
    if waiting.len() > 1 {
        line.push_str(&format!(" and {} more", waiting.len() - 1));
    }
    line.push_str(&format!(" ({healthy} of {total} healthy)"));
    line
}

/// Config names as one line without terminal control characters.
fn printable(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

/// Reads an event payload field, ignoring one this client cannot parse.
fn parse<T: serde::de::DeserializeOwned>(value: Option<&Value>) -> Option<T> {
    serde_json::from_value(value?.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_proto::RpcError;
    use serde_json::json;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    #[test]
    fn sequences_follow_osc_9_4() {
        assert_eq!(Progress::Clear.sequence(), "\x1b]9;4;0;0\x1b\\");
        assert_eq!(Progress::Percent(42).sequence(), "\x1b]9;4;1;42\x1b\\");
        assert_eq!(Progress::Percent(250).sequence(), "\x1b]9;4;1;100\x1b\\");
        assert_eq!(Progress::Error(60).sequence(), "\x1b]9;4;2;60\x1b\\");
        assert_eq!(Progress::Indeterminate.sequence(), "\x1b]9;4;3;0\x1b\\");
        assert_eq!(Progress::Paused(10).sequence(), "\x1b]9;4;4;10\x1b\\");
        assert_eq!(
            Wrap::Tmux.apply(&Progress::Percent(5).sequence()),
            "\x1bPtmux;\x1b\x1b]9;4;1;5\x1b\x1b\\\x1b\\"
        );
    }

    /// Support for an environment given as `KEY=value` pairs.
    fn support(mode: ProgressMode, tty: bool, pairs: &[(&str, &str)]) -> Option<Wrap> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        progress_support(mode, tty, |key| env.get(key).cloned())
    }

    #[test]
    fn detection_matrix() {
        use ProgressMode::{Always, Auto, Never};
        let bare = Some(Wrap::Bare);
        let ghostty = [("TERM_PROGRAM", "ghostty")];
        for (pairs, expected) in [
            (&ghostty[..], bare),
            (&[("TERM_PROGRAM", "WezTerm")][..], bare),
            (&[("WT_SESSION", "abc")][..], bare),
            (&[("ConEmuANSI", "ON")][..], bare),
            (&[("ConEmuANSI", "OFF")][..], None),
            (&[("PTYXIS_VERSION", "48.2")][..], bare),
            (&[("PTYXIS_VERSION", "47.9")][..], None),
            (&[("KONSOLE_VERSION", "260400")][..], bare),
            (&[("KONSOLE_VERSION", "250800")][..], None),
            // iTerm2 before 3.6.6 posts OSC 9 as a notification.
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("TERM_PROGRAM_VERSION", "3.5.14"),
                ][..],
                None,
            ),
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("TERM_PROGRAM_VERSION", "3.6.5"),
                ][..],
                None,
            ),
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("TERM_PROGRAM_VERSION", "3.6.6"),
                ][..],
                bare,
            ),
            (
                &[("TERM_PROGRAM", "iTerm.app"), ("TERM_FEATURES", "MBT2ScP")][..],
                bare,
            ),
            (&[("TERM_PROGRAM", "iTerm.app")][..], None),
            // kitty exports no version and showed OSC 9 as notifications before 0.38.
            (
                &[("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")][..],
                None,
            ),
            (&[("TERM_PROGRAM", "Apple_Terminal")][..], None),
            (&[][..], None),
            // tmux and screen swallow or garble it unless asked for.
            (
                &[
                    ("TERM_PROGRAM", "ghostty"),
                    ("TMUX", "/tmp/tmux-1/default,1,0"),
                ][..],
                None,
            ),
            (&[("TERM_PROGRAM", "ghostty"), ("STY", "1.pts-0")][..], None),
            (&[("TERM_PROGRAM", "ghostty"), ("CI", "true")][..], None),
            (
                &[("TERM_PROGRAM", "ghostty"), ("GITHUB_ACTIONS", "true")][..],
                None,
            ),
            (&[("TERM_PROGRAM", "ghostty"), ("CI", "false")][..], bare),
            (&[("WT_SESSION", "x"), ("TERM", "dumb")][..], None),
        ] {
            assert_eq!(support(Auto, true, pairs), expected, "{pairs:?}");
        }
        // Piped output never gets it, whatever the settings.
        assert_eq!(support(Auto, false, &ghostty), None);
        assert_eq!(support(Always, false, &[(PROGRESS_ENV, "1")]), None);
        // The setting and the override.
        assert_eq!(support(Never, true, &ghostty), None);
        assert_eq!(support(Always, true, &[]), bare);
        assert_eq!(
            support(Always, true, &[("TMUX", "/tmp/tmux")]),
            Some(Wrap::Tmux)
        );
        assert_eq!(support(Auto, true, &[(PROGRESS_ENV, "1")]), bare);
        assert_eq!(support(Always, true, &[(PROGRESS_ENV, "0")]), None);
        assert_eq!(
            support(
                Auto,
                true,
                &[("TERM_PROGRAM", "ghostty"), (PROGRESS_ENV, "0")]
            ),
            None
        );
    }

    #[test]
    fn iterm_versions_compare_numerically() {
        assert!(version_at_least("3.6.10", &[3, 6, 6]));
        assert!(version_at_least("3.7", &[3, 6, 6]));
        assert!(version_at_least("3.6.6beta1", &[3, 6, 6]));
        assert!(!version_at_least("3.6", &[3, 6, 6]));
        assert!(!version_at_least("", &[3, 6, 6]));
    }

    /// A writer tests can read after the bar that owns a clone is gone.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            self.0
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        }
    }

    #[test]
    fn the_bar_clears_on_drop_and_on_panic() -> io::Result<()> {
        let out = Shared::default();
        {
            let mut bar = ProgressBar::new(out.clone(), Some(Wrap::Bare));
            bar.set(Progress::Percent(30))?;
        }
        assert_eq!(out.text(), "\x1b]9;4;1;30\x1b\\\x1b]9;4;0;0\x1b\\");

        let out = Shared::default();
        let writer = out.clone();
        let result = std::panic::catch_unwind(move || {
            let mut bar = ProgressBar::new(writer, Some(Wrap::Bare));
            let _ = bar.set(Progress::Indeterminate);
            panic!("boom");
        });
        assert!(result.is_err());
        assert!(
            out.text().ends_with("\x1b]9;4;0;0\x1b\\"),
            "{:?}",
            out.text()
        );

        // Nothing shown, nothing to clear; disabled writes nothing at all.
        let out = Shared::default();
        drop(ProgressBar::new(out.clone(), Some(Wrap::Bare)));
        let mut off = ProgressBar::new(out.clone(), None);
        off.set(Progress::Percent(50))?;
        drop(off);
        assert_eq!(out.text(), "");
        Ok(())
    }

    fn unit(lane: &str, name: &str, state: &str) -> UnitSummary {
        UnitSummary {
            name: name.into(),
            lane: lane.into(),
            state: state.into(),
            kind: "service".into(),
            ..UnitSummary::default()
        }
    }

    fn lane(reference: &str, path: &str, state: &str, units: Vec<UnitSummary>) -> LaneSummary {
        LaneSummary {
            reference: reference.into(),
            path: path.into(),
            state: state.into(),
            units,
            ..LaneSummary::default()
        }
    }

    fn event(name: &str, data: Value) -> Event {
        Event {
            seq: 1,
            event: name.into(),
            data,
        }
    }

    fn phase(lane: &str, phase: &str, state: &str) -> Event {
        event(
            events::PHASE,
            json!({"lane": lane, "phase": phase, "state": state}),
        )
    }

    /// A phase event of start `id`, with the planned `units` on `services:started`.
    fn phase_of(lane: &str, phase: &str, state: &str, id: u64, units: &[&str]) -> Event {
        let mut data = json!({"lane": lane, "phase": phase, "state": state, "start_id": id});
        if !units.is_empty() {
            data["units"] = json!(units);
        }
        event(events::PHASE, data)
    }

    fn changed(unit: UnitSummary) -> Event {
        event(events::UNIT_CHANGED, json!({ "unit": unit }))
    }

    #[test]
    fn a_start_goes_from_indeterminate_to_a_share_of_planned_units() {
        let mut tracker = StartTracker::command();
        // Nothing queued yet: no share to show.
        assert_eq!(tracker.progress(), Progress::Indeterminate);
        // `db` already ran before this partial start, so it is not part of it.
        tracker.event(&event(
            events::LANE_CHANGED,
            json!({"lane": lane("acme@1", "/work/acme-1", "starting", vec![
                unit("acme@1", "db", "ready"),
                unit("acme@1", "api", "idle"),
                unit("acme@1", "web", "idle"),
                unit("acme@1", "worker", "idle"),
            ])}),
        ));
        tracker.event(&phase_of("acme@1", "infra", "started", 7, &[]));
        assert_eq!(tracker.progress(), Progress::Indeterminate);
        tracker.event(&phase_of("acme@1", "infra", "done", 7, &[]));
        tracker.event(&phase_of("acme@1", "tasks", "started", 7, &[]));
        assert_eq!(tracker.progress(), Progress::Indeterminate);
        tracker.event(&phase_of("acme@1", "tasks", "done", 7, &[]));
        tracker.event(&phase_of(
            "acme@1",
            "services",
            "started",
            7,
            &["api", "web", "worker"],
        ));
        // The plan is the total before any unit is queued.
        assert_eq!(tracker.progress(), Progress::Percent(0));
        tracker.event(&changed(unit("acme@1", "api", "starting")));
        tracker.event(&changed(unit("acme@1", "api", "ready")));
        assert_eq!(tracker.progress(), Progress::Percent(33));
        tracker.event(&changed(unit("acme@1", "web", "ready")));
        tracker.event(&changed(unit("acme@1", "worker", "ready")));
        tracker.event(&phase_of("acme@1", "services", "done", 7, &[]));
        assert_eq!(tracker.progress(), Progress::Percent(100));
    }

    #[test]
    fn an_older_start_does_not_undo_a_newer_one() {
        let mut tracker = StartTracker::viewer();
        tracker.event(&phase_of("acme@1", "services", "started", 4, &["api"]));
        tracker.event(&phase_of("acme@1", "services", "started", 5, &["web"]));
        // Start 4's failure arrives after start 5 began: start 5 stays green.
        tracker.event(&phase_of("acme@1", "services", "failed", 4, &[]));
        tracker.event(&changed(unit("acme@1", "web", "ready")));
        assert_eq!(tracker.progress(), Progress::Percent(100));
        // A newer start forgets the previous one's failure.
        tracker.event(&phase_of("acme@1", "services", "failed", 5, &[]));
        assert_eq!(tracker.progress(), Progress::Error(100));
        tracker.event(&phase_of("acme@1", "infra", "started", 6, &[]));
        assert_eq!(tracker.progress(), Progress::Indeterminate);
        tracker.event(&phase_of("acme@1", "infra", "done", 6, &[]));
        assert_eq!(tracker.progress(), Progress::Clear);
    }

    #[test]
    fn failures_and_memory_holds_change_the_state() {
        let mut tracker = StartTracker::command();
        let mut held = unit("acme@1", "web", "queued");
        held.reason = Some(RpcError::new("queued", json!({"hold": "memory"}), 0));
        // A failure from an earlier run does not turn this start red.
        tracker.seed(&[lane(
            "acme@1",
            "/work/acme-1",
            "starting",
            vec![
                unit("acme@1", "api", "ready"),
                held,
                unit("acme@1", "old", "failed"),
            ],
        )]);
        assert_eq!(tracker.progress(), Progress::Paused(50));
        let mut turn = unit("acme@1", "web", "queued");
        turn.reason = Some(RpcError::new("queued", json!({"hold": "window_full"}), 0));
        tracker.event(&changed(turn));
        assert_eq!(tracker.progress(), Progress::Percent(50));
        tracker.event(&changed(unit("acme@1", "web", "failed")));
        assert_eq!(tracker.progress(), Progress::Error(50));
        // A failed phase is an error too.
        let mut tracker = StartTracker::command();
        tracker.event(&phase("acme@2", "infra", "started"));
        tracker.event(&phase("acme@2", "infra", "failed"));
        assert_eq!(tracker.progress(), Progress::Error(0));
    }

    #[test]
    fn a_restart_counts_only_its_units() {
        // Without a plan (an older daemon), the named units of the lane.
        let mut tracker = StartTracker::command().with_units(vec!["acme@0:api".into()]);
        tracker.seed(&[lane(
            "acme@0",
            "/work/acme",
            "running",
            vec![
                unit("acme@0", "api", "ready"),
                unit("acme@0", "web", "ready"),
            ],
        )]);
        tracker.event(&changed(unit("acme@0", "api", "stopping")));
        assert_eq!(tracker.progress(), Progress::Indeterminate);
        tracker.event(&changed(unit("acme@0", "api", "starting")));
        assert_eq!(tracker.progress(), Progress::Percent(0));
        // With one, exactly the planned units.
        let mut tracker = StartTracker::command();
        tracker.event(&phase_of("acme@0", "services", "started", 3, &["api"]));
        tracker.event(&changed(unit("acme@0", "web", "ready")));
        assert_eq!(tracker.progress(), Progress::Percent(0));
        tracker.event(&changed(unit("acme@0", "api", "ready")));
        assert_eq!(tracker.progress(), Progress::Percent(100));
        // A tag selector names no unit, so every unit counts.
        let tracker = StartTracker::command().with_units(vec!["frontend".into()]);
        assert_eq!(
            tracker.unit_filter(&tracker.followed()),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_status_refresh_in_the_middle_of_a_start_keeps_its_plan() {
        let mut tracker = StartTracker::viewer();
        let starting = |units| lane("acme@1", "/work/acme-1", "starting", units);
        tracker.seed(&[starting(vec![
            unit("acme@1", "api", "starting"),
            unit("acme@1", "web", "starting"),
            unit("acme@1", "docs", "ready"),
        ])]);
        // The start plans api and web; docs ran already and is not its share.
        tracker.event(&phase_of(
            "acme@1",
            "services",
            "started",
            4,
            &["api", "web"],
        ));
        tracker.event(&changed(unit("acme@1", "api", "ready")));
        assert_eq!(tracker.progress(), Progress::Percent(50));
        // A `resync` (the subscriber fell behind while output flooded): the
        // snapshot must not turn the start's next event into a new start.
        tracker.seed(&[starting(vec![
            unit("acme@1", "api", "ready"),
            unit("acme@1", "web", "starting"),
            unit("acme@1", "docs", "ready"),
        ])]);
        assert_eq!(tracker.progress(), Progress::Percent(50));
        tracker.event(&phase_of("acme@1", "services", "progress", 4, &[]));
        assert_eq!(tracker.progress(), Progress::Percent(50));
        tracker.event(&changed(unit("acme@1", "web", "ready")));
        assert_eq!(tracker.progress(), Progress::Percent(100));
        // After a reconnect the ids start over: reset, then the new start.
        tracker.reset();
        tracker.seed(&[starting(vec![unit("acme@1", "api", "starting")])]);
        tracker.event(&phase_of("acme@1", "services", "started", 1, &["api"]));
        assert_eq!(tracker.progress(), Progress::Percent(0));
    }

    #[test]
    fn a_viewer_follows_lanes_only_while_they_start() {
        let mut tracker = StartTracker::viewer();
        tracker.seed(&[lane(
            "acme@0",
            "/work/acme",
            "running",
            vec![unit("acme@0", "api", "ready")],
        )]);
        assert_eq!(tracker.progress(), Progress::Clear);
        let starting = lane(
            "acme@1",
            "/work/acme-1",
            "starting",
            vec![
                unit("acme@1", "api", "starting"),
                unit("acme@1", "web", "ready"),
            ],
        );
        tracker.event(&event(events::LANE_CHANGED, json!({ "lane": starting })));
        assert_eq!(tracker.progress(), Progress::Percent(50));
        let ready = lane(
            "acme@1",
            "/work/acme-1",
            "running",
            vec![
                unit("acme@1", "api", "ready"),
                unit("acme@1", "web", "ready"),
            ],
        );
        tracker.event(&event(events::LANE_CHANGED, json!({ "lane": ready })));
        assert_eq!(tracker.progress(), Progress::Clear);
        // A failed start shows until the viewer has shown it.
        tracker.event(&phase("acme@1", "infra", "started"));
        tracker.event(&phase("acme@1", "infra", "failed"));
        assert_eq!(tracker.progress(), Progress::Error(100));
        tracker.forget_failures();
        assert_eq!(tracker.progress(), Progress::Clear);
    }

    #[test]
    fn phase_detail_names_what_infra_and_steps_wait_for() {
        let containers = |list: Value| json!({ "containers": list });
        assert_eq!(
            phase_detail(&containers(json!([
                {"name": "redis", "state": "healthy", "health": "healthy"},
                {"name": "postgres", "state": "waiting_healthy", "health": "starting"},
            ]))),
            Some("waiting for postgres (1 of 2 healthy)".into())
        );
        assert_eq!(
            phase_detail(&containers(json!([
                {"name": "postgres", "state": "creating"},
                {"name": "redis", "state": "starting", "health": "restarting"},
                {"name": "mail", "state": "a_newer_state"},
            ]))),
            Some("creating postgres and 2 more (0 of 3 healthy)".into())
        );
        assert_eq!(
            phase_detail(&containers(json!([
                {"name": "redis", "state": "healthy"},
                {"name": "postgres", "state": "failed", "health": "exited (1)"},
            ]))),
            Some("postgres failed (exited (1))".into())
        );
        assert_eq!(
            phase_detail(&containers(json!([{"name": "redis", "state": "healthy"}]))),
            Some("1 of 1 healthy".into())
        );
        let step =
            |index, count| json!({"step": {"name": "migrate", "index": index, "count": count}});
        assert_eq!(phase_detail(&step(2, 3)), Some("migrate (2 of 3)".into()));
        assert_eq!(phase_detail(&step(1, 1)), Some("migrate".into()));
        assert_eq!(phase_detail(&json!({"phase": "infra"})), None);
    }

    #[test]
    fn progress_keeps_a_phase_running() {
        let mut tracker = StartTracker::command();
        tracker.event(&phase_of("acme@1", "infra", "started", 2, &[]));
        tracker.event(&event(
            events::PHASE,
            json!({"lane": "acme@1", "phase": "infra", "state": "progress", "start_id": 2,
                   "containers": [{"name": "db", "state": "starting"}]}),
        ));
        assert_eq!(tracker.progress(), Progress::Indeterminate);
    }
}

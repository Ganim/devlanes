//! The daemon page's log model: `daemon.log` lines parsed into entries
//! (`<timestamp> <LEVEL> <spans:> <target>: <message> <key=value ...>`, with
//! continuation lines), and the rows shown after the level floor, the text
//! filter and the folding of repeats. Pure, so the page, copy mode and the
//! tests agree on what is on screen.

use std::collections::{HashMap, VecDeque};

/// A log level, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl Level {
    fn parse(word: &str) -> Option<Self> {
        Some(match word {
            "ERROR" => Self::Error,
            "WARN" => Self::Warn,
            "INFO" => Self::Info,
            "DEBUG" => Self::Debug,
            "TRACE" => Self::Trace,
            _ => return None,
        })
    }

    /// The five-column tag.
    pub fn tag(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN ",
            Self::Info => "INFO ",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

/// One entry: a line with a timestamp and the lines after it that have none
/// (wrapped errors, panics, backtraces).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The lines as they are in the file.
    pub raw: Vec<String>,
    /// The timestamp as written (RFC 3339), `None` for text before the first one.
    pub time: Option<String>,
    pub level: Level,
    /// The module path without `devlanes_`: `daemon::lifecycle`.
    pub target: String,
    pub message: String,
    pub fields: Vec<(String, String)>,
    /// Continuation lines, shown under the message.
    pub more: Vec<String>,
}

impl Entry {
    /// What makes two entries repeats of each other: everything but the time.
    fn key(&self) -> String {
        let mut key = format!("{:?}\u{1}{}\u{1}{}", self.level, self.target, self.message);
        for (name, value) in &self.fields {
            key.push_str(&format!("\u{1}{name}={value}"));
        }
        for line in &self.more {
            key.push_str(&format!("\u{1}{line}"));
        }
        key
    }

    /// Whether every word of `filter` is in the target, message, fields or
    /// continuation lines, ignoring case.
    pub fn matches(&self, filter: &str) -> bool {
        let mut haystack = format!("{} {}", self.target, self.message);
        for (name, value) in &self.fields {
            haystack.push_str(&format!(" {name}={value}"));
        }
        for line in &self.more {
            haystack.push(' ');
            haystack.push_str(line);
        }
        let haystack = haystack.to_lowercase();
        filter
            .split_whitespace()
            .all(|word| haystack.contains(&word.to_lowercase()))
    }
}

/// Splits `daemon.log` lines into entries, oldest first.
pub fn parse(lines: &VecDeque<String>) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in lines {
        match parse_line(line) {
            Some(entry) => entries.push(entry),
            None => match entries.last_mut() {
                Some(entry) => {
                    entry.raw.push(line.clone());
                    entry.more.push(line.trim().to_owned());
                }
                // Text before the first entry stands alone at INFO.
                None => entries.push(Entry {
                    raw: vec![line.clone()],
                    time: None,
                    level: Level::Info,
                    target: String::new(),
                    message: line.trim().to_owned(),
                    fields: Vec::new(),
                    more: Vec::new(),
                }),
            },
        }
    }
    entries
}

/// `2026-10-21T10:17:07.503Z  INFO request{..}: devlanes_daemon::lifecycle: unit ready unit=acme@0:api took=1.8s`.
fn parse_line(line: &str) -> Option<Entry> {
    let mut words = line.split_whitespace();
    let time = words.next()?;
    if !time.starts_with(|c: char| c.is_ascii_digit()) || !time.contains('T') {
        return None;
    }
    let level = Level::parse(words.next()?)?;
    let after_level = line
        .split_once(time)?
        .1
        .trim_start()
        .strip_prefix(level.tag().trim_end())?
        .trim_start();
    let (head, rest) = target_and_rest(after_level).unwrap_or(("", after_level));
    let target = head.strip_prefix("devlanes_").unwrap_or(head).to_owned();
    let (message, fields) = split_fields(rest);
    Some(Entry {
        raw: vec![line.to_owned()],
        time: Some(time.to_owned()),
        level,
        target,
        message,
        fields,
        more: Vec::new(),
    })
}

/// The target and what follows it. Spans (`request{id=1}:phase{..}:`) come
/// first; the target is the first `: `-separated part without braces or
/// spaces. `None` when the line has no target.
fn target_and_rest(text: &str) -> Option<(&str, &str)> {
    let mut rest = text;
    loop {
        let (head, tail) = rest.split_once(": ")?;
        if head.contains('{') {
            rest = tail;
        } else if head.contains(' ') || head.is_empty() {
            return None;
        } else {
            return Some((head, tail));
        }
    }
}

/// The message and the `key=value` fields that end it; quoted values keep
/// their spaces.
fn split_fields(text: &str) -> (String, Vec<(String, String)>) {
    let mut tokens: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                current.push(c);
            }
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    let is_field = |token: &str| {
        token.split_once('=').is_some_and(|(key, _)| {
            !key.is_empty()
                && key
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
        })
    };
    let split = tokens
        .iter()
        .rposition(|token| !is_field(token))
        .map_or(0, |index| index + 1);
    let fields = tokens[split..]
        .iter()
        .filter_map(|token| token.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.trim_matches('"').to_owned()))
        .collect();
    (tokens[..split].join(" "), fields)
}

/// The lowest level shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Floor {
    #[default]
    All,
    Warn,
    Error,
}

impl Floor {
    /// `w`: all, warn and above, errors only.
    pub fn next(self) -> Self {
        match self {
            Self::All => Self::Warn,
            Self::Warn => Self::Error,
            Self::Error => Self::All,
        }
    }

    pub fn admits(self, level: Level) -> bool {
        match self {
            Self::All => true,
            Self::Warn => level <= Level::Warn,
            Self::Error => level == Level::Error,
        }
    }

    /// For the log title.
    pub fn word(self) -> &'static str {
        match self {
            Self::All => "all levels",
            Self::Warn => "warn and above",
            Self::Error => "errors only",
        }
    }
}

/// What the log shows besides following.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    pub floor: Floor,
    /// `/`: words that must all match.
    pub text: String,
    /// `d` turns folding off.
    pub unfolded: bool,
}

/// One shown entry: its index in the parsed entries (the newest occurrence
/// when folded), how many times it repeats and when it first did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shown {
    pub entry: usize,
    pub count: usize,
    pub since: Option<String>,
}

/// The entries the filters let through, oldest first; with folding on,
/// repeats show once at their newest occurrence with their count.
pub fn shown(entries: &[Entry], filters: &Filters) -> Vec<Shown> {
    let text = filters.text.trim();
    let kept: Vec<usize> = (0..entries.len())
        .filter(|&index| {
            let entry = &entries[index];
            filters.floor.admits(entry.level) && (text.is_empty() || entry.matches(text))
        })
        .collect();
    if filters.unfolded {
        return kept
            .into_iter()
            .map(|entry| Shown {
                entry,
                count: 1,
                since: None,
            })
            .collect();
    }
    // Per repeat key: how many, the first time, the newest index.
    let mut groups: HashMap<String, (usize, Option<String>, usize)> = HashMap::new();
    for &index in &kept {
        let entry = &entries[index];
        let group = groups
            .entry(entry.key())
            .or_insert((0, entry.time.clone(), index));
        group.0 += 1;
        group.2 = index;
    }
    kept.into_iter()
        .filter_map(|index| {
            let (count, since, newest) = groups.get(&entries[index].key())?;
            (*newest == index).then(|| Shown {
                entry: index,
                count: *count,
                since: (*count > 1).then(|| since.clone()).flatten(),
            })
        })
        .collect()
}

/// A timestamp in `zone` as `(YYYY-MM-DD, HH:MM:SS)`; the text as written
/// when it does not parse.
pub fn local(time: &str, zone: &jiff::tz::TimeZone) -> (String, String) {
    match time.parse::<jiff::Timestamp>() {
        Ok(stamp) => {
            let zoned = stamp.to_zoned(zone.clone());
            (
                zoned.strftime("%Y-%m-%d").to_string(),
                zoned.strftime("%H:%M:%S").to_string(),
            )
        }
        Err(_) => (String::new(), time.to_owned()),
    }
}

/// An entry as the page shows it, in plain text: `10:18:08 INFO  daemon::lifecycle    unit ready  unit=…  ×5 since 10:03`.
pub fn display(entry: &Entry, shown: &Shown, zone: &jiff::tz::TimeZone) -> String {
    let time = entry
        .time
        .as_deref()
        .map(|t| local(t, zone).1)
        .unwrap_or_default();
    let mut text = format!(
        "{time:<8} {} {:<20} {}",
        entry.level.tag(),
        clip_left(&entry.target, 20),
        entry.message
    );
    if !entry.fields.is_empty() {
        text.push(' ');
    }
    for (name, value) in &entry.fields {
        text.push_str(&format!(" {name}={value}"));
    }
    if shown.count > 1 {
        let since = shown
            .since
            .as_deref()
            .map(|t| local(t, zone).1)
            .unwrap_or_default();
        text.push_str(&format!(
            "  ×{} since {}",
            shown.count,
            &since[..since.len().min(5)]
        ));
    }
    for line in &entry.more {
        text.push_str(&format!("\n{:36}{line}", ""));
    }
    text
}

/// `text` cut to `width` columns from the left, `…` first when cut:
/// `…r_hosts::sync` keeps the end of a module path.
pub fn clip_left(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_owned();
    }
    let keep: String = text.chars().skip(count - width.saturating_sub(1)).collect();
    format!("…{keep}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(text: &[&str]) -> VecDeque<String> {
        text.iter().map(|line| (*line).to_owned()).collect()
    }

    #[test]
    fn lines_split_into_level_target_message_and_fields() {
        let entries = parse(&lines(&[
            "2026-10-21T10:17:07.503Z  INFO devlanes_daemon::lifecycle: unit ready unit=acme@0:api took=1.8s",
            "2026-10-21T10:18:00.000Z  WARN request{id=1 method=start}:phase{name=\"services\"}: devlanes_resolver_hosts::sync: updating /etc/hosts failed code=helper_missing error=\"the helper is missing\"",
            "  caused by: no such file",
            "2026-10-21T10:19:00.000Z ERROR devlanes_daemon::spawn_error: [devlanes] could not start: x: command not found",
        ]));
        assert_eq!(entries.len(), 3);
        let first = &entries[0];
        assert_eq!(first.level, Level::Info);
        assert_eq!(first.target, "daemon::lifecycle");
        assert_eq!(first.message, "unit ready");
        assert_eq!(
            first.fields,
            [
                ("unit".to_owned(), "acme@0:api".to_owned()),
                ("took".to_owned(), "1.8s".to_owned())
            ]
        );
        // Spans before the target are skipped; quoted values keep spaces;
        // a line without a timestamp continues the entry above.
        let second = &entries[1];
        assert_eq!(second.target, "resolver_hosts::sync");
        assert_eq!(second.message, "updating /etc/hosts failed");
        assert_eq!(
            second.fields[1],
            ("error".into(), "the helper is missing".into())
        );
        assert_eq!(second.more, ["caused by: no such file"]);
        assert_eq!(second.raw.len(), 2);
        // A message with `: ` in it stays whole.
        assert_eq!(
            entries[2].message,
            "[devlanes] could not start: x: command not found"
        );
    }

    #[test]
    fn repeats_fold_at_their_newest_occurrence() {
        let log = lines(&[
            "2026-10-21T09:12:00.000Z  WARN devlanes_daemon::gate: start window full lane=acme@1",
            "2026-10-21T09:13:00.000Z  INFO devlanes_daemon::lifecycle: unit ready unit=acme@0:api",
            "2026-10-21T09:14:00.000Z  WARN devlanes_daemon::gate: start window full lane=acme@1",
            "2026-10-21T09:15:00.000Z ERROR devlanes_daemon::lifecycle: unit exited unit=acme@0:web",
        ]);
        let entries = parse(&log);
        let folded = shown(&entries, &Filters::default());
        assert_eq!(
            folded
                .iter()
                .map(|s| (s.entry, s.count))
                .collect::<Vec<_>>(),
            [(1, 1), (2, 2), (3, 1)]
        );
        assert_eq!(folded[1].since.as_deref(), Some("2026-10-21T09:12:00.000Z"));
        // Unfolded, every line; the floor and the text filter combine.
        let all = Filters {
            unfolded: true,
            ..Filters::default()
        };
        assert_eq!(shown(&entries, &all).len(), 4);
        let warn = Filters {
            floor: Floor::Warn,
            ..Filters::default()
        };
        assert_eq!(shown(&entries, &warn).len(), 2);
        let errors = Filters {
            floor: Floor::Error,
            ..Filters::default()
        };
        assert_eq!(shown(&entries, &errors)[0].entry, 3);
        let text = Filters {
            text: "UNIT acme@0:api".into(),
            ..Filters::default()
        };
        assert_eq!(shown(&entries, &text).len(), 1);
    }

    #[test]
    fn long_targets_keep_their_end() {
        assert_eq!(
            clip_left("resolver_hosts::sync", 20),
            "resolver_hosts::sync"
        );
        assert_eq!(
            clip_left("resolver_hosts::sync::retry", 20),
            "…_hosts::sync::retry"
        );
    }
}

use std::{
    collections::VecDeque,
    time::{SystemTime, UNIX_EPOCH},
};

use devlanes_proto::{LogLine, LogQuery, Page};
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, sinks::UTF8};

use crate::{
    Error, Result, UnitLog,
    level::rank,
    storage::{open_reader_at, read_record},
};

const MAX_QUERY_BYTES: usize = 4 * 1024 * 1024;
const MAX_QUERY_LINES: usize = 10_000;
const MAX_CONTEXT: usize = 1_000;

impl UnitLog {
    /// Returns bounded structured data; presentation shaping belongs to the client.
    /// `max_bytes` counts serialized LogLine bytes plus one separator per item.
    /// Cursors `unit#seq` page forward; returned `before:unit#seq` cursors page back.
    /// Tail pages are chronological, and their next cursor retrieves older records.
    /// Long records are UTF-8-safely clipped only if a single record exceeds the budget.
    pub fn query(&mut self, query: &LogQuery) -> Result<Page<LogLine>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Query(e.to_string()))?
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.query_at(query, now)
    }

    /// Caller time makes relative `since` durations deterministic in tests and replay.
    /// `since` accepts Unix milliseconds, RFC3339, or an integer followed by ms/s/m/h/d.
    pub fn query_at(&mut self, query: &LogQuery, now_ms: u64) -> Result<Page<LogLine>> {
        if query.context_lines > MAX_CONTEXT {
            return Err(Error::Query("context exceeds 1000 lines".into()));
        }
        if query.around.is_some() && query.cursor.is_some() {
            return Err(Error::Query("around and cursor cannot be combined".into()));
        }
        if query.tail == Some(0) || query.max_bytes == Some(0) {
            return Err(Error::Query("tail and byte budget must be positive".into()));
        }
        let since = query
            .since
            .as_deref()
            .map(|s| parse_since(s, now_ms))
            .transpose()?;
        let min_level = query
            .level
            .as_deref()
            .map(|s| rank(s).ok_or_else(|| Error::Query(format!("unknown level {s}"))))
            .transpose()?;
        let matcher = query
            .grep
            .as_deref()
            .map(|s| RegexMatcher::new(s).map_err(|e| Error::Query(e.to_string())))
            .transpose()?;
        let mut lower = 0;
        let mut upper = u64::MAX;
        let mut backwards = query.tail.is_some();
        if let Some(cursor) = &query.cursor {
            if let Some(cursor) = cursor.strip_prefix("before:") {
                upper = self.parse_cursor(cursor)?.saturating_sub(1);
                backwards = true;
            } else {
                lower = self
                    .parse_cursor(cursor)?
                    .checked_add(1)
                    .ok_or(Error::SequenceExhausted)?;
                backwards = false;
            }
        }
        if let Some(cursor) = &query.around {
            let anchor = self.parse_cursor(cursor)?;
            lower = anchor.saturating_sub(query.context_lines as u64);
            upper = anchor.saturating_add(query.context_lines as u64);
        }
        let mut page = PageBuilder::new(
            query.max_bytes.unwrap_or(1024 * 1024).min(MAX_QUERY_BYTES),
            query.tail.unwrap_or(1000).min(MAX_QUERY_LINES),
            backwards,
        );
        let empty = LogLine {
            unit: self.unit.clone(),
            seq: u64::MAX,
            timestamp_ms: u64::MAX,
            text: String::new(),
            level: Some("error".into()),
        };
        if encoded_size(&empty)? > page.max_bytes {
            return Err(Error::Query(
                "byte budget cannot hold log record metadata".into(),
            ));
        }
        if !query.units.is_empty() && !query.units.contains(&self.unit) {
            return Ok(Page::complete(Vec::new()));
        }
        self.flush()?;
        let mut searcher = Searcher::new();
        let mut pending = VecDeque::<LogLine>::new();
        let mut trailing = 0usize;
        let context = if query.around.is_some() {
            0
        } else {
            query.context_lines
        };
        'segments: for segment in &self.segments {
            if segment.records == 0
                || segment.last_seq < lower
                || segment.first_seq > upper
                || since.is_some_and(|s| segment.max_timestamp_ms < s)
            {
                continue;
            }
            let offset = segment
                .index
                .iter()
                .rev()
                .find(|entry| entry.seq <= lower)
                .map_or(0, |entry| entry.offset);
            let mut reader = open_reader_at(&segment.path, segment.compressed, offset)?;
            while let Some((line, _)) = read_record(reader.as_mut(), &self.unit, false)? {
                if line.seq < lower
                    || line.seq > upper
                    || since.is_some_and(|s| line.timestamp_ms < s)
                {
                    continue;
                }
                let level_matches = min_level.is_none_or(|minimum| {
                    line.level
                        .as_deref()
                        .and_then(rank)
                        .is_some_and(|level| level >= minimum)
                });
                let mut matches = level_matches;
                if matches && let Some(matcher) = &matcher {
                    matches = false;
                    searcher.search_slice(
                        matcher,
                        line.text.as_bytes(),
                        UTF8(|_, _| {
                            matches = true;
                            Ok(false)
                        }),
                    )?;
                }
                if matches {
                    while let Some(previous) = pending.pop_front() {
                        if !page.push(previous)? {
                            break 'segments;
                        }
                    }
                    if !page.push(line)? {
                        break 'segments;
                    }
                    trailing = context;
                } else if trailing > 0 {
                    if !page.push(line)? {
                        break 'segments;
                    }
                    trailing -= 1;
                } else if context > 0 {
                    pending.push_back(line);
                    if pending.len() > context {
                        pending.pop_front();
                    }
                }
            }
        }
        Ok(page.finish(&self.unit))
    }

    /// Cursor identity includes the unit so accidental cross-unit paging fails loudly.
    fn parse_cursor(&self, value: &str) -> Result<u64> {
        let (unit, sequence) = value
            .rsplit_once('#')
            .ok_or_else(|| Error::Query("cursor must be unit#sequence".into()))?;
        if unit != self.unit {
            return Err(Error::Query("cursor belongs to a different unit".into()));
        }
        sequence
            .parse()
            .map_err(|_| Error::Query("invalid cursor sequence".into()))
    }
}

struct PageBuilder {
    items: VecDeque<LogLine>,
    bytes: usize,
    max_bytes: usize,
    max_lines: usize,
    backwards: bool,
    omitted: bool,
    clipped: bool,
}

impl PageBuilder {
    /// Tail queries retain a byte-bounded suffix instead of collecting the entire run.
    fn new(max_bytes: usize, max_lines: usize, backwards: bool) -> Self {
        Self {
            items: VecDeque::new(),
            bytes: 0,
            max_bytes,
            max_lines,
            backwards,
            omitted: false,
            clipped: false,
        }
    }

    /// False stops a forward scan once another matching record proves truncation.
    fn push(&mut self, mut line: LogLine) -> Result<bool> {
        let mut size = encoded_size(&line)?;
        if !self.backwards
            && !self.items.is_empty()
            && (self.items.len() == self.max_lines || self.bytes + size > self.max_bytes)
        {
            self.omitted = true;
            return Ok(false);
        }
        if size > self.max_bytes {
            clip(&mut line, self.max_bytes)?;
            size = encoded_size(&line)?;
            self.clipped = true;
        }
        if self.backwards {
            while !self.items.is_empty()
                && (self.items.len() >= self.max_lines || self.bytes + size > self.max_bytes)
            {
                if let Some(removed) = self.items.pop_front() {
                    self.bytes -= encoded_size(&removed)?;
                    self.omitted = true;
                }
            }
        }
        self.bytes += size;
        self.items.push_back(line);
        Ok(true)
    }

    /// A continuation cursor always excludes the boundary record to avoid duplicates.
    fn finish(self, unit: &str) -> Page<LogLine> {
        let next_cursor = if self.omitted && self.backwards {
            self.items
                .front()
                .map(|line| format!("before:{unit}#{}", line.seq))
        } else if self.omitted {
            self.items.back().map(|line| format!("{unit}#{}", line.seq))
        } else {
            None
        };
        Page {
            items: self.items.into_iter().collect(),
            truncated: self.omitted || self.clipped,
            next_cursor,
        }
    }
}

/// JSON escaping counts toward wire budgets, including quotes, tabs and control bytes.
fn encoded_size(line: &LogLine) -> Result<usize> {
    Ok(serde_json::to_vec(line)?.len() + 1)
}

/// Binary search over UTF-8 boundaries also handles JSON escape expansion correctly.
fn clip(line: &mut LogLine, budget: usize) -> Result<()> {
    let text = std::mem::take(&mut line.text);
    let boundaries: Vec<_> = text
        .char_indices()
        .map(|(i, _)| i)
        .chain(std::iter::once(text.len()))
        .collect();
    let mut low = 0;
    let mut high = boundaries.len();
    while low + 1 < high {
        let middle = low + (high - low) / 2;
        line.text = text[..boundaries[middle]].to_owned();
        if encoded_size(line)? <= budget {
            low = middle;
        } else {
            high = middle;
        }
    }
    line.text = text[..boundaries[low]].to_owned();
    Ok(())
}

/// Duration subtraction saturates so querying beyond the epoch remains meaningful.
fn parse_since(value: &str, now_ms: u64) -> Result<u64> {
    if let Ok(timestamp) = value.parse::<u64>() {
        return Ok(timestamp);
    }
    for (suffix, multiplier) in [
        ("ms", 1),
        ("s", 1000),
        ("m", 60_000),
        ("h", 3_600_000),
        ("d", 86_400_000),
    ] {
        if let Some(number) = value.strip_suffix(suffix) {
            let duration = number
                .parse::<u64>()
                .ok()
                .and_then(|n| n.checked_mul(multiplier))
                .ok_or_else(|| Error::Query("invalid since duration".into()))?;
            return Ok(now_ms.saturating_sub(duration));
        }
    }
    let timestamp = value
        .parse::<jiff::Timestamp>()
        .map_err(|e| Error::Query(e.to_string()))?;
    u64::try_from(timestamp.as_millisecond())
        .map_err(|_| Error::Query("since precedes Unix epoch".into()))
}

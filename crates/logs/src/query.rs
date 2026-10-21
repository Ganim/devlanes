use std::{
    collections::VecDeque,
    fs::File,
    io::{BufRead, BufReader, ErrorKind, Read, Seek, SeekFrom},
    time::{SystemTime, UNIX_EPOCH},
};

use devlanes_proto::{LogLine, LogQuery, Page};
use grep_regex::RegexMatcher;
use grep_searcher::{Searcher, sinks::UTF8};

use crate::{
    Error, LogReader, Result, SegmentMetadata, UnitLog,
    level::rank,
    storage::{read_record, skip},
};

const MAX_QUERY_BYTES: usize = 4 * 1024 * 1024;
const MAX_QUERY_LINES: usize = 10_000;
const MAX_CONTEXT: usize = 1_000;

impl UnitLog {
    /// Queries a fresh [`UnitLog::reader`] snapshot. Callers sharing the log behind a
    /// lock should take the snapshot under it and query after releasing it.
    pub fn query(&mut self, query: &LogQuery) -> Result<Page<LogLine>> {
        self.reader().query(query)
    }

    /// Deterministic-time variant of [`UnitLog::query`].
    pub fn query_at(&mut self, query: &LogQuery, now_ms: u64) -> Result<Page<LogLine>> {
        self.reader().query_at(query, now_ms)
    }
}

impl LogReader {
    /// Returns bounded structured data; presentation shaping belongs to the client.
    /// `max_bytes` counts serialized LogLine bytes plus one separator per item.
    /// Cursors `unit#seq` page forward; returned `before:unit#seq` cursors page back.
    /// Tail pages are chronological, and their next cursor retrieves older records.
    /// Long records are UTF-8-safely clipped only if a single record exceeds the budget.
    pub fn query(&self, query: &LogQuery) -> Result<Page<LogLine>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| Error::Query(e.to_string()))?
            .as_millis()
            .min(u128::from(u64::MAX)) as u64;
        self.query_at(query, now)
    }

    /// Caller time makes relative `since` durations deterministic in tests and replay.
    /// `since` accepts Unix milliseconds, RFC3339, or an integer followed by ms/s/m/h/d.
    pub fn query_at(&self, query: &LogQuery, now_ms: u64) -> Result<Page<LogLine>> {
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
        let page = PageBuilder::new(
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
        let mut scan = Scan {
            page,
            searcher: Searcher::new(),
            matcher,
            min_level,
            since,
            lower,
            upper,
            context: if query.around.is_some() {
                0
            } else {
                query.context_lines
            },
            pending: VecDeque::new(),
            trailing: 0,
        };
        let candidates = self.segments.iter().filter(|segment| {
            segment.records > 0
                && segment.last_seq >= lower
                && segment.first_seq <= upper
                && since.is_none_or(|s| segment.max_timestamp_ms >= s)
        });
        // Tail pages walk newest first and stop once full, so their cost follows the
        // page size rather than the amount of retained history.
        let ordered: Vec<_> = if backwards {
            candidates.rev().collect()
        } else {
            candidates.collect()
        };
        for segment in ordered {
            let more = if backwards {
                scan_backward(segment, &self.unit, &mut scan)
            } else {
                scan_forward(segment, &self.unit, &mut scan)
            };
            match more {
                Ok(true) => {}
                Ok(false) => break,
                // Retention removed it after the snapshot; older or newer data remains.
                Err(Error::Io(error)) if error.kind() == ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok(scan.page.finish(&self.unit))
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

/// Filter and context state shared by both scan directions.
struct Scan {
    page: PageBuilder,
    searcher: Searcher,
    matcher: Option<RegexMatcher>,
    min_level: Option<u8>,
    since: Option<u64>,
    lower: u64,
    upper: u64,
    context: usize,
    /// Non-matching neighbours seen just before the next match, in scan order.
    pending: VecDeque<LogLine>,
    trailing: usize,
}

impl Scan {
    /// Feeds one record in scan order. Context is symmetric, so a backward scan
    /// selects the same lines as a forward one. False means the page is full.
    fn visit(&mut self, line: LogLine) -> Result<bool> {
        if line.seq < self.lower
            || line.seq > self.upper
            || self.since.is_some_and(|s| line.timestamp_ms < s)
        {
            return Ok(true);
        }
        let mut matches = self.min_level.is_none_or(|minimum| {
            line.level
                .as_deref()
                .and_then(rank)
                .is_some_and(|level| level >= minimum)
        });
        if matches && let Some(matcher) = &self.matcher {
            matches = false;
            self.searcher.search_slice(
                matcher,
                line.text.as_bytes(),
                UTF8(|_, _| {
                    matches = true;
                    Ok(false)
                }),
            )?;
        }
        if matches {
            while let Some(previous) = self.pending.pop_front() {
                if !self.page.push(previous)? {
                    return Ok(false);
                }
            }
            if !self.page.push(line)? {
                return Ok(false);
            }
            self.trailing = self.context;
        } else if self.trailing > 0 {
            if !self.page.push(line)? {
                return Ok(false);
            }
            self.trailing -= 1;
        } else if self.context > 0 {
            self.pending.push_back(line);
            if self.pending.len() > self.context {
                self.pending.pop_front();
            }
        }
        Ok(true)
    }
}

/// Opens a snapshot's segment. An active segment the writer has since rotated is
/// read from its compressed successor, whose uncompressed offsets are identical.
fn open_segment(segment: &SegmentMetadata) -> Result<(File, bool)> {
    match File::open(&segment.path) {
        Ok(file) => Ok((file, segment.compressed)),
        Err(error) if error.kind() == ErrorKind::NotFound && !segment.compressed => {
            Ok((File::open(segment.path.with_extension("log.zst"))?, true))
        }
        Err(error) => Err(error.into()),
    }
}

/// Streams from the index entry at or before `lower` to the snapshot's end.
fn scan_forward(segment: &SegmentMetadata, unit: &str, scan: &mut Scan) -> Result<bool> {
    let start = segment
        .index
        .iter()
        .rev()
        .find(|entry| entry.seq <= scan.lower)
        .map_or(0, |entry| entry.offset);
    let length = segment.uncompressed_bytes.saturating_sub(start);
    let (mut file, compressed) = open_segment(segment)?;
    // Bounded by the snapshot, so records appended after it stay invisible.
    let mut reader: Box<dyn BufRead> = if compressed {
        let mut decoder = BufReader::new(zstd::stream::read::Decoder::new(file)?);
        skip(&mut decoder, start)?;
        Box::new(decoder.take(length))
    } else {
        file.seek(SeekFrom::Start(start))?;
        Box::new(BufReader::new(file.take(length)))
    };
    while let Some((line, _)) = read_record(reader.as_mut(), unit, !segment.closed)? {
        if line.seq > scan.upper {
            return Ok(false);
        }
        if !scan.visit(line)? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Reads index blocks (256 records each) newest first. Plain files seek per block;
/// a zstd stream cannot, so the needed range is decompressed once, at most one segment.
fn scan_backward(segment: &SegmentMetadata, unit: &str, scan: &mut Scan) -> Result<bool> {
    let blocks: Vec<(u64, u64)> = segment
        .index
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            let next = segment.index.get(i + 1);
            let last_seq = next.map_or(segment.last_seq, |n| n.seq.saturating_sub(1));
            let end = next.map_or(segment.uncompressed_bytes, |n| n.offset);
            (entry.seq <= scan.upper && last_seq >= scan.lower).then_some((entry.offset, end))
        })
        .collect();
    let (Some(&(low, _)), Some(&(_, high))) = (blocks.first(), blocks.last()) else {
        return Ok(true);
    };
    let (file, compressed) = open_segment(segment)?;
    let decompressed = if compressed {
        let mut decoder = zstd::stream::read::Decoder::new(&file)?;
        skip(&mut decoder, low)?;
        let mut bytes = Vec::new();
        decoder
            .take(high.saturating_sub(low))
            .read_to_end(&mut bytes)?;
        Some(bytes)
    } else {
        None
    };
    for &(start, end) in blocks.iter().rev() {
        let mut lines = Vec::new();
        let mut reader: Box<dyn BufRead + '_> =
            match &decompressed {
                Some(bytes) => {
                    let block = usize::try_from(start.saturating_sub(low))
                        .ok()
                        .zip(usize::try_from(end.saturating_sub(low)).ok())
                        .and_then(|(a, b)| bytes.get(a..b));
                    Box::new(block.ok_or_else(|| {
                        Error::Corrupt("index offset exceeds segment length".into())
                    })?)
                }
                None => {
                    let mut handle = &file;
                    handle.seek(SeekFrom::Start(start))?;
                    Box::new(BufReader::new(handle.take(end.saturating_sub(start))))
                }
            };
        while let Some((line, _)) = read_record(reader.as_mut(), unit, !segment.closed)? {
            lines.push(line);
        }
        for line in lines.into_iter().rev() {
            if !scan.visit(line)? {
                return Ok(false);
            }
        }
    }
    Ok(true)
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
    /// Backward pages are filled newest first and kept chronological.
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

    /// False stops the scan once another matching record proves truncation.
    fn push(&mut self, mut line: LogLine) -> Result<bool> {
        let mut size = encoded_size(&line)?;
        if !self.items.is_empty()
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
        self.bytes += size;
        if self.backwards {
            self.items.push_front(line);
        } else {
            self.items.push_back(line);
        }
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

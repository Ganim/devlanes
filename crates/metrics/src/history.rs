use std::{collections::BTreeMap, mem, time::Duration};

use serde::{Deserialize, Serialize};

use crate::{Ring, Sample, Subject};

/// 1 s points kept per series: 15 minutes at the daemon's 1 Hz tick.
pub const RAW_CAPACITY: usize = 900;
/// Width of the coarse in-memory windows.
pub const COARSE_INTERVAL_MS: u64 = 10_000;
/// Coarse windows kept per series: 6 hours of 10 s windows.
pub const COARSE_CAPACITY: usize = 2_160;
/// Width of the rollups handed to the daemon for SQLite persistence.
pub const ROLLUP_INTERVAL_MS: u64 = 60_000;

/// One raw reading in a series.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub ts_ms: u64,
    pub cpu_percent: f64,
    pub footprint_bytes: u64,
    pub rss_bytes: u64,
}

/// Min, mean and max CPU percent over a window.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CpuStats {
    pub min: f64,
    pub avg: f64,
    pub max: f64,
}

/// Min, mean (rounded) and max bytes over a window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ByteStats {
    pub min: u64,
    pub avg: u64,
    pub max: u64,
}

/// Aggregate of the samples whose timestamps fall in `[start_ms, start_ms + width)`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub start_ms: u64,
    pub samples: u32,
    pub cpu: CpuStats,
    pub footprint: ByteStats,
    pub rss: ByteStats,
}

/// A closed one-minute window, ready for the daemon's `metric_rollups` table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RollupRecord {
    pub subject: Subject,
    /// Minute-aligned; `window.start_ms` is a multiple of [`ROLLUP_INTERVAL_MS`].
    pub window: Window,
}

/// Per-series capacities. Defaults give 15 minutes of 1 s points and 6 hours of 10 s
/// windows; tests shrink them to exercise eviction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryConfig {
    pub raw_capacity: usize,
    pub coarse_capacity: usize,
}

impl Default for HistoryConfig {
    /// Returns the 15 minute / 6 hour retention from the monitoring design.
    fn default() -> Self {
        Self {
            raw_capacity: RAW_CAPACITY,
            coarse_capacity: COARSE_CAPACITY,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Extremes {
    min: f64,
    max: f64,
    sum: f64,
}

impl Extremes {
    /// Starts an aggregate from its first value.
    fn new(value: f64) -> Self {
        Self {
            min: value,
            max: value,
            sum: value,
        }
    }

    /// Folds in another value.
    fn add(&mut self, value: f64) {
        self.min = self.min.min(value);
        self.max = self.max.max(value);
        self.sum += value;
    }
}

#[derive(Debug, Clone, Copy)]
struct Accumulator {
    start_ms: u64,
    samples: u32,
    cpu: Extremes,
    footprint: Extremes,
    rss: Extremes,
}

impl Accumulator {
    /// Opens a window aligned to `width_ms` around the first point.
    fn new(point: &Point, width_ms: u64) -> Self {
        Self {
            start_ms: point.ts_ms - point.ts_ms % width_ms,
            samples: 1,
            cpu: Extremes::new(point.cpu_percent),
            footprint: Extremes::new(point.footprint_bytes as f64),
            rss: Extremes::new(point.rss_bytes as f64),
        }
    }

    /// Folds a point that belongs to this window.
    fn add(&mut self, point: &Point) {
        self.samples = self.samples.saturating_add(1);
        self.cpu.add(point.cpu_percent);
        self.footprint.add(point.footprint_bytes as f64);
        self.rss.add(point.rss_bytes as f64);
    }

    /// Produces the window's statistics without consuming it, so open windows can be
    /// shown live.
    fn window(&self) -> Window {
        let n = f64::from(self.samples);
        let bytes = |extremes: Extremes| ByteStats {
            min: extremes.min as u64,
            avg: (extremes.sum / n).round() as u64,
            max: extremes.max as u64,
        };
        Window {
            start_ms: self.start_ms,
            samples: self.samples,
            cpu: CpuStats {
                min: self.cpu.min,
                avg: self.cpu.sum / n,
                max: self.cpu.max,
            },
            footprint: bytes(self.footprint),
            rss: bytes(self.rss),
        }
    }
}

/// In-memory history of one subject.
#[derive(Debug, Clone)]
pub struct Series {
    raw: Ring<Point>,
    coarse: Ring<Window>,
    open_coarse: Option<Accumulator>,
    open_minute: Option<Accumulator>,
}

impl Series {
    /// Creates an empty series with the given capacities.
    fn new(config: HistoryConfig) -> Self {
        Self {
            raw: Ring::new(config.raw_capacity),
            coarse: Ring::new(config.coarse_capacity),
            open_coarse: None,
            open_minute: None,
        }
    }

    /// Raw points, oldest first.
    pub fn points(&self) -> impl DoubleEndedIterator<Item = &Point> + ExactSizeIterator {
        self.raw.iter()
    }

    /// Closed 10 s windows, oldest first. The window still filling is
    /// [`Series::open_window`].
    pub fn windows(&self) -> impl DoubleEndedIterator<Item = &Window> + ExactSizeIterator {
        self.coarse.iter()
    }

    /// The 10 s window that is still receiving points.
    pub fn open_window(&self) -> Option<Window> {
        self.open_coarse.as_ref().map(Accumulator::window)
    }

    /// Newest raw point.
    pub fn latest(&self) -> Option<&Point> {
        self.raw.last()
    }

    /// Appends a point and returns a closed minute when the point starts a new one.
    /// Points older than the latest are rejected (`Err(())`) so windows stay ordered.
    fn push(&mut self, point: Point) -> Result<Option<Window>, ()> {
        if self.raw.last().is_some_and(|last| point.ts_ms < last.ts_ms) {
            return Err(());
        }
        self.raw.push(point);
        if let Some(window) = advance(&mut self.open_coarse, &point, COARSE_INTERVAL_MS) {
            self.coarse.push(window);
        }
        Ok(advance(&mut self.open_minute, &point, ROLLUP_INTERVAL_MS))
    }

    /// Closes open windows that ended at or before `now_ms`; returns a closed minute.
    fn close_until(&mut self, now_ms: u64) -> Option<Window> {
        if let Some(window) = take_ended(&mut self.open_coarse, now_ms, COARSE_INTERVAL_MS) {
            self.coarse.push(window);
        }
        take_ended(&mut self.open_minute, now_ms, ROLLUP_INTERVAL_MS)
    }
}

/// Adds `point` to the open window or replaces it, returning the replaced window.
fn advance(open: &mut Option<Accumulator>, point: &Point, width_ms: u64) -> Option<Window> {
    match open {
        Some(accumulator) if point.ts_ms < accumulator.start_ms + width_ms => {
            accumulator.add(point);
            None
        }
        _ => open
            .replace(Accumulator::new(point, width_ms))
            .map(|closed| closed.window()),
    }
}

/// Takes the open window once its interval has fully elapsed.
fn take_ended(open: &mut Option<Accumulator>, now_ms: u64, width_ms: u64) -> Option<Window> {
    open.take_if(|accumulator| accumulator.start_ms + width_ms <= now_ms)
        .map(|closed| closed.window())
}

/// In-memory history for every subject plus the queue of closed minute rollups.
///
/// The daemon records each tick's samples, calls [`History::drain_rollups`] about
/// once a minute and writes the records in one transaction.
#[derive(Debug, Clone, Default)]
pub struct History {
    config: HistoryConfig,
    series: BTreeMap<Subject, Series>,
    completed: Vec<RollupRecord>,
}

impl History {
    /// Creates an empty history with custom capacities.
    pub fn new(config: HistoryConfig) -> Self {
        Self {
            config,
            series: BTreeMap::new(),
            completed: Vec::new(),
        }
    }

    /// Records one sample. Returns `false` when it is older than the subject's latest
    /// point and was dropped.
    pub fn record(&mut self, sample: &Sample) -> bool {
        let point = Point {
            ts_ms: sample.ts_ms,
            cpu_percent: sample.cpu_percent,
            footprint_bytes: sample.footprint_bytes,
            rss_bytes: sample.rss_bytes,
        };
        let config = self.config;
        let series = self
            .series
            .entry(sample.subject.clone())
            .or_insert_with(|| Series::new(config));
        match series.push(point) {
            Ok(closed) => {
                if let Some(window) = closed {
                    self.completed.push(RollupRecord {
                        subject: sample.subject.clone(),
                        window,
                    });
                }
                true
            }
            Err(()) => false,
        }
    }

    /// Records every sample in order; returns how many were accepted.
    pub fn record_all(&mut self, samples: &[Sample]) -> usize {
        samples.iter().filter(|sample| self.record(sample)).count()
    }

    /// Returns one subject's history.
    pub fn series(&self, subject: &Subject) -> Option<&Series> {
        self.series.get(subject)
    }

    /// Iterates subjects with history, in [`Subject`] order.
    pub fn subjects(&self) -> impl Iterator<Item = &Subject> {
        self.series.keys()
    }

    /// Closes every window that ended at or before `now_ms` and returns all closed
    /// minutes not yet drained. Idle series therefore flush their last minute
    /// without waiting for another sample.
    pub fn drain_rollups(&mut self, now_ms: u64) -> Vec<RollupRecord> {
        for (subject, series) in &mut self.series {
            if let Some(window) = series.close_until(now_ms) {
                self.completed.push(RollupRecord {
                    subject: subject.clone(),
                    window,
                });
            }
        }
        mem::take(&mut self.completed)
    }

    /// Forgets series whose newest point is older than `max_idle`, e.g. a stopped
    /// lane's processes. Call [`History::drain_rollups`] first so their last minute
    /// is not lost.
    pub fn prune(&mut self, now_ms: u64, max_idle: Duration) -> usize {
        let cutoff = retention_cutoff_ms(now_ms, max_idle);
        let before = self.series.len();
        self.series.retain(|_, series| {
            series.open_minute.is_some() || series.latest().is_some_and(|p| p.ts_ms >= cutoff)
        });
        before - self.series.len()
    }
}

/// Oldest timestamp to keep under a retention period, e.g. `metrics.retention` for
/// `DELETE FROM metric_rollups WHERE start_ms < ?`.
pub fn retention_cutoff_ms(now_ms: u64, retention: Duration) -> u64 {
    now_ms.saturating_sub(u64::try_from(retention.as_millis()).unwrap_or(u64::MAX))
}

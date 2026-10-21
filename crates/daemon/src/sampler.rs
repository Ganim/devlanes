//! Resource sampling (plan 13.5). Ticks every second while a client watches
//! (subscribers or recent `metrics` calls), and every five seconds otherwise while
//! units run, so CI summaries still get per-start peaks. Monitoring only.

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use devlanes_core::UnitRunId;
use devlanes_metrics::{
    History, RunTracker, SampleRequest, Subject, UnitKey, UnitTarget, aggregate,
};
use devlanes_proto::{Context, LaneSummary, RpcError, UnitPeak};
use devlanes_state::{RollupRow, RunSummaryRow};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    engine::{Inner, params},
    model::{LaneRt, lock},
    units::now_ms,
};

/// How long a `metrics` call keeps the fast tick on.
const INTEREST: Duration = Duration::from_secs(15);
/// Points of history returned per unit.
const HISTORY_POINTS: usize = 120;

/// Sampler state shared with the `metrics` method.
#[derive(Default)]
pub(crate) struct MetricsState {
    pub history: History,
    pub tracker: RunTracker,
    /// Runs being tracked, with their persistence series key.
    pub tracked: HashMap<UnitRunId, String>,
    /// Latest `(cpu, memory)` per `(lane reference, unit)`.
    pub latest: BTreeMap<(String, String), (f64, u64)>,
    /// Series key per `(lane reference, unit)` for history lookups.
    pub subjects: BTreeMap<(String, String), Subject>,
    pub interest_until: Option<Instant>,
}

/// `metrics` parameters.
#[derive(Debug, Default, Deserialize)]
struct MetricsParams {
    #[serde(default)]
    peaks: bool,
}

/// One unit to sample with its lane.
struct Target {
    lane: Arc<LaneRt>,
    reference: String,
    name: String,
    run: Option<UnitRunId>,
    key: UnitKey,
}

impl Inner {
    /// Runs the sampler until shutdown.
    pub(crate) async fn sampler_loop(self: Arc<Self>) {
        if self.integ.metrics.is_empty() {
            return;
        }
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        let mut count: u64 = 0;
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            count += 1;
            let interested = self.bus.subscribers() > 0
                || lock(&self.metrics)
                    .interest_until
                    .is_some_and(|until| until > Instant::now());
            if interested || count.is_multiple_of(5) {
                self.sample_once().await;
            }
        }
    }

    /// Samples every live unit once and updates history, peaks and rollups.
    pub(crate) async fn sample_once(self: &Arc<Self>) {
        let mut targets = Vec::new();
        let mut request = SampleRequest::default();
        for (project, lane) in self.all_lanes() {
            let (reference, lane_id, units) = {
                let st = lane.st();
                let Some(record) = &st.record else { continue };
                let units: Vec<_> = st
                    .units
                    .iter()
                    .filter(|(_, rec)| rec.host.is_alive())
                    .map(|(name, rec)| (name.clone(), rec.host.clone(), rec.run_id))
                    .collect();
                (st.reference(&project.name()), record.id, units)
            };
            for (name, host, run) in units {
                let key = UnitKey {
                    project: project.id,
                    lane: lane_id,
                    unit: name.clone(),
                };
                let identity = host.identity();
                let mut processes = host.subjects().await;
                if let Some(identity) = identity
                    && !processes.iter().any(|p| p.pid == identity.pid)
                {
                    processes.push(identity);
                }
                request.units.push(UnitTarget {
                    unit: key.clone(),
                    sessions: identity.map(|i| vec![i.sid]).unwrap_or_default(),
                    processes,
                });
                targets.push(Target {
                    lane: lane.clone(),
                    reference: reference.clone(),
                    name,
                    run,
                    key,
                });
            }
        }
        let mut leaves = Vec::new();
        for source in &self.integ.metrics {
            match source.sample(&request).await {
                Ok(samples) => leaves.extend(samples),
                Err(error) => tracing::debug!(source = source.name(), %error, "sample failed"),
            }
        }
        let totals = aggregate(&leaves);
        let now = now_ms();
        let (rollups, finished) = {
            let mut state = lock(&self.metrics);
            state.history.record_all(&totals);
            for target in &targets {
                if let Some(run) = target.run
                    && !state.tracked.contains_key(&run)
                {
                    state
                        .tracker
                        .start(run, target.key.clone(), now.saturating_sub(1000));
                    state
                        .tracked
                        .insert(run, Subject::Unit(target.key.clone()).key());
                }
                state.subjects.insert(
                    (target.reference.clone(), target.name.clone()),
                    Subject::Unit(target.key.clone()),
                );
            }
            state.tracker.observe_all(&totals);
            let live: Vec<UnitRunId> = targets.iter().filter_map(|t| t.run).collect();
            let ended: Vec<(UnitRunId, String)> = state
                .tracked
                .iter()
                .filter(|(run, _)| !live.contains(run))
                .map(|(run, series)| (*run, series.clone()))
                .collect();
            let mut finished = Vec::new();
            for (run, series) in ended {
                state.tracked.remove(&run);
                if let Some(summary) = state.tracker.finish(run, now) {
                    finished.push((summary, series));
                }
            }
            for target in &targets {
                let Some(sample) = totals
                    .iter()
                    .find(|s| s.subject == Subject::Unit(target.key.clone()))
                else {
                    continue;
                };
                state.latest.insert(
                    (target.reference.clone(), target.name.clone()),
                    (sample.cpu_percent, sample.footprint_bytes),
                );
                let mut st = target.lane.st();
                let peak = st.peaks.entry(target.name.clone()).or_insert((None, None));
                peak.0 = Some(peak.0.unwrap_or(0).max(sample.footprint_bytes));
                peak.1 = Some(peak.1.unwrap_or(0.0).max(sample.cpu_percent));
            }
            (state.history.drain_rollups(now), finished)
        };
        let rows: Vec<RollupRow> = rollups
            .into_iter()
            .map(|record| RollupRow {
                series: record.subject.key(),
                minute_ms: record.window.start_ms,
                samples: record.window.samples,
                cpu_min: record.window.cpu.min,
                cpu_avg: record.window.cpu.avg,
                cpu_max: record.window.cpu.max,
                mem_min: record.window.footprint.min,
                mem_avg: record.window.footprint.avg,
                mem_max: record.window.footprint.max,
                rss_max: record.window.rss.max,
            })
            .collect();
        let _ = self.store.insert_rollups(rows).await;
        for (summary, series) in finished {
            let _ = self
                .store
                .record_run_summary(RunSummaryRow {
                    unit_run_id: summary.run,
                    series,
                    started_ms: summary.started_ms,
                    ended_ms: summary.ended_ms,
                    samples: summary.samples,
                    mem_p50: summary.footprint.p50,
                    mem_p95: summary.footprint.p95,
                    mem_peak: summary.footprint.peak,
                    cpu_p50: summary.cpu.p50,
                    cpu_p95: summary.cpu.p95,
                    cpu_peak: summary.cpu.peak,
                    rss_peak: summary.rss_peak,
                })
                .await;
        }
    }

    /// Adds the latest CPU and memory to a lane summary's units.
    pub(crate) fn fill_metrics(&self, summary: &mut LaneSummary) {
        let state = lock(&self.metrics);
        for unit in &mut summary.units {
            if unit.state == "idle" || unit.state == "stopped" || unit.state == "exited" {
                continue;
            }
            if let Some((cpu, memory)) = state
                .latest
                .get(&(summary.reference.clone(), unit.name.clone()))
            {
                unit.cpu_percent = Some(*cpu);
                unit.memory_bytes = Some(*memory);
            }
        }
    }

    /// `metrics`: per-unit samples and history, or `peaks: true` for the current
    /// lane start.
    pub(crate) async fn metrics_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: MetricsParams = params(value)?;
        let first = {
            let mut state = lock(&self.metrics);
            let first = state
                .interest_until
                .is_none_or(|until| until < Instant::now());
            state.interest_until = Some(Instant::now() + INTEREST);
            first
        };
        if first {
            self.sample_once().await;
        }
        if args.peaks {
            let (project, lane) = self.target_lane(context).await?;
            let st = lane.st();
            let mut names: Vec<String> = st.units.keys().cloned().collect();
            names.extend(
                st.peaks
                    .keys()
                    .filter(|k| !st.units.contains_key(*k))
                    .cloned(),
            );
            let peaks: Vec<UnitPeak> = names
                .into_iter()
                .map(|unit| {
                    let peak = st.peaks.get(&unit).copied().unwrap_or((None, None));
                    UnitPeak {
                        unit,
                        peak_memory_bytes: peak.0,
                        peak_cpu_percent: peak.1,
                    }
                })
                .collect();
            return Ok(json!({"lane": st.reference(&project.name()), "peaks": peaks}));
        }
        let state = lock(&self.metrics);
        let mut units = Vec::new();
        for (project, lane) in self.all_lanes() {
            let st = lane.st();
            let reference = st.reference(&project.name());
            for (name, rec) in &st.units {
                if !rec.host.is_alive() {
                    continue;
                }
                let key = (reference.clone(), name.clone());
                let latest = state.latest.get(&key).copied();
                let history: Vec<Value> = state
                    .subjects
                    .get(&key)
                    .and_then(|subject| state.history.series(subject))
                    .map(|series| {
                        let points: Vec<_> = series.points().collect();
                        let skip = points.len().saturating_sub(HISTORY_POINTS);
                        points
                            .into_iter()
                            .skip(skip)
                            .map(|p| json!([p.ts_ms, p.cpu_percent, p.footprint_bytes]))
                            .collect()
                    })
                    .unwrap_or_default();
                units.push(json!({
                    "lane": reference,
                    "unit": name,
                    "cpu_percent": latest.map(|l| l.0),
                    "memory_bytes": latest.map(|l| l.1),
                    "history": history,
                }));
            }
        }
        Ok(json!({"units": units}))
    }

    /// Daily retention: run history 30 days, rollups per `metrics.retention`.
    pub(crate) async fn retention_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(Duration::from_secs(86_400));
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            let now = i64::try_from(now_ms()).unwrap_or(i64::MAX);
            let runs = now - 30 * 86_400_000;
            let metrics = now
                - i64::try_from(self.opts.user.metrics.retention.0.as_millis()).unwrap_or(i64::MAX);
            if let Err(error) = self.store.prune_history(runs, metrics.max(0)).await {
                tracing::warn!(%error, "history retention failed");
            }
        }
    }
}

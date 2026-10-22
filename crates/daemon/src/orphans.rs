//! Processes a previous daemon left running (decision 16 over plan 4.3). After a
//! crash or restart they keep running: startup records each one whose identity
//! (pid, session, start time) still verifies as an `orphaned` unit of its lane,
//! and only an explicit stop, restart, lane clean or state reset terminates it.
//! They are never adopted into supervision, since this daemon cannot reap them or
//! read their output. Descendants that left the leader's session are recorded
//! while the unit runs ([`Inner::track_escaped`]), so termination reaches them too.

use std::{collections::HashSet, sync::Arc, time::Duration};

use devlanes_core::{ExitOutcome, ProcessIdentity, UnitRunId};
use devlanes_proto::{RpcError, UnitSummary};
use devlanes_state::UnitRun;
use futures_util::future::join_all;

use crate::{
    engine::Inner,
    model::{LaneRt, Orphan, ProjectRt, orphan_summary, proto_actor},
    registry::store_error,
    units::{UnitHost, now_ms},
};

/// Grace between SIGTERM and SIGKILL when an orphan is stopped.
pub(crate) const ORPHAN_GRACE: Duration = Duration::from_secs(5);
/// How often orphans are re-verified and escaped descendants recorded.
const CHECK_INTERVAL: Duration = Duration::from_secs(2);
/// Attempts to finish a run row before giving up until the next chance.
const FINISH_ATTEMPTS: u32 = 3;

/// Exit recorded when this daemon terminated an orphan (SIGTERM, maybe SIGKILL).
#[cfg(unix)]
const TERMINATED: ExitOutcome = ExitOutcome {
    code: None,
    signal: Some(15),
};
/// Exit recorded when this daemon terminated an orphan: the exit code
/// `TerminateProcess` gives it, as Windows has no signals.
#[cfg(windows)]
const TERMINATED: ExitOutcome = ExitOutcome {
    code: Some(1),
    signal: None,
};
/// Exit recorded for an orphan whose process ended on its own: unknown.
const GONE: ExitOutcome = ExitOutcome {
    code: None,
    signal: None,
};

impl Inner {
    /// Startup: unfinished runs whose leader or a recorded escaped descendant
    /// still verifies become orphans of their lane; the rest are marked ended.
    /// Call after projects are loaded.
    pub(crate) async fn track_orphans(self: &Arc<Self>, runs: Vec<UnitRun>) {
        let mut members = Vec::with_capacity(runs.len());
        for run in &runs {
            members.push(self.store.unit_run_members(run.id).await.unwrap_or_else(|error| {
                tracing::warn!(%error, run = run.id.0, "escaped descendants of a run unreadable");
                Vec::new()
            }));
        }
        let checks = join_all(
            runs.iter()
                .zip(&members)
                .map(|(run, members)| self.any_alive(run.process, members)),
        )
        .await;
        let since_ms = now_ms();
        let lanes = self.all_lanes();
        let mut tracked = 0;
        for ((run, members), alive) in runs.into_iter().zip(members).zip(checks) {
            if !alive {
                self.finish_run(run.id, GONE).await;
                continue;
            }
            let lane = lanes.iter().find(|(_, lane)| {
                lane.st()
                    .record
                    .as_ref()
                    .is_some_and(|r| r.id == run.lane_id)
            });
            let Some((project, lane)) = lane else {
                // Kept unfinished: the next startup that knows its lane shows it,
                // and `daemon reset-state` terminates it.
                tracing::warn!(pid = run.process.pid, unit = %run.unit, "orphan of an unknown lane left running");
                continue;
            };
            lane.st().orphans.entry(run.unit.clone()).or_insert(Orphan {
                run_id: run.id,
                process: run.process,
                started_at_ms: u64::try_from(run.started_at).unwrap_or_default(),
                since_ms,
                actor: proto_actor(&run.actor),
                members,
            });
            tracked += 1;
            self.emit_lane(project, lane);
        }
        if tracked > 0 {
            tracing::warn!(
                tracked,
                "units of a previous daemon are still running; left as orphaned"
            );
        }
    }

    /// Terminates the orphans among `names` (verified signalling: SIGTERM to the
    /// recorded session and escaped descendants, then SIGKILL), ends their runs and
    /// returns their summaries as `stopped`, keeping `orphaned_since_ms`.
    pub(crate) async fn terminate_orphans(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        names: &[String],
    ) -> Vec<UnitSummary> {
        let picked: Vec<(String, Orphan, UnitSummary)> = {
            let st = lane.st();
            let reference = st.reference(&project.name());
            names
                .iter()
                .filter_map(|name| {
                    let orphan = st.orphans.get(name)?;
                    let summary = orphan_summary(&st, &reference, name, orphan);
                    Some((name.clone(), orphan.clone(), summary))
                })
                .collect()
        };
        if picked.is_empty() {
            return Vec::new();
        }
        join_all(
            picked
                .iter()
                .map(|(_, orphan, _)| self.terminate_all(orphan.process, &orphan.members)),
        )
        .await;
        let mut stopped = Vec::new();
        for (name, orphan, mut summary) in picked {
            // An unfinished row keeps the record, so the liveness check retries it.
            if self.finish_run(orphan.run_id, TERMINATED).await {
                let mut st = lane.st();
                if st
                    .orphans
                    .get(&name)
                    .is_some_and(|o| o.run_id == orphan.run_id)
                {
                    st.orphans.remove(&name);
                }
            }
            tracing::info!(pid = orphan.process.pid, unit = %name, "terminated a previous daemon's unit");
            summary.state = "stopped".into();
            stopped.push(summary);
        }
        self.emit_lane(project, lane);
        self.publish_routes();
        stopped
    }

    /// Clears orphans whose processes are all gone (exited, or their pids reused).
    pub(crate) async fn check_orphans(self: &Arc<Self>) {
        for (project, lane) in self.all_lanes() {
            let orphans: Vec<(String, Orphan)> = lane
                .st()
                .orphans
                .iter()
                .map(|(name, orphan)| (name.clone(), orphan.clone()))
                .collect();
            let mut changed = false;
            for (name, orphan) in orphans {
                if self.any_alive(orphan.process, &orphan.members).await {
                    continue;
                }
                if !self.finish_run(orphan.run_id, GONE).await {
                    continue;
                }
                let mut st = lane.st();
                if st
                    .orphans
                    .get(&name)
                    .is_some_and(|o| o.run_id == orphan.run_id)
                {
                    st.orphans.remove(&name);
                    changed = true;
                }
            }
            if changed {
                self.emit_lane(&project, &lane);
                self.publish_routes();
            }
        }
    }

    /// Re-verifies orphans every few seconds until shutdown.
    pub(crate) async fn orphan_watch_loop(self: Arc<Self>) {
        let mut tick = tokio::time::interval(CHECK_INTERVAL);
        loop {
            tokio::select! {
                () = self.shutdown.cancelled() => return,
                _ = tick.tick() => {}
            }
            let any = self
                .all_lanes()
                .iter()
                .any(|(_, lane)| !lane.st().orphans.is_empty());
            if any {
                self.check_orphans().await;
            }
        }
    }

    /// Records the live descendants of a running unit that left its leader's
    /// session (on Windows, every descendant), so a daemon started after a crash
    /// can stop them with the run. Reads the runner's last snapshot (no
    /// process-table scan) every few seconds until the unit exits, and rewrites the
    /// run's rows only when the live set changed.
    pub(crate) fn track_escaped(self: &Arc<Self>, host: UnitHost, run: UnitRunId) {
        let Some(leader) = host.identity() else {
            return;
        };
        let engine = self.clone();
        tokio::spawn(async move {
            // The set last stored; the rows are only rewritten when it changes.
            let mut recorded: HashSet<(u32, u64)> = HashSet::new();
            let mut tick = tokio::time::interval(CHECK_INTERVAL);
            while host.is_alive() {
                tokio::select! {
                    () = engine.shutdown.cancelled() => return,
                    _ = tick.tick() => {}
                }
                // Only live ones: on Windows every member counts as escaped, and
                // watch-mode tools start children all day long.
                let escaped: Vec<ProcessIdentity> = host
                    .subjects()
                    .await
                    .into_iter()
                    .filter(|p| p.pid != leader.pid && p.sid != leader.sid)
                    .collect();
                let current: HashSet<(u32, u64)> =
                    escaped.iter().map(|p| (p.pid, p.start_time)).collect();
                if current == recorded {
                    continue;
                }
                match engine.store.replace_unit_run_members(run, escaped).await {
                    Ok(()) => recorded = current,
                    // The run already ended; its descendants are no longer tracked.
                    Err(devlanes_state::Error::NotFound(_) | devlanes_state::Error::Closed) => {
                        return;
                    }
                    Err(error) => tracing::debug!(%error, "escaped descendants not recorded"),
                }
            }
        });
    }

    /// Before the store is reset or closed: finishes the rows of this daemon's
    /// exited units with their exit status, without waiting for their watchers.
    pub(crate) async fn finish_stopped_runs(&self) {
        let runs: Vec<(UnitRunId, Option<ExitOutcome>)> = self
            .all_lanes()
            .iter()
            .flat_map(|(_, lane)| {
                lane.st()
                    .units
                    .values()
                    .filter(|rec| !rec.host.is_alive())
                    .filter_map(|rec| rec.run_id.map(|id| (id, rec.host.exit_outcome())))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (id, outcome) in runs {
            self.finish_run(id, outcome.unwrap_or(GONE)).await;
        }
    }

    /// `daemon reset-state`, after this daemon's units were stopped: terminates
    /// every process still recorded as running (orphans of known and unknown
    /// lanes alike, with their escaped descendants) through verified signals and
    /// finishes every run, so the reset erases no identity of a live process.
    /// Returns how many runs had to be terminated.
    pub(crate) async fn end_recorded_runs(self: &Arc<Self>) -> Result<usize, RpcError> {
        self.finish_stopped_runs().await;
        let runs = self.store.active_unit_runs().await.map_err(store_error)?;
        let ended = join_all(runs.into_iter().map(|run| async move {
            let members = self
                .store
                .unit_run_members(run.id)
                .await
                .unwrap_or_default();
            let alive = self.any_alive(run.process, &members).await;
            if alive {
                self.terminate_all(run.process, &members).await;
            }
            self.finish_run(run.id, if alive { TERMINATED } else { GONE })
                .await;
            alive
        }))
        .await;
        Ok(ended.into_iter().filter(|alive| *alive).count())
    }

    /// Finishes a run row, retrying briefly. An unfinished row keeps the unit's
    /// active-run slot (one per lane and unit) taken, which would refuse its next
    /// start. Returns false when the row is still unfinished.
    pub(crate) async fn finish_run(&self, id: UnitRunId, outcome: ExitOutcome) -> bool {
        let mut delay = Duration::from_millis(100);
        for attempt in 1..=FINISH_ATTEMPTS {
            match self.store.finish_unit_run(id, outcome).await {
                // A missing row was reset or pruned: nothing is left to finish.
                Ok(()) | Err(devlanes_state::Error::NotFound(_)) => return true,
                Err(devlanes_state::Error::Closed) => return false,
                Err(error) if attempt == FINISH_ATTEMPTS => {
                    tracing::warn!(%error, run = id.0, "unit run left unfinished");
                }
                Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay *= 2;
                }
            }
        }
        false
    }

    /// True when the leader or any recorded escaped descendant still verifies.
    async fn any_alive(&self, leader: ProcessIdentity, members: &[ProcessIdentity]) -> bool {
        if self.integ.processes.alive(leader).await {
            return true;
        }
        for member in members {
            if self.integ.processes.alive(*member).await {
                return true;
            }
        }
        false
    }

    /// Terminates the leader's session and each escaped descendant's, every
    /// target checked by identity before it is signalled.
    async fn terminate_all(&self, leader: ProcessIdentity, members: &[ProcessIdentity]) {
        let processes = &self.integ.processes;
        join_all(
            std::iter::once(leader)
                .chain(members.iter().copied())
                .map(|identity| processes.terminate(identity, ORPHAN_GRACE)),
        )
        .await;
    }
}

//! Processes a previous daemon left running (decision 16 over plan 4.3). After a
//! crash or restart they keep running: startup records each one whose identity
//! (pid, session, start time) still verifies as an `orphaned` unit of its lane,
//! and only an explicit stop, restart or lane clean terminates it. They are never
//! adopted into supervision, since this daemon cannot reap them or read their output.

use std::{sync::Arc, time::Duration};

use devlanes_core::ExitOutcome;
use devlanes_proto::UnitSummary;
use devlanes_state::UnitRun;
use futures_util::future::join_all;

use crate::{
    engine::Inner,
    model::{LaneRt, Orphan, ProjectRt, orphan_summary, proto_actor},
    units::now_ms,
};

/// Grace between SIGTERM and SIGKILL when an orphan is stopped.
pub(crate) const ORPHAN_GRACE: Duration = Duration::from_secs(5);
/// How often orphans are re-verified.
const CHECK_INTERVAL: Duration = Duration::from_secs(2);

/// Exit recorded when this daemon terminated an orphan (SIGTERM, maybe SIGKILL).
const TERMINATED: ExitOutcome = ExitOutcome {
    code: None,
    signal: Some(15),
};
/// Exit recorded for an orphan whose process ended on its own: unknown.
const GONE: ExitOutcome = ExitOutcome {
    code: None,
    signal: None,
};

impl Inner {
    /// Startup: unfinished runs whose process still verifies become orphans of
    /// their lane; the rest are marked ended. Call after projects are loaded.
    pub(crate) async fn track_orphans(self: &Arc<Self>, runs: Vec<UnitRun>) {
        let checks = join_all(
            runs.iter()
                .map(|run| self.integ.processes.alive(run.process)),
        )
        .await;
        let since_ms = now_ms();
        let lanes = self.all_lanes();
        let mut tracked = 0;
        for (run, alive) in runs.into_iter().zip(checks) {
            if !alive {
                let _ = self.store.finish_unit_run(run.id, GONE).await;
                continue;
            }
            let lane = lanes.iter().find(|(_, lane)| {
                lane.st()
                    .record
                    .as_ref()
                    .is_some_and(|r| r.id == run.lane_id)
            });
            let Some((project, lane)) = lane else {
                // Kept unfinished: the next startup that knows its lane shows it.
                tracing::warn!(pid = run.process.pid, unit = %run.unit, "orphan of an unknown lane left running");
                continue;
            };
            lane.st().orphans.entry(run.unit.clone()).or_insert(Orphan {
                run_id: run.id,
                process: run.process,
                started_at_ms: u64::try_from(run.started_at).unwrap_or_default(),
                since_ms,
                actor: proto_actor(&run.actor),
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
    /// recorded session, then SIGKILL), ends their runs and returns their
    /// summaries as `stopped`, keeping `orphaned_since_ms`.
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
                .map(|(_, orphan, _)| self.integ.processes.terminate(orphan.process, ORPHAN_GRACE)),
        )
        .await;
        let mut stopped = Vec::new();
        for (name, orphan, mut summary) in picked {
            let _ = self.store.finish_unit_run(orphan.run_id, TERMINATED).await;
            let mut st = lane.st();
            if st
                .orphans
                .get(&name)
                .is_some_and(|o| o.run_id == orphan.run_id)
            {
                st.orphans.remove(&name);
            }
            drop(st);
            tracing::info!(pid = orphan.process.pid, unit = %name, "terminated a previous daemon's unit");
            summary.state = "stopped".into();
            stopped.push(summary);
        }
        self.emit_lane(project, lane);
        self.publish_routes();
        stopped
    }

    /// Clears orphans whose process is gone (exited, or its pid reused).
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
                if self.integ.processes.alive(orphan.process).await {
                    continue;
                }
                let _ = self.store.finish_unit_run(orphan.run_id, GONE).await;
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
}

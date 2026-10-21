//! Local `services.<name>.sync` (plan 10.4): while a service unit runs, each of
//! its `sync` entries is mirrored from the worktree to its `to` folder on this
//! machine by a [`Syncer`]. The syncs start after the spawn and stop when the
//! unit's run ends, whatever ended it.

use std::sync::Arc;

use devlanes_resolve::SyncSpec;
use devlanes_sync::{LocalTarget, SyncHandle, SyncOptions, Syncer};
use serde_json::{Value, json};

use crate::{model::LaneRt, units::UnitHost};

/// One running (or failed) sync of a service unit, kept in its `UnitRec`.
pub(crate) struct LocalSync {
    /// Position within the service's `sync` list.
    pub index: usize,
    pub spec: SyncSpec,
    /// `None` when the sync could not start; `error` says why.
    pub handle: Option<SyncHandle>,
    pub error: Option<String>,
}

impl LocalSync {
    /// The `local` part of a `sync.status` item: destination and progress.
    pub(crate) fn status(&self) -> Value {
        match &self.handle {
            Some(handle) => json!({
                "to": self.spec.to,
                "status": handle.status(),
            }),
            None => json!({
                "to": self.spec.to,
                "error": self.error,
            }),
        }
    }
}

/// Starts the syncs of a service unit that was just spawned. Runs in its own
/// task, since the first copy of a large folder takes a while; the handles land
/// in the unit's record. A run that ended meanwhile gets its syncs stopped
/// right away, so none outlives the unit.
pub(crate) fn start(lane: Arc<LaneRt>, unit: String, host: UnitHost, specs: Vec<SyncSpec>) {
    if specs.is_empty() {
        return;
    }
    tokio::spawn(async move {
        let mut syncs = Vec::with_capacity(specs.len());
        for (index, spec) in specs.into_iter().enumerate() {
            let started = start_one(&spec).await;
            let (handle, error) = match started {
                Ok(handle) => (Some(handle), None),
                Err(error) => {
                    tracing::warn!(%unit, from = %spec.from.display(), to = %spec.to.display(), %error, "sync did not start");
                    (None, Some(error.to_string()))
                }
            };
            syncs.push(LocalSync {
                index,
                spec,
                handle,
                error,
            });
        }
        let left = {
            let mut st = lane.st();
            match st
                .units
                .get_mut(&unit)
                .filter(|rec| rec.host.same_run(&host) && host.is_alive())
            {
                Some(rec) => {
                    rec.syncs = syncs;
                    Vec::new()
                }
                None => syncs,
            }
        };
        stop_all(left).await;
    });
}

/// Copies the folder once and starts following it.
async fn start_one(spec: &SyncSpec) -> Result<SyncHandle, devlanes_sync::SyncError> {
    let target = LocalTarget::owned(&spec.to).await?;
    let options = SyncOptions {
        ignore: spec.ignore.clone(),
        ..SyncOptions::default()
    };
    Syncer::new(&spec.from, Arc::new(target), options)?
        .start()
        .await
}

/// Takes the syncs of the run `host` from its record and stops them. Called
/// once the run ended.
pub(crate) async fn stop(lane: &LaneRt, unit: &str, host: &UnitHost) {
    let syncs = {
        let mut st = lane.st();
        match st.units.get_mut(unit).filter(|rec| rec.host.same_run(host)) {
            Some(rec) => std::mem::take(&mut rec.syncs),
            None => Vec::new(),
        }
    };
    stop_all(syncs).await;
}

/// Stops each sync, letting it finish the file it is writing.
async fn stop_all(syncs: Vec<LocalSync>) {
    for sync in syncs {
        if let Some(handle) = sync.handle {
            handle.stop().await;
        }
    }
}

/// The `local` part of `sync.status` for entry `index` of `unit`, while its
/// sync runs or after it failed to start.
pub(crate) fn status(lane: &LaneRt, unit: &str, index: usize) -> Option<Value> {
    let st = lane.st();
    st.units
        .get(unit)?
        .syncs
        .iter()
        .find(|sync| sync.index == index)
        .map(LocalSync::status)
}

//! `daemon restart` brings back what ran: the lanes and their running services are
//! read before the old daemon stops, and started again on the new one with the
//! restarting client's environment, as `lane resume` would.

use std::collections::BTreeMap;

use devlanes_proto::{Actor, Context, LaneSummary, Method, Page, Selection, StartResult};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Client, Error, Result};

/// One lane that ran when the daemon restarted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunningLane {
    /// `project@index`.
    pub lane: String,
    /// The profiles of its last start, in order.
    pub profiles: Vec<String>,
    /// Its services that were starting, queued or up.
    pub units: Vec<String>,
}

/// Unit states that count as running for a restart.
const RUNNING: [&str; 4] = ["starting", "queued", "ready", "running"];

/// The lanes with running services, from `status --all`. Tasks, exec units and a
/// previous daemon's orphans are left out: they cannot be started again as they were.
pub fn running_lanes(lanes: &[LaneSummary]) -> Vec<RunningLane> {
    lanes
        .iter()
        .filter_map(|lane| {
            let units: Vec<String> = lane
                .units
                .iter()
                .filter(|unit| unit.kind == "service" && RUNNING.contains(&unit.state.as_str()))
                .map(|unit| unit.name.clone())
                .collect();
            (!units.is_empty()).then(|| RunningLane {
                lane: lane.reference.clone(),
                profiles: lane.profiles.clone(),
                units,
            })
        })
        .collect()
}

impl Client {
    /// [`running_lanes`] of every project this daemon knows.
    pub async fn running_lanes(&mut self) -> Result<Vec<RunningLane>> {
        let status = self
            .request(Method::Status, json!({"all": true}), Context::default())
            .await?;
        let page: Page<LaneSummary> = serde_json::from_value(status)
            .map_err(|error| Error::Startup(format!("unreadable status: {error}")))?;
        Ok(running_lanes(&page.items))
    }

    /// Starts each lane's services again with its profiles, the caller's `env` and
    /// `actor`. Returns one `StartResult` per lane, or `{lane, error}` for a lane
    /// whose start the daemon refused.
    pub async fn resume_running(
        &mut self,
        lanes: &[RunningLane],
        env: &BTreeMap<String, String>,
        actor: &Actor,
    ) -> Vec<Value> {
        let mut results = Vec::with_capacity(lanes.len());
        for lane in lanes {
            let selection = Selection {
                units: lane.units.clone(),
                profiles: lane.profiles.clone(),
                ..Selection::default()
            };
            let context = Context {
                lane: Some(lane.lane.clone()),
                env: Some(env.clone()),
                actor: actor.clone(),
                ..Context::default()
            };
            let params = serde_json::to_value(selection).unwrap_or(Value::Null);
            results.push(match self.request(Method::Start, params, context).await {
                Ok(value) => match serde_json::from_value::<StartResult>(value.clone()) {
                    Ok(_) => value,
                    Err(_) => json!({"lane": lane.lane, "result": value}),
                },
                Err(Error::Rpc(error)) => json!({"lane": lane.lane, "error": error}),
                Err(error) => json!({
                    "lane": lane.lane,
                    "error": {"code": "io", "data": {"message": error.to_string()}, "exit_code": 1},
                }),
            });
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_proto::UnitSummary;

    /// A unit summary with `kind` and `state`.
    fn unit(name: &str, kind: &str, state: &str) -> UnitSummary {
        UnitSummary {
            name: name.into(),
            kind: kind.into(),
            state: state.into(),
            ..UnitSummary::default()
        }
    }

    #[test]
    /// Only lanes with running services count, with those services and profiles.
    fn running_lanes_keep_services_and_profiles() {
        let lanes = vec![
            LaneSummary {
                reference: "acme@0".into(),
                profiles: vec!["backend".into()],
                units: vec![
                    unit("api", "service", "ready"),
                    unit("web", "service", "idle"),
                    unit("worker", "service", "starting"),
                    unit("migrate", "task", "running"),
                    unit("probe", "exec", "ready"),
                    unit("old", "service", devlanes_proto::ORPHANED),
                ],
                ..LaneSummary::default()
            },
            LaneSummary {
                reference: "acme@1".into(),
                units: vec![unit("api", "service", "stopped")],
                ..LaneSummary::default()
            },
        ];
        assert_eq!(
            running_lanes(&lanes),
            [RunningLane {
                lane: "acme@0".into(),
                profiles: vec!["backend".into()],
                units: vec!["api".into(), "worker".into()],
            }]
        );
    }
}

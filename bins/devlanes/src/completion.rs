//! Dynamic completion that asks an already-running daemon and never spawns one.

use std::{path::Path, sync::Arc, time::Duration};

use devlanes_cli::{CompletionKind, CompletionSource, complete_environment};
use devlanes_client::{Client, Paths};
use devlanes_proto::{Context, LaneSummary, Method, Page};
use serde_json::{Value, json};

/// Completion runs once per keypress; anything slower than this feels broken.
const BUDGET: Duration = Duration::from_millis(300);

struct DaemonCandidates;

impl CompletionSource for DaemonCandidates {
    /// Builds a tiny runtime per call because completion runs before main's runtime exists.
    fn existing_daemon_candidates(&self, kind: CompletionKind, _prefix: &str) -> Vec<String> {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            return Vec::new();
        };
        runtime
            .block_on(async { tokio::time::timeout(BUDGET, candidates(kind)).await.ok() })
            .flatten()
            .unwrap_or_default()
    }
}

/// Names from `items[].key` of a list result.
fn item_names(data: &Value, key: &str) -> Vec<String> {
    data.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| item.get(key).and_then(Value::as_str).map(str::to_owned))
        .collect()
}

/// The known lane whose worktree contains `cwd` (the deepest, for nested worktrees).
fn worktree_lane<'a>(lanes: &'a [LaneSummary], cwd: &Path) -> Option<&'a str> {
    lanes
        .iter()
        .filter(|lane| {
            !lane.path.as_os_str().is_empty()
                && devlanes_core::paths::path_starts_with(cwd, &lane.path)
        })
        .max_by_key(|lane| lane.path.as_os_str().len())
        .map(|lane| lane.reference.as_str())
}

/// Status (with `all` and no cwd) knows every lane and unit name; tasks come from the
/// lane of the current worktree, named by reference. No request carries a cwd, so
/// completing in a new repository never registers it as a project.
async fn candidates(kind: CompletionKind) -> Option<Vec<String>> {
    let paths = Paths::discover().ok()?;
    let mut client = Client::connect(&paths).await.ok()?;
    let mut names = if kind == CompletionKind::Host {
        let data = client
            .request(Method::HostList, json!({}), Context::default())
            .await
            .ok()?;
        item_names(&data, "host")
    } else {
        let data = client
            .request(Method::Status, json!({"all": true}), Context::default())
            .await
            .ok()?;
        let page: Page<LaneSummary> = serde_json::from_value(data).ok()?;
        let mut names = Vec::new();
        // Tasks and profiles belong to the worktree's lane, named by reference.
        if matches!(kind, CompletionKind::Task | CompletionKind::Profile) {
            let cwd = std::env::current_dir().ok()?;
            let lane = worktree_lane(&page.items, &cwd)?.to_owned();
            let context = Context {
                lane: Some(lane),
                ..Context::default()
            };
            let method = if kind == CompletionKind::Task {
                Method::TaskList
            } else {
                Method::ProfileList
            };
            let data = client.request(method, json!({}), context).await.ok()?;
            names = item_names(&data, "name");
        }
        for lane in &page.items {
            match kind {
                CompletionKind::Lane => names.push(lane.reference.clone()),
                CompletionKind::Project => names.push(lane.project.clone()),
                CompletionKind::Unit => {
                    for unit in &lane.units {
                        names.push(unit.name.clone());
                        names.push(format!("{}:{}", lane.reference, unit.name));
                    }
                }
                CompletionKind::Task | CompletionKind::Host | CompletionKind::Profile => {}
            }
        }
        names
    };
    names.sort();
    names.dedup();
    Some(names)
}

/// Installs the hook; returns normally unless the shell is asking for completions.
pub fn install() {
    complete_environment(Arc::new(DaemonCandidates));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tasks complete from the lane whose worktree holds the cwd.
    #[test]
    fn task_completion_uses_the_worktree_lane() {
        let lane = |reference: &str, path: &str| LaneSummary {
            reference: reference.into(),
            path: path.into(),
            ..LaneSummary::default()
        };
        let lanes = [
            lane("acme@0", "/work/acme"),
            lane("acme@1", "/work/acme/.wt/one"),
        ];
        assert_eq!(
            worktree_lane(&lanes, Path::new("/work/acme/.wt/one/src")),
            Some("acme@1")
        );
        assert_eq!(
            worktree_lane(&lanes, Path::new("/work/acme/src")),
            Some("acme@0")
        );
        assert_eq!(worktree_lane(&lanes, Path::new("/elsewhere")), None);
        let hosts = json!({"items": [{"host": "box"}, {"host": "gpu"}]});
        assert_eq!(item_names(&hosts, "host"), ["box", "gpu"]);
    }
}

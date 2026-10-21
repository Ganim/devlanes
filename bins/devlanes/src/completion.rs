//! Dynamic completion that asks an already-running daemon and never spawns one.

use std::{sync::Arc, time::Duration};

use devlanes_cli::{CompletionKind, CompletionSource, complete_environment};
use devlanes_client::{Client, Paths};
use devlanes_proto::{Context, LaneSummary, Method, Page};

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

/// Status is the one query that knows every lane and unit name.
async fn candidates(kind: CompletionKind) -> Option<Vec<String>> {
    let paths = Paths::discover().ok()?;
    let mut client = Client::connect(&paths).await.ok()?;
    let context = Context {
        cwd: std::env::current_dir().ok()?,
        ..Context::default()
    };
    let data = client
        .request(Method::Status, serde_json::json!({"all": true}), context)
        .await
        .ok()?;
    let page: Page<LaneSummary> = serde_json::from_value(data).ok()?;
    let mut names = Vec::new();
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
            CompletionKind::Task | CompletionKind::Host => {}
        }
    }
    names.sort();
    names.dedup();
    Some(names)
}

/// Installs the hook; returns normally unless the shell is asking for completions.
pub fn install() {
    complete_environment(Arc::new(DaemonCandidates));
}

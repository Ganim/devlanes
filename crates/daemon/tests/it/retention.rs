//! State retention from the user config, run with a fixed clock.

use std::time::Duration;

use devlanes_proto::Method;
use serde_json::json;

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

const CONFIG: &str = "project: demo\nservices:\n  worker: { run: \"echo hi; sleep\" }\n";

#[tokio::test(flavor = "multi_thread")]
/// Finished runs outlive `history.retention` only until a pass after it; the
/// pass is remembered in `doctor`.
async fn state_retention_follows_history_retention() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let mut user = devlanes_config::UserConfig::default();
        user.history.retention = devlanes_config::ConfigDuration(Duration::from_secs(86_400));
        // A run with younger log segments stays; the worker's output must age out too.
        user.logs.retention.days = 1;
        user.state.max_size = devlanes_config::ByteSize(64 << 20);
        let h = harness(
            &home,
            Setup {
                user: Some(user),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        let store = devlanes_state::Store::open(home.join("state.db"))
            .await
            .expect("store");
        let lane = store.lanes(None).await.expect("lanes")[0].id;
        let finished = eventually(Duration::from_secs(5), || async {
            let runs = store.unit_runs(lane, 10).await.ok()?;
            runs.iter()
                .all(|run| run.outcome.is_some())
                .then_some(runs.len())
        })
        .await
        .expect("runs finished");
        assert!(finished >= 1);

        let now = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_millis(),
        )
        .expect("ms");
        let early = h
            .engine
            .prune_state_at(now + 3_600_000)
            .await
            .expect("prune");
        assert_eq!(early.unit_runs, 0, "younger than a day");
        let late = h
            .engine
            .prune_state_at(now + 2 * 86_400_000)
            .await
            .expect("prune");
        assert_eq!(late.unit_runs, u64::try_from(finished).expect("count"));
        assert!(store.unit_runs(lane, 10).await.expect("runs").is_empty());

        let doctor = h
            .call(Method::Doctor, json!({}), cx(&main))
            .await
            .expect("doctor");
        let state = &doctor["state"];
        assert_eq!(state["budget_bytes"], 64 << 20);
        assert!(
            state["size_bytes"].as_u64().is_some_and(|b| b > 0),
            "{state}"
        );
        assert_eq!(state["last_prune"]["at_ms"], now + 2 * 86_400_000);
        assert_eq!(state["last_prune"]["unit_runs"], finished);
        assert_eq!(state["last_prune"]["budget_exceeded"], false);
        h.close().await;
    })
    .await
    .expect("timeout");
}

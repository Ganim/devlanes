//! Idle lanes: `debug freeze` and `debug thaw`, the idle timers, what blocks a
//! freeze, the proxy's wait for a thaw, and thawing before a stop.

use std::{path::Path, sync::atomic::Ordering, time::Duration};

use devlanes_config::{ConfigDuration, UserConfig};
use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::json;

use crate::{Harness, LIMIT, Setup, cx, eventually, harness, repo, temp};

const CONFIG: &str = r#"project: idle
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
services:
  api: { run: sleep }
  web: { run: sleep }
"#;

/// The same services without containers, for engines without infra.
const PLAIN: &str = r#"project: idle
services:
  api: { run: sleep }
  web: { run: sleep }
"#;

/// The lane visible at `cwd`.
async fn lane(h: &Harness, cwd: &Path) -> LaneSummary {
    let value = h
        .call(Method::Status, json!({}), cx(cwd))
        .await
        .expect("status");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items.into_iter().next().expect("lane")
}

/// The lane's power level word, `None` while it runs normally.
async fn level(h: &Harness, cwd: &Path) -> Option<String> {
    lane(h, cwd).await.power.map(|power| power.level)
}

/// User config with the given idle timers.
fn timers(throttle: Duration, freeze: Duration) -> UserConfig {
    let mut user = UserConfig::default();
    user.lanes.throttle_after = ConfigDuration(throttle);
    user.lanes.freeze_after = ConfigDuration(freeze);
    user
}

/// Power changes the fake runner saw, as `<Power>` words in order.
fn powers(h: &Harness) -> Vec<String> {
    h.runner
        .powers
        .entries()
        .iter()
        .filter_map(|entry| entry.split_once(' ').map(|(_, level)| level.to_owned()))
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
/// `debug freeze` suspends every service and pauses the lane's own
/// containers; status keeps `ready` and adds `power`; `debug thaw` undoes both
/// in order, containers first.
async fn debug_freeze_and_thaw_round_trip() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "main", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let frozen = h
            .call(Method::DebugFreeze, json!({}), cx(&main))
            .await
            .expect("freeze");
        assert_eq!(frozen["level"], "frozen");
        assert_eq!(powers(&h), ["Frozen", "Frozen"]);
        assert!(h.infra.calls.entries().contains(&"pause 0".to_owned()));
        let summary = lane(&h, &main).await;
        assert_eq!(
            summary.power.as_ref().map(|p| p.level.as_str()),
            Some("frozen")
        );
        for unit in summary.units.iter().filter(|u| u.kind == "service") {
            assert_eq!(unit.state, "ready", "{}", unit.name);
            assert_eq!(unit.power.as_deref(), Some("frozen"), "{}", unit.name);
            assert_eq!(unit.shown_state(), "frozen");
        }
        // A second freeze of a frozen lane changes nothing.
        h.call(Method::DebugFreeze, json!({}), cx(&main))
            .await
            .expect("freeze again");
        assert_eq!(powers(&h).len(), 2);

        let thawed = h
            .call(Method::DebugThaw, json!({}), cx(&main))
            .await
            .expect("thaw");
        assert_eq!(thawed["thawed"], true);
        assert_eq!(powers(&h), ["Frozen", "Frozen", "Normal", "Normal"]);
        let calls = h.infra.calls.entries();
        let pause = calls.iter().position(|c| c == "pause 0").expect("paused");
        assert!(
            calls[pause..].contains(&"unpause 0".to_owned()),
            "{calls:?}"
        );
        assert_eq!(level(&h, &main).await, None);
        let again = h
            .call(Method::DebugThaw, json!({}), cx(&main))
            .await
            .expect("thaw again");
        assert_eq!(again["thawed"], false);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A running `keep_awake` service keeps the whole lane awake; a lane without
/// running services has nothing to freeze.
async fn keep_awake_and_idle_lanes_refuse_a_freeze() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(
            &tmp.path,
            "main",
            "project: awake\nservices:\n  api: { run: sleep }\n  worker: { run: sleep, keep_awake: true }\n",
        );
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let idle = h
            .call(Method::DebugFreeze, json!({}), cx(&main))
            .await
            .expect_err("nothing runs");
        assert_eq!(idle.code, "lane_not_running");
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let refused = h
            .call(Method::DebugFreeze, json!({}), cx(&main))
            .await
            .expect_err("keep_awake");
        assert_eq!(refused.code, "lane_keep_awake");
        assert_eq!(refused.data["units"], json!(["worker"]));
        assert!(powers(&h).is_empty());
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Without activity a lane is throttled after `throttle_after` and frozen
/// after `freeze_after`; a client command thaws it and restarts the timers.
async fn idle_timers_throttle_then_freeze() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "main", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                // Three seconds between the two, so polling sees the throttle.
                user: Some(timers(Duration::from_secs(1), Duration::from_secs(4))),
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let throttled = eventually(Duration::from_secs(10), || async {
            (level(&h, &main).await.as_deref() == Some("throttled")).then_some(())
        })
        .await;
        assert!(throttled.is_some(), "throttled: {:?}", powers(&h));
        let frozen = eventually(Duration::from_secs(10), || async {
            (level(&h, &main).await.as_deref() == Some("frozen")).then_some(())
        })
        .await;
        assert!(frozen.is_some(), "frozen: {:?}", powers(&h));
        assert_eq!(
            powers(&h),
            ["Throttled", "Throttled", "Frozen", "Frozen"],
            "both services throttled, then frozen"
        );
        let summary = lane(&h, &main).await;
        let power = summary.power.expect("power");
        assert!(power.last_activity.is_some(), "{power:?}");

        // `start` of a running lane spawns nothing but is a command on it.
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start again");
        assert_eq!(level(&h, &main).await, None);
        assert_eq!(powers(&h)[4..], ["Normal", "Normal"]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// When the runner cannot throttle (Linux), idle lanes skip straight to frozen.
async fn lanes_freeze_when_throttling_is_unsupported() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "main", PLAIN);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                user: Some(timers(Duration::from_secs(1), Duration::from_secs(3))),
                ..Setup::default()
            },
        )
        .await;
        h.runner.no_throttle.store(true, Ordering::SeqCst);
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let frozen = eventually(Duration::from_secs(10), || async {
            (level(&h, &main).await.as_deref() == Some("frozen")).then_some(())
        })
        .await;
        assert!(frozen.is_some(), "frozen: {:?}", powers(&h));
        assert_eq!(powers(&h), ["Frozen", "Frozen"]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A proxied request to a frozen lane waits for the thaw, which records it as
/// proxy activity.
async fn proxy_requests_wait_for_the_thaw() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "main", PLAIN);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let reference = lane(&h, &main).await.reference;
        assert!(
            h.idle.request(&reference).is_none(),
            "awake lanes forward at once"
        );
        h.call(Method::DebugFreeze, json!({}), cx(&main))
            .await
            .expect("freeze");
        let wait = h
            .idle
            .request(&reference)
            .expect("a frozen lane is waited for");
        tokio::time::timeout(Duration::from_secs(10), wait)
            .await
            .expect("thawed");
        assert_eq!(level(&h, &main).await, None);
        assert_eq!(powers(&h), ["Frozen", "Frozen", "Normal", "Normal"]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Stopping a frozen lane thaws it first, containers included.
async fn stop_thaws_a_frozen_lane_first() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "main", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        h.call(Method::DebugFreeze, json!({}), cx(&main))
            .await
            .expect("freeze");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        assert_eq!(powers(&h), ["Frozen", "Frozen", "Normal", "Normal"]);
        assert!(h.infra.calls.entries().contains(&"unpause 0".to_owned()));
        let summary = lane(&h, &main).await;
        assert!(summary.power.is_none());
        assert!(
            summary.units.iter().all(|u| u.state != "ready"),
            "{:?}",
            summary.units
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

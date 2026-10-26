//! Several profiles per start, `profile.list`, `autostart` expressions and unit
//! colors.

use std::time::Duration;

use devlanes_proto::{LaneSummary, Method, Page, StartResult};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp, worktree};

const CONFIG: &str = r#"project: demo
tags:
  team: { color: bright-magenta }
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
profiles:
  front: [web]
  back: { include: [api], env: { MODE: back }, infra: false }
  quiet: { include: [web], infra: "${{ lane.index > 100 }}" }
services:
  web: { run: "printenv MODE; serve" }
  api: { run: serve, tags: [team] }
  worker: { run: serve }
tasks:
  migrate: { run: "echo migrated" }
"#;

/// The first lane visible from `cwd`.
async fn lane(h: &crate::Harness, cwd: &std::path::Path) -> LaneSummary {
    let value = h
        .call(Method::LaneList, json!({}), cx(cwd))
        .await
        .expect("lanes");
    let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
    page.items.into_iter().next().expect("lane")
}

/// Units of `summary` that are running or ready.
fn running(summary: &LaneSummary) -> Vec<&str> {
    summary
        .units
        .iter()
        .filter(|u| matches!(u.state.as_str(), "running" | "ready" | "starting"))
        .map(|u| u.name.as_str())
        .collect()
}

/// A harness over `home` with fake infra, for [`CONFIG`]'s container.
async fn with_infra(home: &std::path::Path) -> crate::Harness {
    harness(
        home,
        Setup {
            infra: true,
            ..Setup::default()
        },
    )
    .await
}

/// Number of `infra.up` calls so far.
fn ups(h: &crate::Harness) -> usize {
    h.infra
        .calls
        .entries()
        .iter()
        .filter(|c| c.starts_with("up "))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
/// `profiles` select the union of their services, merge env in order, keep infra
/// on unless every profile turns it off (a literal or an expression), and persist.
async fn several_profiles_select_merge_and_persist() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = harness(
            &home,
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;

        let listed = h
            .call(Method::ProfileList, json!({}), cx(&main))
            .await
            .expect("profile list");
        assert_eq!(listed["lane"], "demo@0");
        let names: Vec<&str> = listed["items"]
            .as_array()
            .expect("items")
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert_eq!(names, ["front", "back", "quiet"]);
        assert_eq!(
            listed["items"][1],
            json!({"name": "back", "include": ["api"], "exclude": [], "skips_infra": true, "is_default": false})
        );

        let unknown = h
            .call(Method::Start, json!({"profiles": ["nope"]}), cx(&main))
            .await
            .expect_err("unknown profile");
        assert_eq!(unknown.code, "unknown_profile");
        assert!(lane(&h, &main).await.profiles.is_empty());

        h.call(
            Method::Start,
            json!({"profile": "front", "profiles": ["back", "front"]}),
            cx(&main),
        )
        .await
        .expect("start");
        let summary = lane(&h, &main).await;
        assert_eq!(summary.profiles, ["front", "back"]);
        assert_eq!(summary.profile.as_deref(), Some("front"));
        let mut up = running(&summary);
        up.sort_unstable();
        assert_eq!(up, ["api", "web"]);
        assert_eq!(ups(&h), 1, "front keeps infra on");
        let logged = eventually(Duration::from_secs(5), || async {
            let page = h
                .call(Method::Logs, json!({"units": ["web"]}), cx(&main))
                .await
                .ok()?;
            page.to_string().contains("back").then_some(())
        })
        .await;
        assert!(logged.is_some(), "the back profile's env reaches web");
        h.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");

        for profile in ["back", "quiet"] {
            h.call(Method::Start, json!({"profiles": [profile]}), cx(&main))
                .await
                .expect("start");
            assert_eq!(ups(&h), 1, "{profile} turns infra off");
            h.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");
        }

        h.call(
            Method::Start,
            json!({"profiles": ["front", "back"]}),
            cx(&main),
        )
        .await
        .expect("start");
        // A daemon that stops without a `devlanes stop` keeps the lane's selection.
        h.close().await;
        let again = with_infra(&home).await;
        assert_eq!(lane(&again, &main).await.profiles, ["front", "back"]);
        again.close().await;
    })
    .await
    .expect("timeout");
}

/// Names of `summary`'s running units, sorted.
fn running_sorted(summary: &LaneSummary) -> Vec<&str> {
    let mut names = running(summary);
    names.sort_unstable();
    names
}

#[tokio::test(flavor = "multi_thread")]
/// A plain start after a daemon restart brings back the services that ran, with
/// their profiles: one added by name stays, one stopped by name does not, and a
/// `transient` start and stop (`devlanes with`) change nothing.
async fn plain_start_restores_what_ran_before_the_daemon_stopped() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let home = tmp.path.join("home");
        let h = with_infra(&home).await;
        h.call(Method::Start, json!({"profiles": ["front"]}), cx(&main))
            .await
            .expect("start front");
        h.call(
            Method::Start,
            json!({"units": ["api", "worker"]}),
            cx(&main),
        )
        .await
        .expect("start api and worker");
        h.call(Method::Stop, json!({"units": ["worker"]}), cx(&main))
            .await
            .expect("stop worker");
        h.call(Method::Restart, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("restart");
        h.call(
            Method::Start,
            json!({"units": ["worker"], "profiles": ["back"], "transient": true}),
            cx(&main),
        )
        .await
        .expect("transient start");
        h.call(
            Method::Stop,
            json!({"units": ["worker"], "transient": true}),
            cx(&main),
        )
        .await
        .expect("transient stop");
        let before = lane(&h, &main).await;
        assert_eq!(running_sorted(&before), ["api", "web"]);
        h.close().await;

        let again = with_infra(&home).await;
        assert!(lane(&again, &main).await.needs_env);
        let started: StartResult = serde_json::from_value(
            again
                .call(Method::Start, json!({}), cx(&main))
                .await
                .expect("plain start"),
        )
        .expect("start result");
        assert!(started.whole_lane);
        let restored = lane(&again, &main).await;
        assert_eq!(restored.profiles, ["front"]);
        assert_eq!(running_sorted(&restored), ["api", "web"]);
        again
            .call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        again.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// After a restart only the lane a plain start runs in comes back; the result
/// names the other lanes that ran for `lane resume`, which brings them back too.
async fn plain_start_names_other_lanes_to_resume() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let other = tmp.path.join("wt");
        worktree(&main, &other, "feat/other");
        let home = tmp.path.join("home");
        let h = with_infra(&home).await;
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        for cwd in [&main, &other] {
            h.call(Method::Start, json!({"units": ["web"]}), cx(cwd))
                .await
                .expect("start");
        }
        h.close().await;

        let again = with_infra(&home).await;
        let started = again
            .call(Method::Start, json!({}), cx(&main))
            .await
            .expect("plain start");
        let warning = started["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .find(|w| w["code"] == "lanes_to_resume")
            .cloned()
            .expect("lanes_to_resume");
        assert_eq!(warning["data"]["lanes"], json!(["demo@1"]));
        let second = || async {
            let value = again
                .call(Method::LaneList, json!({}), cx(&main))
                .await
                .expect("lanes");
            let page: Page<LaneSummary> = serde_json::from_value(value).expect("page");
            page.items
                .into_iter()
                .find(|lane| lane.reference == "demo@1")
                .expect("demo@1")
        };
        assert!(running(&second().await).is_empty());
        again
            .call(Method::Resume, json!({}), cx(&main))
            .await
            .expect("resume");
        assert_eq!(running(&second().await), ["web"]);
        for cwd in [&main, &other] {
            again
                .call(Method::Stop, json!({}), cx(cwd))
                .await
                .expect("stop");
        }
        again.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A lane stopped as a whole forgets its services and profiles: the next plain
/// start uses its default, as sending the default profile name does.
async fn whole_stop_resets_to_the_default() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        h.call(Method::Start, json!({"profiles": ["front"]}), cx(&main))
            .await
            .expect("start");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        assert!(lane(&h, &main).await.profiles.is_empty());
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let fresh = lane(&h, &main).await;
        assert!(fresh.profiles.is_empty());
        assert_eq!(
            running_sorted(&fresh),
            ["api", "web", "worker"],
            "every autostart service"
        );
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");

        h.call(
            Method::Start,
            json!({"profiles": [devlanes_proto::DEFAULT_PROFILE]}),
            cx(&main),
        )
        .await
        .expect("start");
        let reset = lane(&h, &main).await;
        assert_eq!(reset.profiles, ["default"]);
        let mut all = running(&reset);
        all.sort_unstable();
        assert_eq!(all, ["api", "web", "worker"], "every autostart service");
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A plain start settles an `autostart` expression per lane: `worker` starts in
/// lane 0 and stays idle in lane 1, and `why` agrees with what each start picked.
async fn autostart_expression_decides_per_lane() {
    const AUTOSTART: &str = "project: demo\nservices:\n  api: { run: serve }\n  worker: { run: serve, autostart: \"${{ lane.index == 0 }}\" }\n";
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", AUTOSTART);
        let other = tmp.path.join("wt");
        worktree(&main, &other, "feat/other");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        // Registers both lanes, so the main worktree gets index 0.
        h.call(Method::LaneList, json!({}), cx(&main))
            .await
            .expect("lane list");
        for (cwd, expected) in [
            (&main, &["demo@0:api", "demo@0:worker"][..]),
            (&other, &["demo@1:api"][..]),
        ] {
            let started: StartResult = serde_json::from_value(
                h.call(Method::Start, json!({}), cx(cwd))
                    .await
                    .expect("start"),
            )
            .expect("start result");
            assert_eq!(started.started, expected);
        }
        let reasons = |why: &Value| -> Vec<String> {
            why["reasons"]
                .as_array()
                .expect("reasons")
                .iter()
                .filter_map(|reason| reason["code"].as_str().map(str::to_owned))
                .collect()
        };
        let idle = h
            .call(Method::Why, json!({"unit": "worker"}), cx(&other))
            .await
            .expect("why in lane 1");
        assert_eq!(idle["lane"], "demo@1");
        assert!(
            reasons(&idle).contains(&"autostart_off".to_owned()),
            "{idle}"
        );
        // Stopped, lane 0's worker is idle too, yet still selected by a plain start.
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        let selected = h
            .call(Method::Why, json!({"unit": "worker"}), cx(&main))
            .await
            .expect("why in lane 0");
        assert_eq!(selected["lane"], "demo@0");
        let codes = reasons(&selected);
        assert!(
            !codes
                .iter()
                .any(|c| c == "autostart_off" || c == "not_in_profile"),
            "{selected}"
        );
        h.call(Method::Stop, json!({}), cx(&other))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Services (idle or running), tasks, exec units and containers carry a color.
async fn units_and_containers_have_colors() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let none = std::collections::BTreeMap::new();
        let color = |summary: &LaneSummary, unit: &str| {
            summary
                .units
                .iter()
                .find(|u| u.name == unit)
                .and_then(|u| u.color.clone())
        };

        let idle = lane(&h, &main).await;
        assert_eq!(color(&idle, "web").as_deref(), Some("green"));
        assert_eq!(color(&idle, "api").as_deref(), Some("bright-magenta"));

        h.call(Method::Start, json!({"units": ["api"]}), cx(&main))
            .await
            .expect("start");
        h.call(Method::Run, json!({"task": "migrate"}), cx(&main))
            .await
            .expect("run");
        let exec: Value = h
            .call(
                Method::Exec,
                json!({"name": "db-shell", "command": ["sleep"]}),
                cx(&main),
            )
            .await
            .expect("exec");
        assert_eq!(exec["color"], "cyan");

        let live = lane(&h, &main).await;
        assert_eq!(color(&live, "api").as_deref(), Some("bright-magenta"));
        assert_eq!(color(&live, "web").as_deref(), Some("green"));
        assert_eq!(
            color(&live, "migrate").as_deref(),
            Some(devlanes_core::color::unit_color(
                &[],
                "migrate",
                None,
                &none
            ))
        );
        assert_eq!(color(&live, "db-shell").as_deref(), Some("cyan"));
        let db = live
            .containers
            .iter()
            .find(|c| c.name == "db")
            .expect("db container");
        assert_eq!(db.color.as_deref(), Some("cyan"));
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

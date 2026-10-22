//! Several profiles per start, `profile.list`, and unit colors.

use std::time::Duration;

use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

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
        h.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");
        h.close().await;
        let again = harness(&home, Setup::default()).await;
        assert_eq!(lane(&again, &main).await.profiles, ["front", "back"]);
        again.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// An empty selection keeps the previous profiles (start and restart); sending
/// the default profile name resets the lane to its default selection.
async fn empty_keeps_and_default_resets_profiles() {
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
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let kept = lane(&h, &main).await;
        assert_eq!(kept.profiles, ["front"]);
        assert_eq!(running(&kept), ["web"]);
        h.call(Method::Restart, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("restart");
        assert_eq!(lane(&h, &main).await.profiles, ["front"]);
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

//! Lane events on the request that caused them (`"events": true`).

use std::collections::{BTreeSet, VecDeque};

use devlanes_proto::{ContainerPhase, Event, Method, PhaseContainer, StartResult, events};
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, drain_events, harness, repo, temp, worktree};

const CONFIG: &str = r#"project: demo
tasks:
  migrate: { run: "echo migrate", when: lane.start }
services:
  web:
    run: serve
  worker:
    run: "printenv RUN; sleep"
"#;

/// The lane an event is about, from any of the streamed shapes.
fn lane_of(event: &Event) -> Option<&str> {
    let data = &event.data;
    match event.event.as_str() {
        events::UNIT_CHANGED => data["unit"]["lane"].as_str(),
        events::LANE_CHANGED => data["lane"]["reference"].as_str(),
        _ => data["lane"].as_str(),
    }
}

/// `phase:state` of each `lane.phase` event.
fn phases(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter(|event| event.event == events::PHASE)
        .map(|event| {
            format!(
                "{}:{}",
                event.data["phase"].as_str().unwrap_or_default(),
                event.data["state"].as_str().unwrap_or_default()
            )
        })
        .collect()
}

/// The `start_id`s on `lane.phase` events.
fn start_ids(events: &[Event]) -> BTreeSet<u64> {
    events
        .iter()
        .filter(|event| event.event == events::PHASE)
        .filter_map(|event| event.data["start_id"].as_u64())
        .collect()
}

/// The `units` of the `services:started` event.
fn planned(events: &[Event]) -> Option<Vec<String>> {
    let event = events.iter().find(|event| {
        event.event == events::PHASE
            && event.data["phase"] == "services"
            && event.data["state"] == "started"
    })?;
    serde_json::from_value(event.data["units"].clone()).ok()
}

#[tokio::test(flavor = "multi_thread")]
/// A start streams its own phases, tasks and units before the response, and a
/// partial start plans only the units it spawns.
async fn a_start_streams_its_events_before_the_response() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;

        let (session, mut frames) = h.session();
        let value = h
            .call_on(Method::Start, json!({"events": true}), cx(&main), &session)
            .await
            .expect("start");
        let result: StartResult = serde_json::from_value(value).expect("result");
        assert!(result.ready, "{result:?}");
        let streamed = drain_events(&mut frames);
        assert_eq!(
            phases(&streamed),
            [
                "infra:started",
                "infra:skipped",
                "setup:started",
                "setup:skipped",
                "tasks:started",
                "tasks:progress",
                "tasks:done",
                "services:started",
                "services:done",
                "after-services:started",
                "after-services:skipped",
            ]
        );
        assert_eq!(
            planned(&streamed),
            Some(vec!["web".to_owned(), "worker".to_owned()])
        );
        let id = result.start_id.expect("start id");
        assert_eq!(start_ids(&streamed), BTreeSet::from([id]));
        assert!(
            streamed
                .iter()
                .any(|e| e.event == events::TASK && e.data["task"] == "migrate"),
            "{streamed:?}"
        );
        // Each unit's watcher task emits its transitions, so the last one may
        // trail the response; the first ones are long out by then.
        assert!(
            streamed
                .iter()
                .any(|e| e.event == events::UNIT_CHANGED && e.data["unit"]["name"] == "web"),
            "{streamed:?}"
        );

        // Without the flag nothing new arrives on the request.
        h.call(Method::Stop, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("stop web");
        let (session, mut frames) = h.session();
        h.call_on(
            Method::Start,
            json!({"units": ["web"]}),
            cx(&main),
            &session,
        )
        .await
        .expect("start web");
        assert!(drain_events(&mut frames).is_empty());

        // The worker ran before this partial start, so only web is its share.
        h.call(Method::Stop, json!({"units": ["web"]}), cx(&main))
            .await
            .expect("stop web");
        let (session, mut frames) = h.session();
        h.call_on(Method::Start, json!({"events": true}), cx(&main), &session)
            .await
            .expect("start rest");
        let streamed = drain_events(&mut frames);
        assert_eq!(planned(&streamed), Some(vec!["web".to_owned()]));
        // Unit changes are the planned units' only.
        assert!(
            !streamed
                .iter()
                .any(|e| e.event == events::UNIT_CHANGED && e.data["unit"]["name"] == "worker"),
            "{streamed:?}"
        );

        // A restart is a services phase of its own start.
        let (session, mut frames) = h.session();
        let value = h
            .call_on(
                Method::Restart,
                json!({"units": ["worker"], "events": true}),
                cx(&main),
                &session,
            )
            .await
            .expect("restart");
        let restart: StartResult = serde_json::from_value(value).expect("result");
        let streamed = drain_events(&mut frames);
        assert_eq!(phases(&streamed), ["services:started", "services:done"]);
        assert_eq!(planned(&streamed), Some(vec!["worker".to_owned()]));
        let restart_id = restart.start_id.expect("restart id");
        assert_ne!(restart_id, id);
        assert_eq!(start_ids(&streamed), BTreeSet::from([restart_id]));
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// Concurrent starts each get only their own lane's events and their own start's
/// phases, also when two of them start the same lane.
async fn concurrent_starts_do_not_mix() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let other = tmp.path.join("wt");
        worktree(&main, &other, "feat/other");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let start = |dir: &std::path::Path| {
            let (session, frames) = h.session();
            let dir = dir.to_path_buf();
            let h = &h;
            async move {
                let value = h
                    .call_on(Method::Start, json!({"events": true}), cx(&dir), &session)
                    .await
                    .expect("start");
                let mut frames = frames;
                let result: StartResult = serde_json::from_value(value).expect("result");
                (result, drain_events(&mut frames))
            }
        };
        let ((first, first_events), (second, second_events), (third, third_events)) =
            tokio::join!(start(&main), start(&main), start(&other));
        let ids: BTreeSet<Option<u64>> = [first.start_id, second.start_id, third.start_id]
            .into_iter()
            .collect();
        assert_eq!(ids.len(), 3, "start ids are distinct: {ids:?}");
        for (result, streamed) in [
            (&first, &first_events),
            (&second, &second_events),
            (&third, &third_events),
        ] {
            assert!(!streamed.is_empty());
            let lanes: BTreeSet<&str> = streamed.iter().filter_map(lane_of).collect();
            assert_eq!(
                lanes,
                BTreeSet::from([result.lane.as_str()]),
                "{streamed:?}"
            );
            assert_eq!(
                start_ids(streamed),
                result.start_id.into_iter().collect::<BTreeSet<_>>()
            );
        }
        assert_ne!(first.lane, third.lane);
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `wait` on a unit begins with the unit as it is, and streams only that unit.
async fn wait_streams_its_unit() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let (session, mut frames) = h.session();
        let value = h
            .call_on(
                Method::Wait,
                json!({"target": "web", "events": true}),
                cx(&main),
                &session,
            )
            .await
            .expect("wait");
        assert_eq!(value["ready"], Value::Bool(true));
        let streamed = drain_events(&mut frames);
        let names: Vec<&str> = streamed
            .iter()
            .filter_map(|e| e.data["unit"]["name"].as_str())
            .collect();
        assert_eq!(names, ["web"], "{streamed:?}");
        h.close().await;
    })
    .await
    .expect("timeout");
}

const STEPS: &str = r#"project: steps
infra:
  containers:
    db: { image: "postgres:16", ports: { pg: 5432 }, per_lane: true }
  setup:
    database: "echo setup-db"
    vhost: "echo setup-vhost"
tasks:
  build: { run: "echo task-build", when: lane.create }
  migrate: { run: "echo task-migrate", when: lane.start }
services:
  api:
    run: sleep
"#;

/// The `progress` events of `phase`.
fn progress<'a>(events: &'a [Event], phase: &str) -> Vec<&'a Value> {
    events
        .iter()
        .filter(|e| {
            e.event == events::PHASE && e.data["phase"] == phase && e.data["state"] == "progress"
        })
        .map(|e| &e.data)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
/// While infra comes up, each change in its containers is one `progress` event
/// (repeats are not); setup and tasks report the step that runs and its place.
async fn running_phases_report_their_detail() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", STEPS);
        let h = harness(
            &tmp.path.join("home"),
            Setup {
                infra: true,
                ..Setup::default()
            },
        )
        .await;
        let db = |state: ContainerPhase, health: Option<&str>| {
            vec![PhaseContainer {
                name: "db".into(),
                state,
                health: health.map(str::to_owned),
            }]
        };
        *h.infra.progress.lock().expect("script") = VecDeque::from([
            db(ContainerPhase::Creating, None),
            db(ContainerPhase::WaitingHealthy, Some("starting")),
            db(ContainerPhase::WaitingHealthy, Some("starting")),
            db(ContainerPhase::Healthy, Some("healthy")),
        ]);
        let (session, mut frames) = h.session();
        // The harness's 10 s start timeout covers the whole start, and infra alone
        // takes 3 s here (one look per second until the last snapshot). On a busy
        // Windows runner the git calls and spawns around it took the rest, so the
        // service spawned after the deadline and the start reported `timed_out`.
        // This test is about the events, so only `LIMIT` bounds the start.
        let value = h
            .call_on(
                Method::Start,
                json!({"events": true, "timeout_ms": LIMIT.as_secs() * 1000}),
                cx(&main),
                &session,
            )
            .await
            .expect("start");
        let result: StartResult = serde_json::from_value(value).expect("result");
        assert!(result.ready, "{result:?}");
        let streamed = drain_events(&mut frames);

        let infra: Vec<Value> = progress(&streamed, "infra")
            .iter()
            .map(|data| data["containers"][0]["state"].clone())
            .collect();
        assert_eq!(
            infra,
            [
                json!("creating"),
                json!("waiting_healthy"),
                json!("healthy")
            ]
        );
        assert!(
            progress(&streamed, "infra")
                .iter()
                .all(|data| data["start_id"] == json!(result.start_id)),
        );

        let setup: Vec<(u64, u64)> = progress(&streamed, "setup")
            .iter()
            .map(|data| {
                let step = &data["step"];
                (
                    step["index"].as_u64().unwrap_or_default(),
                    step["count"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        assert_eq!(setup, [(1, 2), (2, 2)], "{streamed:?}");

        let tasks: Vec<String> = progress(&streamed, "tasks")
            .iter()
            .map(|data| {
                let step = &data["step"];
                format!("{} {}/{}", step["name"], step["index"], step["count"])
            })
            .collect();
        assert_eq!(tasks, [r#""build" 1/2"#, r#""migrate" 2/2"#]);
        h.close().await;
    })
    .await
    .expect("timeout");
}

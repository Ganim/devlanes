//! `run` of a detached task: what the client attaches to right after the answer.

use std::{sync::Arc, time::Duration};

use devlanes_daemon::{
    Engine, EngineOptions, Handler,
    integrations::{CoreRunner, Integrations},
};
use devlanes_ipc::Frame;
use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{LIMIT, Setup, cx, harness, repo, temp};

/// The pid of `unit` in the cwd's lane.
async fn pid_of(h: &crate::Harness, main: &std::path::Path, unit: &str) -> Option<u32> {
    let status = h.call(Method::Status, json!({}), cx(main)).await.ok()?;
    let lanes: Page<LaneSummary> = serde_json::from_value(status).ok()?;
    lanes.items[0].units.iter().find(|u| u.name == unit)?.pid
}

#[tokio::test(flavor = "multi_thread")]
/// A detached run answers once its own run is listed, not an earlier one that
/// ended, so a client attaching right away reaches the new run.
async fn a_detached_rerun_answers_with_the_new_run() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(
            &tmp.path,
            "r",
            "project: demo\ntasks:\n  hello: { run: \"echo hi\" }\n",
        );
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let detached = json!({"task": "hello", "detach": true, "terminal": true});
        h.call(Method::Run, detached.clone(), cx(&main))
            .await
            .expect("first run");
        let first = crate::eventually(Duration::from_secs(5), || async {
            pid_of(&h, &main, "hello").await
        })
        .await
        .expect("first run listed");
        h.call(Method::Run, detached, cx(&main))
            .await
            .expect("second run");
        let second = pid_of(&h, &main, "hello").await.expect("listed");
        assert_ne!(
            second, first,
            "the answer came after the new run was listed"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// With the real runner, a pipe task's first line is on the attach screen within
/// a second of the `run` answer.
async fn a_pipe_task_shows_its_output_right_away() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(
            &tmp.path,
            "r",
            "project: piped\ntasks:\n  slow: { run: \"echo started; sleep 5; echo done\" }\n",
        );
        let home = tmp.path.join("home");
        std::fs::create_dir_all(&home).expect("home");
        let store = devlanes_state::Store::open(home.join("state.db"))
            .await
            .expect("store");
        let integ = Integrations::with_runner(Arc::new(CoreRunner(
            devlanes_runner_pty::PtyRunner::default(),
        )));
        let mut options = EngineOptions::new(&home, home.join("d.sock"));
        options.watch = false;
        let engine = Engine::new(options, store, integ, CancellationToken::new());
        engine.reconcile().await.expect("reconcile");
        let (session, _frames) = devlanes_daemon::Session::local(1);
        let request = |id, method, params| devlanes_proto::Request {
            id,
            method,
            params,
            context: cx(&main),
        };
        let asked = std::time::Instant::now();
        engine
            .handle(
                request(
                    1,
                    Method::Run,
                    json!({"task": "slow", "detach": true, "terminal": true}),
                ),
                session.clone(),
            )
            .await
            .expect("run");
        let answered = asked.elapsed();
        let (viewer, mut frames) = devlanes_daemon::Session::local(2);
        engine
            .handle(
                request(
                    2,
                    Method::Attach,
                    json!({"unit": "slow", "interactive": false, "columns": 80, "rows": 20}),
                ),
                viewer.clone(),
            )
            .await
            .expect("attach");
        let mut screen = String::new();
        let shown = tokio::time::timeout(Duration::from_secs(1), async {
            while let Some(frame) = frames.recv().await {
                if let Frame::Bytes { data, .. } = frame {
                    screen.push_str(&String::from_utf8_lossy(&data));
                    if screen.contains("started") {
                        return;
                    }
                }
            }
        })
        .await;
        assert!(
            shown.is_ok(),
            "no output within 1 s of the answer: {screen:?}"
        );
        assert!(
            answered < Duration::from_secs(2),
            "answered after {answered:?}"
        );
        viewer.close();
        engine
            .shutdown(crate::signal_shutdown())
            .await
            .expect("shutdown");
    })
    .await
    .expect("timeout");
}

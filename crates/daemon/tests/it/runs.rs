//! `run` of a detached task: what the client attaches to right after the answer.

use std::{sync::Arc, time::Duration};

use devlanes_daemon::{
    Engine, EngineOptions, Handler,
    integrations::{CoreRunner, Integrations, detect_shell},
};
use devlanes_ipc::Frame;
use devlanes_proto::{LaneSummary, Method, Page};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::{LIMIT, Setup, cx, harness, repo, temp};

/// `(pid, state)` of `unit` in the cwd's lane. The fake runner hands out each
/// pid once (a counter), so the pid names the run.
async fn run_of(h: &crate::Harness, main: &std::path::Path, unit: &str) -> Option<(u32, String)> {
    let status = h.call(Method::Status, json!({}), cx(main)).await.ok()?;
    let lanes: Page<LaneSummary> = serde_json::from_value(status).ok()?;
    let unit = lanes.items[0].units.iter().find(|u| u.name == unit)?;
    Some((unit.pid?, unit.state.clone()))
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
        // The earlier run has ended, as when someone runs a task again.
        let (first, _) = crate::eventually(Duration::from_secs(5), || async {
            run_of(&h, &main, "hello")
                .await
                .filter(|(_, state)| state != "running" && state != "starting")
        })
        .await
        .expect("first run ended");
        h.call(Method::Run, detached, cx(&main))
            .await
            .expect("second run");
        let (second, _) = run_of(&h, &main, "hello").await.expect("listed");
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
/// A detached run of a task whose previous run still goes answers with
/// `unit_running` instead of a success for a run that never started.
async fn a_detached_run_over_a_running_one_is_refused() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(
            &tmp.path,
            "r",
            "project: demo\ntasks:\n  hello: { run: \"echo hi; hold\" }\n",
        );
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let detached = json!({"task": "hello", "detach": true, "terminal": true});
        h.call(Method::Run, detached.clone(), cx(&main))
            .await
            .expect("first run");
        let (first, _) = crate::eventually(Duration::from_secs(5), || async {
            run_of(&h, &main, "hello").await
        })
        .await
        .expect("first run listed");
        let refused = h
            .call(Method::Run, detached, cx(&main))
            .await
            .expect_err("still running");
        assert_eq!(refused.code, "unit_running");
        assert_eq!(run_of(&h, &main, "hello").await.map(|r| r.0), Some(first));
        h.runner.release();
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// The runner the daemon uses on this OS.
#[cfg(unix)]
fn real_runner() -> devlanes_runner_pty::PtyRunner {
    devlanes_runner_pty::PtyRunner::default()
}

/// The runner the daemon uses on this OS, with a console helper that always
/// fails, so a stop terminates at once: these tests stop nothing gracefully.
#[cfg(windows)]
fn real_runner() -> devlanes_runner_windows::WindowsRunner {
    devlanes_runner_windows::WindowsRunner::new(devlanes_runner_windows::ConsoleHelper {
        program: std::env::var_os("ComSpec")
            .unwrap_or_else(|| "cmd.exe".into())
            .into(),
        args: ["/d", "/c", "exit 1"].map(Into::into).into(),
    })
}

/// How soon after the `run` answer the task's first line must be on screen.
#[cfg(unix)]
const FIRST_OUTPUT: Duration = Duration::from_secs(1);

/// How soon after the `run` answer the task's first line must be on screen.
/// Git's `sh.exe` is a launcher that starts the MSYS2 shell, two process starts
/// that took up to 0.9 s on a loaded machine. Output that waited for the exit
/// would still come only after the 5 s sleep.
#[cfg(windows)]
const FIRST_OUTPUT: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread")]
/// With the real runner, a pipe task's first line is on the attach screen within
/// [`FIRST_OUTPUT`] of the `run` answer.
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
        let mut integ = Integrations::with_runner(Arc::new(CoreRunner(real_runner())));
        // The `run:` string is POSIX; on Windows that needs Git's `sh.exe`.
        integ.shell = detect_shell().await;
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
        // The first request for a cwd discovers its project through several `git`
        // processes, which take seconds on a loaded Windows machine. The bounds
        // below are about the run, so that happens before the clock starts.
        engine
            .handle(request(0, Method::Status, json!({})), session.clone())
            .await
            .expect("status");
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
        let shown = tokio::time::timeout(FIRST_OUTPUT, async {
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
            "no output within {FIRST_OUTPUT:?} of the answer: {screen:?}"
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

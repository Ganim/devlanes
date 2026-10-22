//! `devlanes start` running inside the TUI: the phase line, the result, the
//! focus on the started lane, and the exit status when the user quits.

use std::{sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::{Event, events};
use devlanes_tui::{
    DaemonApi, Options,
    app::{App, Msg, Selected, Target},
    drive_start,
    start::{StartExit, StartFrame, StartHandoff},
};
use futures_util::stream;
use ratatui::Terminal;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::errors::{render, row};
use crate::fake::{FakeApi, mixed_lanes};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A frame carrying a `lane.phase` event for `acme@1`.
fn phase(phase: &str, state: &str) -> StartFrame {
    StartFrame::Event(Event {
        seq: 1,
        event: events::PHASE.into(),
        data: json!({"lane": "acme@1", "phase": phase, "state": state, "units": ["api"]}),
    })
}

/// A start result for `acme@1` whose `api` failed with log lines.
fn failed_result() -> Value {
    json!({
        "lane": "acme@1",
        "ready": false,
        "units": [{"name": "api", "lane": "acme@1", "state": "failed", "kind": "service",
                   "pid": null, "exit_code": 1, "ports": {}, "urls": [],
                   "actor": {"kind": "human"}, "reason": null}],
        "failures": [{"lane": "acme@1", "unit": "api", "exit_code": 1,
                      "log_tail": ["listening on 4000", "error: DATABASE_URL is not set"]}]
    })
}

/// The header's first row.
fn header(app: &App) -> Result<String, Box<dyn std::error::Error>> {
    Ok(row(&render(app)?, 0))
}

#[tokio::test]
async fn the_header_follows_the_start_and_the_failure_lands_on_its_unit() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(100, 24);
    app.open_start("start".into());
    // The first frames arrive before the lanes are listed.
    app.update(Msg::Start(phase("infra", "started")));
    app.update(Msg::Status(api.status().await));
    assert!(header(&app)?.contains("starting acme@1: containers"));
    // The started lane shows on its whole-lane row, with the phase list.
    assert_eq!(app.selected, Some(Selected::lane("acme@1")));
    assert!(app.phases.contains_key("acme@1"));
    app.update(Msg::Start(phase("infra", "done")));
    app.update(Msg::Start(StartFrame::Event(Event {
        seq: 2,
        event: events::TASK.into(),
        data: json!({"lane": "acme@1", "task": "schema", "state": "started"}),
    })));
    app.update(Msg::Start(phase("setup", "started")));
    assert!(header(&app)?.contains("starting acme@1: setup"));
    app.update(Msg::Start(phase("setup", "done")));
    app.update(Msg::Start(phase("services", "started")));
    assert!(header(&app)?.contains("starting acme@1: 0 of 1 services ready"));
    assert_eq!(app.start_exit(), Some(StartExit::Pending));

    app.update(Msg::Start(StartFrame::Done(Ok(failed_result()))));
    assert_eq!(app.start_exit(), Some(StartExit::Failed));
    let entry = app
        .target_error(&Target::Unit {
            lane: "acme@1".into(),
            unit: "api".into(),
        })
        .ok_or("no error on the unit")?;
    assert_eq!(entry.what, "start acme@1:api");
    assert_eq!(entry.worded.summary, "exited with code 1");
    assert!(
        entry
            .worded
            .detail
            .contains(&"error: DATABASE_URL is not set".to_owned())
    );
    // The selection had not moved, so the failed unit is shown.
    assert_eq!(app.selected, Some(Selected::unit("acme@1", "api")));
    // The result line stays once the error notice is gone.
    app.notice = None;
    assert!(
        header(&app)?.contains("✕ acme@1: api failed, 0 of 1 services ready"),
        "{}",
        header(&app)?
    );
    Ok(())
}

#[tokio::test]
async fn a_successful_start_ends_ready() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(100, 24);
    app.update(Msg::Status(api.status().await));
    app.open_start("start".into());
    app.update(Msg::Start(phase("services", "started")));
    let ready = json!({"lane": "acme@1", "ready": true, "units": [
        {"name": "api", "lane": "acme@1", "state": "ready", "kind": "service", "pid": 7,
         "exit_code": null, "ports": {}, "urls": [], "actor": {"kind": "human"}, "reason": null}]});
    app.update(Msg::Start(StartFrame::Done(Ok(ready))));
    assert_eq!(app.start_exit(), Some(StartExit::Ready));
    assert!(header(&app)?.contains("acme@1 up: 1 of 1 services ready"));
    assert!(app.errors.is_empty());
    Ok(())
}

/// Runs the loop on a start whose remaining frames are `frames`, waits until
/// `shown` (what the last frame puts in the header) is drawn, then presses
/// `quit`. The key and start streams stay open until the loop ended, so it
/// ends on the key alone.
async fn run(
    frames: Vec<StartFrame>,
    shown: &str,
    quit: KeyEvent,
) -> Result<(StartExit, Arc<FakeApi>), Box<dyn std::error::Error>> {
    let api = Arc::new(FakeApi::with_lanes(mixed_lanes()));
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let keys = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let (start_tx, start_rx) = mpsc::channel(16);
    let handoff = StartHandoff {
        label: "start".into(),
        frames: vec![phase("infra", "started")],
        stream: start_rx,
    };
    let backend = crate::SharedBackend::new(100, 24);
    let script = {
        let backend = backend.clone();
        async move {
            for frame in frames {
                start_tx.send(frame).await.ok();
            }
            backend.wait_for(shown).await;
            keys_tx.send(Ok(TermEvent::Key(quit))).await.ok();
            (keys_tx, start_tx)
        }
    };
    let mut terminal = Terminal::new(backend)?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    // q closes at once; the quit dialog has its own tests.
    let options = Options {
        on_quit: Some("keep".into()),
        ..crate::hermetic_options()
    };
    let run = drive_start(
        &mut terminal,
        driver_api,
        &options,
        keys,
        Box::new(std::io::sink()),
        handoff,
    );
    let (exit, _streams) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    Ok((exit?, api))
}

#[tokio::test]
async fn quitting_after_a_failure_reports_it_and_stops_nothing() -> TestResult {
    let q = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
    let (exit, api) = run(
        vec![StartFrame::Done(Ok(failed_result()))],
        "✕ start acme@1:api failed",
        q,
    )
    .await?;
    assert_eq!(exit, StartExit::Failed);
    let calls = api.calls();
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("Stop") || c.starts_with("Restart")),
        "{calls:?}"
    );
    Ok(())
}

#[tokio::test]
async fn ctrl_c_before_the_result_leaves_the_start_pending() -> TestResult {
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    let (exit, api) = run(
        vec![phase("services", "started")],
        "starting acme@1: 0 of 1 services",
        ctrl_c,
    )
    .await?;
    assert_eq!(exit, StartExit::Pending);
    assert!(!api.calls().iter().any(|c| c.starts_with("Stop")));
    Ok(())
}

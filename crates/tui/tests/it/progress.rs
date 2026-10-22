//! The tab's progress bar while lanes start.

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::events;
use devlanes_term::{Progress, Wrap};
use devlanes_tui::{
    DaemonApi, Options,
    app::{App, Msg},
    drive,
};
use futures_util::stream;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tokio::sync::mpsc;

use crate::fake::{FakeApi, lane, mixed_lanes, unit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Terminal output the test reads after the loop ended.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("poisoned"))?
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn text(&self) -> String {
        self.0
            .lock()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    /// Waits up to 4 s until `text` was written after byte `from`.
    async fn wait_for(&self, from: usize, text: &str) {
        for _ in 0..400 {
            if self
                .text()
                .get(from..)
                .is_some_and(|after| after.contains(text))
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// `acme@1` starting with one of its two services ready.
fn half_started() -> devlanes_proto::LaneSummary {
    lane(
        "acme",
        Some(1),
        "feat",
        "starting",
        vec![
            unit("acme@1", "api", "ready"),
            unit("acme@1", "web", "starting"),
        ],
    )
}

#[tokio::test]
async fn the_bar_follows_starting_lanes_and_errors_briefly() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(100, 24);
    app.update(Msg::Status(api.status().await));
    // Nothing starting in the fake's lanes: no bar.
    assert_eq!(app.start_progress(), Progress::Clear);
    let changed = |lane| {
        Msg::Event(devlanes_proto::Event {
            seq: 2,
            event: events::LANE_CHANGED.into(),
            data: json!({ "lane": lane }),
        })
    };
    app.update(changed(half_started()));
    assert_eq!(app.start_progress(), Progress::Percent(50));
    let phase = |state| {
        Msg::Event(devlanes_proto::Event {
            seq: 3,
            event: events::PHASE.into(),
            data: json!({"lane": "acme@1", "phase": "services", "state": state}),
        })
    };
    app.update(phase("failed"));
    assert!(matches!(app.start_progress(), Progress::Error(_)));
    let mut failed = half_started();
    failed.state = "failed".into();
    app.update(changed(failed));
    // Red for three ticks, then gone.
    for _ in 0..2 {
        app.update(Msg::Tick);
        assert!(matches!(app.start_progress(), Progress::Error(_)));
    }
    app.update(Msg::Tick);
    assert_eq!(app.start_progress(), Progress::Clear);
    Ok(())
}

#[tokio::test]
async fn the_loop_writes_the_bar_and_clears_it_on_quit() -> TestResult {
    let api = Arc::new(FakeApi::with_lanes(mixed_lanes()));
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let events = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let out = Captured::default();
    let script = {
        let api = Arc::clone(&api);
        let out = out.clone();
        async move {
            for _ in 0..100 {
                if api
                    .emit(events::LANE_CHANGED, json!({ "lane": half_started() }))
                    .await
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Quit once the bar for the starting lane was written.
            out.wait_for(0, "\x1b]9;4;1;50\x1b\\").await;
            let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
            keys_tx.send(Ok(TermEvent::Key(quit))).await.ok();
        }
    };
    // q closes at once; the quit dialog has its own tests.
    let options = Options {
        progress: Some(Wrap::Bare),
        on_quit: Some("keep".into()),
        ..crate::hermetic_options()
    };
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        events,
        Box::new(out.clone()),
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    result?;
    let text = out.text();
    assert!(text.contains("\x1b]9;4;1;50\x1b\\"), "{text:?}");
    // Quitting while the lane still starts leaves no bar in the tab.
    assert!(text.ends_with("\x1b]9;4;0;0\x1b\\"), "{text:?}");
    Ok(())
}

#[tokio::test]
async fn the_bar_survives_switching_lanes_and_a_resync_during_a_start() -> TestResult {
    // acme@1 starts api and web; docs ran already and is not the start's.
    let mut starting = half_started();
    starting.units.push(unit("acme@1", "docs", "ready"));
    let lanes: Vec<_> = mixed_lanes()
        .into_iter()
        .map(|l| {
            if l.reference == "acme@1" {
                starting.clone()
            } else {
                l
            }
        })
        .collect();
    let api = Arc::new(FakeApi::with_lanes(lanes));
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(16);
    let events = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let out = Captured::default();
    let script = {
        let api = Arc::clone(&api);
        let out = out.clone();
        async move {
            let plan = json!({"lane": "acme@1", "phase": "services", "state": "started",
                "start_id": 4, "units": ["api", "web"]});
            for _ in 0..100 {
                if api.emit(events::PHASE, plan.clone()).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // The plan is in: one of its two units is ready.
            out.wait_for(0, "\x1b]9;4;1;50\x1b\\").await;
            let press = |c| {
                Ok(TermEvent::Key(KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                )))
            };
            // Back and forth between the lanes while it starts, and a resync
            // (the subscriber fell behind) in the middle; its status reply is
            // waited for.
            for c in [']', '[', ']', '['] {
                keys_tx.send(press(c)).await.ok();
            }
            let statuses = |n: usize| {
                move |calls: &[String]| calls.iter().filter(|c| *c == "status").count() >= n
            };
            api.emit(events::RESYNC, json!({})).await;
            crate::wait_for_calls(&api, statuses(2)).await;
            // The start's next event, then a second resync: events are handled
            // in order, so its status call means the phase event was handled.
            api.emit(
                events::PHASE,
                json!({"lane": "acme@1", "phase": "services", "state": "progress", "start_id": 4}),
            )
            .await;
            api.emit(events::RESYNC, json!({})).await;
            crate::wait_for_calls(&api, statuses(3)).await;
            // A key after all that writes the bar again, with the start's
            // own share: one of its two units, not two of three.
            let before = out.text().len();
            keys_tx.send(press(']')).await.ok();
            out.wait_for(before, "\x1b]9;4;1;").await;
            let after = out.text()[before..].to_owned();
            keys_tx.send(press('q')).await.ok();
            after
        }
    };
    let options = Options {
        progress: Some(Wrap::Bare),
        on_quit: Some("keep".into()),
        ..crate::hermetic_options()
    };
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        events,
        Box::new(out.clone()),
    );
    let (result, after) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    result?;
    assert!(after.contains("\x1b]9;4;1;50\x1b\\"), "{after:?}");
    // Before the plan arrives the lane's three units count (66%); once it
    // has, nothing (lane switches, the resync) brings that back.
    let text = out.text();
    let planned = text.find("\x1b]9;4;1;50").ok_or("no 50%")?;
    assert!(!text[planned..].contains("\x1b]9;4;1;66"), "{text:?}");
    Ok(())
}

#[tokio::test]
async fn without_support_nothing_is_written() -> TestResult {
    let api = Arc::new(FakeApi::with_lanes(vec![half_started()]));
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let events = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let quit = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        keys_tx.send(Ok(TermEvent::Key(quit))).await.ok();
    };
    let out = Captured::default();
    let options = crate::hermetic_options();
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        events,
        Box::new(out.clone()),
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    result?;
    assert_eq!(out.text(), "");
    Ok(())
}

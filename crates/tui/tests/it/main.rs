//! TUI tests against a fake daemon: rendering snapshots, key handling and resync.

mod daemon;
mod errors;
mod fake;

use std::{sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::{Method, events};
use devlanes_tui::{
    DaemonApi, Options, UnitMetrics,
    app::{App, Connection, Dialog, Effect, Focus, Msg, Tab, Target},
    drive, ui,
};
use fake::{FakeApi, mixed_lanes};
use futures_util::stream;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tokio::sync::mpsc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Draws the app into a fresh test terminal.
fn draw(app: &App) -> Result<Terminal<TestBackend>, Box<dyn std::error::Error>> {
    let mut terminal = Terminal::new(TestBackend::new(app.size.0, app.size.1))?;
    terminal.draw(|frame| ui::render(app, frame))?;
    Ok(terminal)
}

/// An app that has loaded `api`'s status.
async fn loaded(api: &FakeApi, width: u16, height: u16) -> (App, Vec<Effect>) {
    let mut app = App::new(width, height);
    let effects = app.update(Msg::Status(api.status().await));
    (app, effects)
}

/// A key press without modifiers.
fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Ctrl+`c`.
fn ctrl(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL))
}

/// Shift+Tab, the focus key; crossterm reports it as `BackTab`.
fn shift_tab() -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
}

/// Presses `j` until the selected unit is `name`; returns the last attach generation.
fn select_unit(app: &mut App, name: &str) -> Option<u64> {
    let mut generation = None;
    for _ in 0..20 {
        if app.selected_unit().is_some_and(|u| u.name == name) {
            return generation;
        }
        for effect in app.update(key(KeyCode::Char('j'))) {
            if let Effect::Attach { generation: g, .. } = effect {
                generation = Some(g);
            }
        }
    }
    None
}

/// Marks the attach stream open and feeds it terminal output.
fn feed(app: &mut App, generation: u64, bytes: &[u8]) {
    app.update(Msg::AttachOpened {
        generation,
        result: Ok(devlanes_proto::AttachResult {
            stream_id: 1,
            unit: String::new(),
            lane: String::new(),
        }),
    });
    app.update(Msg::AttachOutput {
        generation,
        bytes: bytes.to_vec(),
    });
}

// ---- snapshots ---------------------------------------------------------------

#[tokio::test]
async fn snapshot_mixed_lanes_with_live_unit() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(
        &mut app,
        generation,
        b"\x1b[1mapi\x1b[0m listening on :20001\r\nGET /health 200\r\n",
    );
    insta::assert_snapshot!("mixed_lanes", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_failed_unit() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 20).await;
    let generation = select_unit(&mut app, "worker").ok_or("worker not reachable")?;
    feed(
        &mut app,
        generation,
        b"Error: connect ECONNREFUSED 127.0.0.1:5432\r\n",
    );
    insta::assert_snapshot!("failed_unit", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_idle_lane_details() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 20).await;
    for _ in 0..20 {
        if app.selected_lane().is_some_and(|l| l.reference == "acme@2")
            && app.selected_unit().is_none()
        {
            break;
        }
        app.update(key(KeyCode::Char('j')));
    }
    insta::assert_snapshot!("lane_details", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_empty_project() -> TestResult {
    let api = FakeApi::with_lanes(Vec::new());
    let (app, effects) = loaded(&api, 80, 12).await;
    assert!(effects.is_empty());
    insta::assert_snapshot!("empty", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_tiny_terminal() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (app, _) = loaded(&api, 30, 6).await;
    insta::assert_snapshot!("tiny", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_monitor_tab() -> TestResult {
    let mut api = FakeApi::with_lanes(mixed_lanes());
    api.metrics = vec![
        UnitMetrics {
            lane: "acme@0".into(),
            unit: "api".into(),
            cpu_percent: Some(42.0),
            memory_bytes: Some(3 * 1024 * 1024 * 1024),
            history: (0..30)
                .map(|i| (i, (i % 10) as f64 * 10.0, (1000 + i * 40) << 20))
                .collect(),
        },
        UnitMetrics {
            lane: "acme@0".into(),
            unit: "web".into(),
            cpu_percent: Some(3.0),
            memory_bytes: Some(900 * 1024 * 1024),
            history: Vec::new(),
        },
    ];
    let (mut app, _) = loaded(&api, 100, 20).await;
    app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Monitor);
    let effects = app.update(Msg::Tick);
    assert!(matches!(effects.as_slice(), [Effect::FetchMetrics]));
    app.update(Msg::Metrics(api.metrics().await));
    insta::assert_snapshot!("monitor", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_monitor_without_metrics_uses_status_numbers() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 80, 12).await;
    app.update(key(KeyCode::Tab));
    app.update(Msg::Metrics(api.metrics().await));
    insta::assert_snapshot!("monitor_fallback", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_run_confirm_and_reconnecting() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 80, 16).await;
    select_unit(&mut app, "migrate").ok_or("migrate not reachable")?;
    app.update(key(KeyCode::Char('t')));
    app.update(Msg::EventsEnded(None));
    insta::assert_snapshot!("confirm_reconnecting", draw(&app)?.backend());
    Ok(())
}

// ---- keys ----------------------------------------------------------------------

#[tokio::test]
async fn shift_tab_toggles_focus_only_with_a_unit() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    app.update(shift_tab());
    assert_eq!(app.focus, Focus::List, "a lane row has no terminal");
    assert!(app.notice.is_some());
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"$ ");
    assert!(app.update(shift_tab()).is_empty());
    assert_eq!(app.focus, Focus::Terminal);
    // In terminal focus, list keys go to the unit instead.
    let effects = app.update(key(KeyCode::Char('q')));
    assert!(matches!(effects.as_slice(), [Effect::Input(bytes)] if bytes == b"q"));
    // Shift+Tab is consumed: it switches focus and sends nothing to the unit.
    assert!(app.update(shift_tab()).is_empty());
    assert_eq!(app.focus, Focus::List);
    Ok(())
}

#[tokio::test]
async fn help_says_shift_tab_is_reserved() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    app.update(key(KeyCode::Char('?')));
    let terminal = draw(&app)?;
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("switch between list and terminal"), "{text}");
    assert!(
        text.contains("shift+tab is reserved for focus; apps in the pane don't receive it"),
        "{text}"
    );
    assert!(!text.contains("C-a"), "{text}");
    Ok(())
}

#[tokio::test]
async fn tab_and_ctrl_a_reach_the_unit_in_terminal_focus() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"$ ");
    // In the list, Tab cycles lanes, monitor, daemon.
    app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Monitor);
    let effects = app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Daemon);
    assert!(effects.iter().any(|e| matches!(e, Effect::FetchDaemon)));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.tab, Tab::Lanes);
    app.update(ctrl('a'));
    assert_eq!(app.focus, Focus::List, "C-a no longer switches focus");
    app.update(shift_tab());
    let effects = app.update(key(KeyCode::Tab));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\t"));
    assert_eq!(
        app.tab,
        Tab::Lanes,
        "Tab completes in the shell, the tab stays"
    );
    let effects = app.update(ctrl('a'));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == &[1]));
    assert_eq!(app.focus, Focus::Terminal);
    Ok(())
}

#[tokio::test]
async fn start_stop_restart_dispatch_lane_and_unit_refs() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let effects = app.update(key(KeyCode::Char('s')));
    let [
        Effect::Control {
            method,
            lane,
            selection,
            ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!((*method, lane.as_str()), (Method::Start, "acme@0"));
    assert!(selection.units.is_empty(), "lane start selects no units");

    let effects = app.update(key(KeyCode::Char('r')));
    let [
        Effect::Control {
            method, selection, ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(*method, Method::Restart);
    assert_eq!(
        selection.units,
        ["acme@0:api", "acme@0:web"],
        "only running services"
    );

    select_unit(&mut app, "worker").ok_or("worker not reachable")?;
    let effects = app.update(key(KeyCode::Char('x')));
    let [
        Effect::Control {
            method,
            lane,
            selection,
            label,
            target,
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!((*method, lane.as_str()), (Method::Stop, "acme@0"));
    assert_eq!(selection.units, ["acme@0:worker"]);
    assert_eq!(label, "stop acme@0:worker");
    assert_eq!(
        *target,
        Target::Unit {
            lane: "acme@0".into(),
            unit: "worker".into()
        }
    );
    Ok(())
}

#[tokio::test]
async fn idle_lane_starts_with_one_key() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    while app.selected_lane().is_some_and(|l| l.reference != "acme@1") {
        app.update(key(KeyCode::Char('j')));
    }
    assert!(app.selected_unit().is_none(), "idle lanes start collapsed");
    let effects = app.update(key(KeyCode::Char('s')));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Control { method: Method::Start, lane, .. }] if lane == "acme@1"
    ));
    Ok(())
}

#[tokio::test]
async fn arrows_follow_application_cursor_mode() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    app.update(shift_tab());
    let effects = app.update(key(KeyCode::Up));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\x1b[A"));
    // The program switches to application cursor mode (as vim and less do).
    app.update(Msg::AttachOutput {
        generation,
        bytes: b"\x1b[?1h".to_vec(),
    });
    let effects = app.update(key(KeyCode::Up));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\x1bOA"));
    let effects = app.update(ctrl('c'));
    assert!(
        matches!(effects.as_slice(), [Effect::Input(b)] if b == &[3]),
        "C-c goes to the unit"
    );
    Ok(())
}

#[tokio::test]
async fn paste_respects_bracketed_mode_and_focus() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"\x1b[?2004h");
    assert!(
        app.update(Msg::Paste("ls\n".into())).is_empty(),
        "list focus ignores paste"
    );
    app.update(shift_tab());
    let effects = app.update(Msg::Paste("ls\n".into()));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\x1b[200~ls\r\x1b[201~"));
    Ok(())
}

#[tokio::test]
async fn task_runs_only_after_confirmation() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    // From a lane row, `t` offers the lane's tasks.
    assert!(app.update(key(KeyCode::Char('t'))).is_empty());
    assert!(matches!(&app.dialog, Some(Dialog::Tasks { tasks, .. }) if tasks == &["migrate"]));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(
        matches!(&app.dialog, Some(Dialog::Confirm { prompt, .. }) if prompt == "run migrate in acme@0?")
    );
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    assert!(app.dialog.is_none());

    app.update(key(KeyCode::Char('t')));
    app.update(key(KeyCode::Enter));
    let effects = app.update(key(KeyCode::Char('y')));
    let [Effect::Run { lane, params }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!((lane.as_str(), params.task.as_str()), ("acme@0", "migrate"));
    assert!(params.terminal && params.detach);

    // An interactive task gets the terminal once it starts.
    let effects = app.update(Msg::RunDone {
        lane: "acme@0".into(),
        task: "migrate".into(),
        result: Ok(json!({"interactive": true})),
    });
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Attach { unit, .. } if unit == "acme@0:migrate"))
    );
    assert_eq!(app.focus, Focus::Terminal);
    Ok(())
}

#[tokio::test]
async fn zoom_resizes_the_pty() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let before = app.pane_size();
    let effects = app.update(key(KeyCode::Char('z')));
    let [Effect::ResizePty { columns, rows }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!((*columns, *rows), (98, 20));
    assert!(*columns > before.0);
    Ok(())
}

#[tokio::test]
async fn copy_mode_copies_the_cursor_line() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"first line\r\nsecond line");
    app.update(key(KeyCode::Char('c')));
    // The cursor starts on the bottom row; move up to the second line.
    let rows = app.pane_size().1;
    for _ in 0..(rows - 2) {
        app.update(key(KeyCode::Char('k')));
    }
    let effects = app.update(key(KeyCode::Char('y')));
    assert!(matches!(effects.as_slice(), [Effect::Copy(text)] if text == "second line"));
    Ok(())
}

#[tokio::test]
async fn quit_keys_never_stop_units() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    assert!(matches!(
        app.update(key(KeyCode::Char('q'))).as_slice(),
        [Effect::Quit]
    ));
    assert!(matches!(app.update(ctrl('c')).as_slice(), [Effect::Quit]));
    Ok(())
}

// ---- events and connection -----------------------------------------------------

#[tokio::test]
async fn resync_and_unreadable_events_requery_status() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let event = |name: &str, data| {
        Msg::Event(devlanes_proto::Event {
            seq: 9,
            event: name.into(),
            data,
        })
    };
    assert!(matches!(
        app.update(event(events::RESYNC, json!({}))).as_slice(),
        [Effect::Refresh]
    ));
    assert!(matches!(
        app.update(event(events::UNIT_CHANGED, json!({"unit": "garbage"})))
            .as_slice(),
        [Effect::Refresh]
    ));
    assert!(matches!(
        app.update(event(
            events::UNIT_CHANGED,
            json!({"unit": fake::unit("ghost@9", "x", "ready")})
        ))
        .as_slice(),
        [Effect::Refresh]
    ));
    Ok(())
}

#[tokio::test]
async fn unit_changed_updates_in_place_and_keeps_selection() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    select_unit(&mut app, "worker").ok_or("worker not reachable")?;
    let mut restarted = fake::unit("acme@0", "worker", "starting");
    restarted.exit_code = None;
    let effects = app.update(Msg::Event(devlanes_proto::Event {
        seq: 2,
        event: events::UNIT_CHANGED.into(),
        data: json!({ "unit": restarted }),
    }));
    assert!(
        effects.is_empty(),
        "the stream is still open, no reattach: {effects:?}"
    );
    assert_eq!(
        app.selected_unit().map(|u| u.state.as_str()),
        Some("starting")
    );
    Ok(())
}

#[tokio::test]
async fn disconnect_backs_off_and_resubscribes() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    app.update(Msg::AttachClosed { generation });
    let effects = app.update(Msg::EventsEnded(None));
    assert!(matches!(
        effects.as_slice(),
        [Effect::ScheduleReconnect { after_ms: 250 }]
    ));
    assert_eq!(app.connection, Connection::Reconnecting { attempt: 0 });
    // A second failure before the retry fires does not schedule another one.
    assert!(
        app.update(Msg::Status(Err(devlanes_tui::ApiError::Disconnected(
            "gone".into()
        ))))
        .is_empty()
    );
    let effects = app.update(Msg::Reconnect);
    assert!(matches!(
        effects.as_slice(),
        [Effect::Subscribe, Effect::Refresh]
    ));
    let effects = app.update(Msg::Subscribed(Err(devlanes_tui::ApiError::Disconnected(
        "gone".into(),
    ))));
    assert!(matches!(
        effects.as_slice(),
        [Effect::ScheduleReconnect { after_ms: 1000 }]
    ));
    app.update(Msg::Reconnect);
    let effects = app.update(Msg::Subscribed(Ok(())));
    assert_eq!(app.connection, Connection::Online);
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Attach { unit, .. } if unit == "acme@0:api")),
        "the closed stream is reopened: {effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn stale_attach_output_is_ignored() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let first = select_unit(&mut app, "api").ok_or("api not reachable")?;
    let second = select_unit(&mut app, "web").ok_or("web not reachable")?;
    assert_ne!(first, second);
    feed(&mut app, second, b"web output");
    app.update(Msg::AttachOutput {
        generation: first,
        bytes: b"\x1b[2J\x1b[Hapi output".to_vec(),
    });
    let text = app
        .attached
        .as_ref()
        .map(|a| a.screen.screen().contents())
        .unwrap_or_default();
    assert!(text.starts_with("web output"), "{text:?}");
    Ok(())
}

// ---- full loop -----------------------------------------------------------------

#[tokio::test]
async fn drive_loop_resyncs_and_quits_without_stopping() -> TestResult {
    let api = Arc::new(FakeApi::with_lanes(mixed_lanes()));
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let events = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = {
        let api = Arc::clone(&api);
        async move {
            // Wait for the subscription, then ask the TUI to resync.
            for _ in 0..100 {
                if api.emit(events::RESYNC, json!({})).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            let press = |c| {
                Ok(TermEvent::Key(KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                )))
            };
            keys_tx.send(press('j')).await.ok();
            tokio::time::sleep(Duration::from_millis(50)).await;
            keys_tx.send(press('q')).await.ok();
        }
    };
    let options = Options::default();
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        events,
        Box::new(std::io::sink()),
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    result?;
    let calls = api.calls();
    assert_eq!(
        calls.iter().filter(|c| *c == "status").count(),
        2,
        "{calls:?}"
    );
    assert!(calls.contains(&"subscribe".to_owned()), "{calls:?}");
    assert!(
        calls.iter().any(|c| c.starts_with("attach acme@0:api")),
        "{calls:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("Stop") || c.starts_with("Restart")),
        "quitting must not stop anything: {calls:?}"
    );
    insta::assert_snapshot!("drive_after_resync", terminal.backend());
    Ok(())
}

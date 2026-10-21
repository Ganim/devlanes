//! The daemon tab, its confirmed actions, and cleaning a lane.

use std::{io::Write, sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::RpcError;
use devlanes_tui::{
    ApiError, DaemonApi, Options,
    app::{App, Dialog, Effect, Msg, Tab, Target},
    drive,
    logfile::LogChunk,
};
use futures_util::stream;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tokio::sync::mpsc;

use crate::errors::{assert_clean, key, render, row, screen};
use crate::fake::{FakeApi, daemon_status, doctor_facts, mixed_lanes};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Shift+letter as crossterm reports it.
fn shifted(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT))
}

/// Types `text` one key at a time.
fn type_text(app: &mut App, text: &str) -> Vec<Effect> {
    text.chars()
        .flat_map(|c| app.update(key(KeyCode::Char(c))))
        .collect()
}

/// Log lines as tracing writes them, with one very long line, colors and a tab.
fn log_lines() -> Vec<String> {
    vec![
        "2026-10-21T09:12:01.003Z  INFO devlanes_daemon: listening socket=/home/acme/.devlanes/run/daemon.sock".into(),
        "2026-10-21T09:12:01.120Z  INFO devlanes_proxy: http bound port=80".into(),
        format!(
            "2026-10-21T09:12:01.121Z  \x1b[33mWARN\x1b[0m devlanes_proxy: https fell back port=8443 reason=\"{}\"",
            "port 443 needs root ".repeat(12)
        ),
        "2026-10-21T09:13:44.502Z  INFO devlanes_daemon::lifecycle: lane started\tlane=acme@0 units=3".into(),
        "2026-10-21T09:14:02.990Z ERROR devlanes_daemon::lifecycle: compose failed lane=acme@1".into(),
    ]
}

/// An app on the daemon tab with fake facts and a log tail.
async fn daemon_app(width: u16, height: u16) -> Result<App, Box<dyn std::error::Error>> {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(width, height);
    app.update(Msg::Status(api.status().await));
    app.update(Msg::Subscribed(Ok(())));
    let effects = app.update(key(KeyCode::Char('3')));
    assert_eq!(app.tab, Tab::Daemon);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::FetchDaemon)),
        "{effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ReadDaemonLog { offset: 0 }))
    );
    app.update(Msg::DaemonInfo {
        status: api.daemon_status().await,
        doctor: api.doctor().await,
    });
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines: log_lines(),
        offset: 900,
        reset: true,
    })));
    Ok(app)
}

#[tokio::test]
async fn daemon_tab_at_80x24() -> TestResult {
    let app = daemon_app(80, 24).await?;
    let buffer = render(&app)?;
    assert_clean(&buffer, "daemon tab");
    let text = screen(&buffer);
    for expected in [
        "0.1.0 · pid 4242 · up 2h 5m",
        "3 managed · 4 event subscribers · 2 projects",
        "https on fallback port 8443: port 443 needs root",
        "fix: devlanes certs trust",
        "Compose 2.29.1",
        "following",
        // The long log lines wrap instead of running past the border.
        "lifecycle: compose failed",
        "│lane=acme@1",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    insta::assert_snapshot!("daemon_tab_80x24", text);
    Ok(())
}

#[tokio::test]
async fn daemon_log_pauses_when_scrolled_and_follows_again() -> TestResult {
    let mut app = daemon_app(80, 24).await?;
    app.update(key(KeyCode::Char('k')));
    app.update(key(KeyCode::Char('k')));
    assert_eq!(app.daemon.scroll, 2);
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines: vec!["new line".into()],
        offset: 950,
        reset: false,
    })));
    assert_eq!(
        app.daemon.scroll, 3,
        "a paused view stays on the same lines"
    );
    let text = screen(&render(&app)?);
    assert!(text.contains("paused 3 lines up · G follows"), "{text}");
    app.update(key(KeyCode::Char('G')));
    assert_eq!(app.daemon.scroll, 0);
    // Every tick reads the log; every third also refreshes the facts.
    let ticks: Vec<Vec<Effect>> = (0..3).map(|_| app.update(Msg::Tick)).collect();
    assert!(ticks.iter().all(|e| {
        e.iter()
            .any(|e| matches!(e, Effect::ReadDaemonLog { offset: 950 }))
    }));
    assert_eq!(
        ticks
            .iter()
            .flatten()
            .filter(|e| matches!(e, Effect::FetchDaemon))
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn restart_and_stop_ask_first_and_enter_does_not_confirm() -> TestResult {
    let mut app = daemon_app(80, 24).await?;
    assert!(app.update(shifted('R')).is_empty());
    let Some(Dialog::Confirm {
        prompt,
        detail,
        strict,
        ..
    }) = &app.dialog
    else {
        return Err(format!("no confirm dialog: {:?}", app.dialog).into());
    };
    assert_eq!(prompt, "restart the daemon?");
    assert!(
        detail[0].contains("stops 3 managed processes"),
        "{detail:?}"
    );
    assert!(detail[0].contains("devlanes lane resume"), "{detail:?}");
    assert!(*strict);
    insta::assert_snapshot!("restart_confirm_80x24", screen(&render(&app)?));
    assert!(
        app.update(key(KeyCode::Enter)).is_empty(),
        "enter must not restart"
    );
    assert!(app.dialog.is_some());
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    assert!(app.dialog.is_none());

    app.update(shifted('R'));
    let effects = app.update(key(KeyCode::Char('y')));
    assert!(
        matches!(effects.as_slice(), [Effect::RestartDaemon]),
        "{effects:?}"
    );
    assert_eq!(app.daemon.busy, Some("restarting"));
    // The old daemon closes the subscription; that is expected, not an error.
    let effects = app.update(Msg::EventsEnded(None));
    assert!(matches!(
        effects.as_slice(),
        [Effect::ScheduleReconnect { .. }]
    ));
    assert!(app.errors.is_empty(), "{:?}", app.errors);
    assert!(row(&render(&app)?, 0).contains("daemon: restarting"));
    let effects = app.update(Msg::DaemonRestarted(Ok(daemon_status())));
    assert!(matches!(
        effects.as_slice(),
        [Effect::Subscribe, Effect::Refresh, Effect::FetchDaemon]
    ));
    assert_eq!(
        app.notice.as_ref().map(|n| n.text.as_str()),
        Some("daemon restarted")
    );

    app.update(shifted('S'));
    let effects = app.update(key(KeyCode::Char('y')));
    assert!(matches!(effects.as_slice(), [Effect::StopDaemon]));
    app.update(Msg::DaemonStopped(Ok(json!({"stopping": true}))));
    app.update(Msg::EventsEnded(None));
    assert!(app.daemon.stopped);
    assert!(app.errors.is_empty(), "a requested stop is not an error");
    let buffer = render(&app)?;
    assert!(
        row(&buffer, 0).contains("daemon stopped"),
        "{}",
        row(&buffer, 0)
    );
    assert!(
        row(&buffer, 23).contains("s start daemon"),
        "{}",
        row(&buffer, 23)
    );
    // Restart and stop refuse while stopped; s starts it again.
    assert!(app.update(shifted('R')).is_empty());
    assert!(app.dialog.is_none());
    let effects = app.update(key(KeyCode::Char('s')));
    assert!(matches!(effects.as_slice(), [Effect::StartDaemon]));
    app.update(Msg::DaemonStarted(Ok(daemon_status())));
    assert!(!app.daemon.stopped);
    assert_eq!(
        app.notice.as_ref().map(|n| n.text.as_str()),
        Some("daemon started")
    );
    Ok(())
}

#[tokio::test]
async fn lane_gc_asks_first() -> TestResult {
    let mut app = daemon_app(80, 24).await?;
    assert!(app.update(key(KeyCode::Char('g'))).is_empty());
    assert!(
        matches!(&app.dialog, Some(Dialog::Confirm { prompt, .. }) if prompt == "run lane gc?")
    );
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    app.update(key(KeyCode::Char('g')));
    let effects = app.update(key(KeyCode::Char('y')));
    assert!(matches!(effects.as_slice(), [Effect::LaneGc]));
    let effects = app.update(Msg::GcDone(Ok(json!({"cleaned": ["acme@4"]}))));
    assert!(matches!(effects.as_slice(), [Effect::Refresh]));
    assert_eq!(
        app.notice.as_ref().map(|n| n.text.as_str()),
        Some("lane gc: cleaned acme@4")
    );
    Ok(())
}

#[tokio::test]
async fn clean_requires_typing_the_lane_name() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(80, 24);
    app.update(Msg::Status(api.status().await));
    // From a unit row, C cleans the unit's lane.
    app.update(key(KeyCode::Char('j')));
    assert!(app.selected_unit().is_some());
    assert!(app.update(shifted('C')).is_empty());
    let Some(Dialog::TypeConfirm {
        expected, detail, ..
    }) = &app.dialog
    else {
        return Err(format!("no clean dialog: {:?}", app.dialog).into());
    };
    assert_eq!(expected, "acme@0");
    assert!(
        detail
            .iter()
            .any(|d| d == "worktree files are not touched.")
    );
    // `y` alone is text here, not a confirmation.
    assert!(type_text(&mut app, "y").is_empty());
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(matches!(
        app.dialog,
        Some(Dialog::TypeConfirm { mismatch: true, .. })
    ));
    app.update(key(KeyCode::Backspace));
    assert!(type_text(&mut app, "acme@0").is_empty());
    insta::assert_snapshot!("clean_confirm_80x24", screen(&render(&app)?));
    let effects = app.update(key(KeyCode::Enter));
    assert!(matches!(effects.as_slice(), [Effect::CleanLane { lane }] if lane == "acme@0"));
    assert!(app.is_cleaning("acme@0"));
    assert!(screen(&render(&app)?).contains("@0 main  cleaning…"));

    // A failure lands on the lane like a failed start.
    let failure = RpcError::new(
        "compose_failed",
        json!({"message": "`docker compose -p acme down -v` failed (exit status: 1): volume in use"}),
        1,
    );
    app.update(Msg::CleanDone {
        lane: "acme@0".into(),
        result: Err(ApiError::Rpc(failure)),
    });
    assert!(!app.is_cleaning("acme@0"));
    assert!(app.target_error(&Target::Lane("acme@0".into())).is_some());
    assert_eq!(
        app.errors.front().map(|e| e.what.as_str()),
        Some("clean acme@0")
    );
    assert!(screen(&render(&app)?).contains("! clean failed"));

    app.update(shifted('C'));
    type_text(&mut app, "acme@0");
    app.update(key(KeyCode::Enter));
    let effects = app.update(Msg::CleanDone {
        lane: "acme@0".into(),
        result: api.clean_lane("acme@0".into()).await,
    });
    assert!(matches!(effects.as_slice(), [Effect::Refresh]));
    assert!(app.target_error(&Target::Lane("acme@0".into())).is_none());
    assert_eq!(
        app.notice.as_ref().map(|n| n.text.as_str()),
        Some("clean acme@0: done, stopped 4 units")
    );
    Ok(())
}

#[tokio::test]
async fn help_lists_clean_and_the_daemon_tab() -> TestResult {
    let mut app = App::new(80, 24);
    app.update(key(KeyCode::Char('?')));
    let text = screen(&render(&app)?);
    for expected in [
        "clean the lane (asks you to type its name)",
        "daemon tab: restart / stop the daemon",
        "daemon tab: lane gc",
        "shift+tab is reserved for focus",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    Ok(())
}

// ---- full loop -------------------------------------------------------------------

/// Runs `drive` against `api` while `keys` are pressed with a pause between them.
pub async fn run_keys(
    api: Arc<FakeApi>,
    keys: Vec<Msg>,
) -> Result<Terminal<TestBackend>, Box<dyn std::error::Error>> {
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(64);
    let input = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = {
        let api = Arc::clone(&api);
        async move {
            for _ in 0..100 {
                if api.calls().iter().any(|c| c == "status") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            for msg in keys {
                if let Msg::Key(event) = msg {
                    keys_tx.send(Ok(TermEvent::Key(event))).await.ok();
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
            keys_tx
                .send(Ok(TermEvent::Key(KeyEvent::new(
                    KeyCode::Char('q'),
                    KeyModifiers::NONE,
                ))))
                .await
                .ok();
        }
    };
    let options = Options::default();
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        input,
        Box::new(std::io::sink()),
    );
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    result?;
    Ok(terminal)
}

#[tokio::test]
async fn drive_cleans_only_after_the_name_is_typed() -> TestResult {
    let api = Arc::new(FakeApi::with_lanes(mixed_lanes()));
    let mut keys = vec![shifted('C'), key(KeyCode::Esc), shifted('C')];
    keys.extend("acme@0".chars().map(|c| key(KeyCode::Char(c))));
    keys.push(key(KeyCode::Enter));
    let terminal = run_keys(Arc::clone(&api), keys).await?;
    let calls = api.calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("lane.clean")).count(),
        1,
        "{calls:?}"
    );
    assert!(calls.contains(&"lane.clean acme@0".to_owned()), "{calls:?}");
    let after_clean = calls.iter().skip_while(|c| !c.starts_with("lane.clean"));
    assert!(
        after_clean.filter(|c| *c == "status").count() >= 1,
        "refresh after clean: {calls:?}"
    );
    let header = row(terminal.backend().buffer(), 0);
    assert!(header.contains("clean acme@0: done"), "{header}");
    Ok(())
}

#[tokio::test]
async fn drive_tails_the_daemon_log_file() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("daemon.log");
    let mut file = std::fs::File::create(&path)?;
    for line in log_lines() {
        writeln!(file, "{line}")?;
    }
    let mut fake = FakeApi::with_lanes(mixed_lanes());
    fake.log_path = Some(path);
    let api = Arc::new(fake);
    let terminal = run_keys(Arc::clone(&api), vec![key(KeyCode::Char('3'))]).await?;
    let text = screen(terminal.backend().buffer());
    assert!(text.contains("lifecycle: compose failed"), "{text}");
    assert!(text.contains("Compose 2.29.1"), "{text}");
    assert_clean(terminal.backend().buffer(), "drive daemon tab");
    assert!(!api.calls().iter().any(|c| c.starts_with("daemon.shutdown")));
    Ok(())
}

/// Doctor data the fake serves; used to keep the snapshot readable.
#[test]
fn fake_doctor_has_a_warning_and_a_failure() {
    let checks = devlanes_tui::doctor::checks(&doctor_facts());
    assert!(
        checks
            .iter()
            .any(|c| c.state == devlanes_tui::doctor::CheckState::Warn)
    );
    assert!(
        checks
            .iter()
            .any(|c| c.state == devlanes_tui::doctor::CheckState::Fail)
    );
}

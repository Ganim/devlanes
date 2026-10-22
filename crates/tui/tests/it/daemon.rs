//! The daemon tab, its confirmed actions, and cleaning a lane.

use std::{io::Write, sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::RpcError;
use devlanes_tui::{
    ApiError, DaemonApi, Options,
    app::{App, Dialog, Effect, Msg, Target, View},
    drive,
    logfile::{LogChunk, LogPosition},
};
use futures_util::stream;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tokio::sync::mpsc;

use crate::errors::{assert_clean, key, render, row, screen};
use crate::fake::{FakeApi, daemon_status, doctor_facts, mixed_lanes};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Shift+letter as crossterm reports it.
pub fn shifted(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT))
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
    let effects = app.update(shifted('D'));
    assert_eq!(app.view, View::Daemon);
    assert!(
        effects.iter().any(|e| matches!(e, Effect::FetchDaemon)),
        "{effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::ReadDaemonLog { from } if from.offset == 0))
    );
    app.update(Msg::DaemonInfo {
        status: api.daemon_status().await,
        doctor: api.doctor().await,
    });
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines: log_lines(),
        position: LogPosition {
            offset: 900,
            file: (1, 2),
        },
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
        // Below 100 columns the facts are two lines in the checks' box.
        "0.1.0 · pid 4242 · up 2h 5m · 3 managed",
        "https on fallback port 8443: port 443 needs root",
        "fix: devlanes certs trust",
        "following · all levels · repeats folded",
        // Parsed: level tag, target without `devlanes_`, the message, fields.
        "ERROR daemon::lifecycle",
        "compose failed",
        "lane=acme@1",
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
    assert!(app.daemon.paused, "scrolling up pauses");
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines: vec!["2026-10-21T09:15:00.000Z  INFO devlanes_daemon: new entry".into()],
        position: LogPosition {
            offset: 950,
            file: (1, 2),
        },
        reset: false,
    })));
    assert_eq!(
        app.daemon.scroll, 3,
        "a paused view stays on the same entries"
    );
    let text = screen(&render(&app)?);
    assert!(text.contains("paused · 1 new · G follows"), "{text}");
    app.update(key(KeyCode::Char('G')));
    assert_eq!(app.daemon.scroll, 0);
    assert!(!app.daemon.paused);
    // F pauses at the bottom: arriving entries no longer scroll the view.
    app.update(key(KeyCode::Char('F')));
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines: vec!["2026-10-21T09:16:00.000Z  INFO devlanes_daemon: newer entry".into()],
        position: LogPosition {
            offset: 950,
            file: (1, 2),
        },
        reset: false,
    })));
    assert_eq!(app.daemon.scroll, 1);
    app.update(key(KeyCode::Char('F')));
    assert_eq!((app.daemon.scroll, app.daemon.paused), (0, false));
    // Every tick reads the log; every third also refreshes the facts.
    let ticks: Vec<Vec<Effect>> = (0..3).map(|_| app.update(Msg::Tick)).collect();
    assert!(ticks.iter().all(|e| {
        e.iter()
            .any(|e| matches!(e, Effect::ReadDaemonLog { from } if from.offset == 950))
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
async fn the_log_filters_by_level_and_text_and_folds_repeats() -> TestResult {
    let mut app = daemon_app(120, 40).await?;
    // A repeat of the first warning folds into it.
    let repeat = log_lines()[2].replace("09:12:01.121Z", "09:20:00.000Z");
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines: vec![repeat],
        position: LogPosition {
            offset: 950,
            file: (1, 2),
        },
        reset: false,
    })));
    let text = screen(&render(&app)?);
    assert!(text.contains("×2 since 09:12"), "{text}");
    assert_eq!(app.daemon_shown().1.len(), 5);
    app.update(key(KeyCode::Char('d')));
    assert_eq!(app.daemon_shown().1.len(), 6, "d shows every repeat");
    app.update(key(KeyCode::Char('d')));
    // w: warn and above, then errors only, then all again.
    app.update(key(KeyCode::Char('w')));
    assert_eq!(app.daemon_shown().1.len(), 2);
    assert!(screen(&render(&app)?).contains("warn and above"));
    app.update(key(KeyCode::Char('w')));
    assert_eq!(app.daemon_shown().1.len(), 1);
    app.update(key(KeyCode::Char('w')));
    // /: words that must all match; Enter keeps it, Esc clears it before
    // leaving the page.
    app.update(key(KeyCode::Char('/')));
    for c in "LANE acme@0".chars() {
        app.update(key(KeyCode::Char(c)));
    }
    app.update(key(KeyCode::Enter));
    assert_eq!(app.daemon_shown().1.len(), 1);
    assert!(screen(&render(&app)?).contains("/LANE acme@0 1 of 6"));
    app.update(key(KeyCode::Esc));
    assert!(app.daemon.filters.text.is_empty());
    assert_eq!(app.view, View::Daemon);
    // i lists the passing checks one per line.
    let before = screen(&render(&app)?);
    app.update(key(KeyCode::Char('i')));
    let after = screen(&render(&app)?);
    assert!(!before.contains("Compose 2.29.1") && after.contains("Compose 2.29.1"));
    app.update(key(KeyCode::Esc));
    assert_eq!(app.view, View::Project);
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
    assert_eq!(prompt, "Restart the daemon?");
    assert!(detail[0].contains("It stops 3 processes"), "{detail:?}");
    assert!(
        detail[0].contains("The new daemon starts the running lanes again"),
        "{detail:?}"
    );
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
    assert!(row(&render(&app)?, 0).contains("daemon restarting…"));
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
        row(&buffer, 23).contains("S start daemon"),
        "{}",
        row(&buffer, 23)
    );
    // Restart refuses while stopped; S starts it again.
    assert!(app.update(shifted('R')).is_empty());
    assert!(app.dialog.is_none());
    let effects = app.update(shifted('S'));
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
async fn a_small_window_keeps_the_dialog_keys() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(50, 8);
    app.update(Msg::Status(api.status().await));
    app.update(shifted('C'));
    let buffer = render(&app)?;
    let text = screen(&buffer);
    // The explanation is cut; the keys that confirm and cancel are not.
    for expected in [
        "clean acme@0",
        "Clean main (acme@0)?",
        "enter clean",
        "esc cancel",
        "…",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    insta::assert_snapshot!("clean_confirm_50x8", text);
    Ok(())
}

#[tokio::test]
async fn daemon_log_lines_can_be_selected_and_copied() -> TestResult {
    let mut app = daemon_app(100, 30).await?;
    let entries = app.daemon.log.len();
    assert!(app.update(key(KeyCode::Char('c'))).is_empty());
    assert_eq!(
        app.daemon.copy.map(|c| c.cursor),
        Some(entries - 1),
        "starts on the newest entry"
    );
    let text = screen(&render(&app)?);
    assert!(text.contains("v mark"), "{text}");
    app.update(key(KeyCode::Char('k')));
    app.update(key(KeyCode::Char('v')));
    app.update(key(KeyCode::Char('k')));
    // y: the entries as they are in the file, timestamps and all.
    let effects = app.update(key(KeyCode::Char('y')));
    let [Effect::Copy(copied)] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    let expected: Vec<String> = app
        .daemon
        .log
        .range(entries - 3..entries - 1)
        .cloned()
        .collect();
    assert_eq!(copied, &expected.join("\n"));
    assert!(!copied.contains('\x1b'), "colors are not copied");
    assert!(app.daemon.copy.is_none());
    // Y: as shown, local time and the parsed columns. G first: the copy
    // left the view where the selection was.
    app.update(key(KeyCode::Char('G')));
    app.update(key(KeyCode::Char('c')));
    let effects = app.update(key(KeyCode::Char('Y')));
    let [Effect::Copy(copied)] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert!(
        copied.contains("ERROR daemon::lifecycle") && copied.contains("compose failed"),
        "{copied}"
    );
    assert!(!copied.contains("2026-10-21T"), "{copied}");
    // Esc leaves without copying; q there does not quit.
    app.update(key(KeyCode::Char('c')));
    assert!(app.update(key(KeyCode::Char('q'))).is_empty());
    assert!(app.daemon.copy.is_none());
    Ok(())
}

#[tokio::test]
async fn lane_gc_asks_first() -> TestResult {
    let mut app = daemon_app(80, 24).await?;
    // `g` is "top" on every tab, the daemon log included; gc moved to `X`.
    app.update(key(KeyCode::Char('g')));
    assert!(app.dialog.is_none());
    assert_eq!(app.daemon.scroll, app.daemon.log.len());
    assert!(app.update(shifted('X')).is_empty());
    assert!(
        matches!(&app.dialog, Some(Dialog::Confirm { prompt, .. }) if prompt == "Clean every lane whose worktree is gone?")
    );
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    app.update(shifted('X'));
    let effects = app.update(key(KeyCode::Char('y')));
    assert!(matches!(effects.as_slice(), [Effect::LaneGc]));
    let effects = app.update(Msg::GcDone(Ok(json!({"cleaned": ["acme@4"]}))));
    assert!(matches!(effects.as_slice(), [Effect::Refresh]));
    assert_eq!(
        app.notice.as_ref().map(|n| n.text.as_str()),
        Some("cleaned acme@4")
    );
    Ok(())
}

#[tokio::test]
async fn clean_asks_y_or_enter() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let mut app = App::new(80, 24);
    app.update(Msg::Status(api.status().await));
    // From a unit row (the lane opens on its failed worker), C cleans the lane.
    assert!(app.selected_unit().is_some());
    assert!(app.update(shifted('C')).is_empty());
    let Some(Dialog::Confirm {
        prompt,
        detail,
        strict,
        ..
    }) = &app.dialog
    else {
        return Err(format!("no clean dialog: {:?}", app.dialog).into());
    };
    assert_eq!(prompt, "Clean main (acme@0)?");
    assert!(!strict, "Enter confirms too");
    assert!(
        detail
            .iter()
            .any(|d| d == "Files in /work/acme/main stay as they are.")
    );
    // n and Esc cancel without cleaning.
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    assert!(app.dialog.is_none());
    app.update(shifted('C'));
    insta::assert_snapshot!("clean_confirm_80x24", screen(&render(&app)?));
    let effects = app.update(key(KeyCode::Enter));
    assert!(matches!(effects.as_slice(), [Effect::CleanLane { lane }] if lane == "acme@0"));
    assert!(app.is_cleaning("acme@0"));
    assert!(screen(&render(&app)?).contains("cleaning…"));

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
    select_lane(&mut app, "acme@0");
    assert!(screen(&render(&app)?).contains("clean failed"));

    app.update(shifted('C'));
    app.update(key(KeyCode::Char('y')));
    let effects = app.update(Msg::CleanDone {
        lane: "acme@0".into(),
        result: api.clean_lane("acme@0".into()).await,
    });
    assert!(matches!(effects.as_slice(), [Effect::Refresh]));
    assert!(app.target_error(&Target::Lane("acme@0".into())).is_none());
    assert_eq!(
        app.notice.as_ref().map(|n| n.text.as_str()),
        Some("cleaned acme@0, stopped 4 units")
    );
    Ok(())
}

/// Shows the whole-lane row of `lane`.
fn select_lane(app: &mut App, lane: &str) {
    let mut effects = Vec::new();
    app.select(devlanes_tui::app::Selected::lane(lane), &mut effects);
}

#[tokio::test]
async fn help_lists_clean_and_the_daemon_page() -> TestResult {
    let mut app = App::new(100, 60);
    app.update(key(KeyCode::Char('?')));
    let text = screen(&render(&app)?);
    for expected in [
        "clean the lane (asks first)",
        "copy mode: v select, y copy as in the file, Y as shown",
        "restart, stop or start the daemon",
        "clean every lane whose worktree is gone",
        "hide the hint strip or version notice",
        "monitor, daemon; again or esc returns",
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
    // The closing q quits at once; the quit dialog has its own tests.
    let options = Options {
        on_quit: Some("keep".into()),
        ..Options::default()
    };
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
    assert!(header.contains("cleaned acme@0"), "{header}");
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
    let terminal = run_keys(Arc::clone(&api), vec![shifted('D')]).await?;
    let text = screen(terminal.backend().buffer());
    assert!(text.contains("ERROR daemon::lifecycle"), "{text}");
    assert!(text.contains("compose failed"), "{text}");
    assert!(
        text.contains("2 fail · 1 warn") || text.contains("fail"),
        "{text}"
    );
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

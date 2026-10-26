//! The daemon tab, its confirmed actions, and cleaning a lane.

use std::{io::Write, sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::RpcError;
use devlanes_tui::{
    ApiError, DaemonApi, Options,
    app::{App, DaemonBox, Dialog, Effect, Msg, Target, View},
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
    let facts = format!("{} · pid 4242 · up 2h 5m · 3 managed", crate::fake::VERSION);
    for expected in [
        // Below 100 columns the facts are two lines in the checks' box.
        facts.as_str(),
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
    assert_screen!("daemon_tab_80x24", text);
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
async fn targets_pick_what_the_log_shows_and_matches_are_marked() -> TestResult {
    let mut app = daemon_app(120, 40).await?;
    // T lists the targets of the kept lines with their counts.
    app.update(shifted('T'));
    let Some(Dialog::Targets(picker)) = &app.dialog else {
        return Err(format!("no picker: {:?}", app.dialog).into());
    };
    let names: Vec<&str> = picker.items.iter().map(|i| i.0.as_str()).collect();
    assert_eq!(names, ["daemon", "daemon::lifecycle", "proxy"]);
    assert_eq!(picker.items[1].1, 2, "two lifecycle lines");
    let text = screen(&render(&app)?);
    assert!(text.contains("daemon.log targets"), "{text}");
    // Check proxy and apply: only its lines show; the title says so.
    for code in [
        KeyCode::Char('j'),
        KeyCode::Char('j'),
        KeyCode::Char(' '),
        KeyCode::Enter,
    ] {
        app.update(key(code));
    }
    assert!(app.dialog.is_none());
    assert_eq!(app.daemon_shown().1.len(), 2);
    let text = screen(&render(&app)?);
    assert!(text.contains("1 target"), "{text}");
    assert!(!text.contains("compose failed"), "{text}");
    // Esc in the picker keeps the filter; `a` then Enter shows every target.
    app.update(shifted('T'));
    app.update(key(KeyCode::Esc));
    assert_eq!(app.daemon.filters.targets.len(), 1);
    app.update(shifted('T'));
    app.update(key(KeyCode::Char('a')));
    app.update(key(KeyCode::Enter));
    assert!(app.daemon.filters.targets.is_empty());
    // `/compose`: the matching word is marked like a search hit.
    app.update(key(KeyCode::Char('/')));
    for c in "compose".chars() {
        app.update(key(KeyCode::Char(c)));
    }
    app.update(key(KeyCode::Enter));
    let buffer = render(&app)?;
    let (y, line) = (0..buffer.area.height)
        .map(|y| (y, row(&buffer, y)))
        .find(|(_, line)| line.contains("compose failed"))
        .ok_or("no compose line")?;
    let x = u16::try_from(line[..line.find("compose").unwrap_or(0)].chars().count())?;
    let cell = &buffer[(x, y)];
    assert!(
        cell.modifier.contains(ratatui::style::Modifier::REVERSED),
        "{cell:?}"
    );
    // The word after it is not marked.
    let after = &buffer[(x + 8, y)];
    assert!(!after.modifier.contains(ratatui::style::Modifier::REVERSED));
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
    assert_screen!("restart_confirm_80x24", screen(&render(&app)?));
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
    assert_screen!("clean_confirm_50x8", text);
    Ok(())
}

/// The copied text of the one `Copy` effect in `effects`.
fn copied(effects: &[Effect]) -> Result<String, Box<dyn std::error::Error>> {
    match effects {
        [Effect::Copy(text)] => Ok(text.clone()),
        other => Err(format!("unexpected {other:?}").into()),
    }
}

#[tokio::test]
async fn daemon_log_text_is_selected_cell_by_cell_like_a_pane() -> TestResult {
    let mut app = daemon_app(100, 30).await?;
    render(&app)?;
    let newest = app.daemon_shown().1.len() - 1;
    assert!(app.update(key(KeyCode::Char('c'))).is_empty());
    let cursor = app.daemon.copy.map(|c| c.cursor).ok_or("no copy mode")?;
    assert_eq!(
        (cursor.entry, cursor.line, cursor.column),
        (newest, 0, 0),
        "starts at the left of the bottom line"
    );
    let buffer = render(&app)?;
    assert!(
        screen(&buffer).contains("h j k l move"),
        "{}",
        screen(&buffer)
    );
    // Only the cursor's cell is marked until v; the line is not painted.
    let (y, _) = (0..buffer.area.height)
        .map(|y| (y, row(&buffer, y)))
        .find(|(_, line)| line.contains("compose failed"))
        .ok_or("no compose line")?;
    let reversed = |x: u16| {
        buffer[(x, y)]
            .modifier
            .contains(ratatui::style::Modifier::REVERSED)
    };
    let marks: Vec<bool> = (0..6).map(reversed).collect();
    assert_eq!(marks, [false, true, false, false, false, false], "row {y}");

    // y without a mark: the cursor's line as shown.
    let line = copied(&app.update(key(KeyCode::Char('y'))))?;
    assert!(
        line.contains("ERROR daemon::lifecycle") && line.contains("compose failed"),
        "{line}"
    );
    assert!(
        !line.contains("2026-10-21T") && !line.contains('\n'),
        "{line}"
    );
    assert!(app.daemon.copy.is_none());

    // From the level of the line above to the tenth cell of the last one.
    app.update(key(KeyCode::Char('c')));
    app.update(key(KeyCode::Char('k')));
    for _ in 0..9 {
        app.update(key(KeyCode::Char('l')));
    }
    app.update(key(KeyCode::Char('v')));
    app.update(key(KeyCode::Char('j')));
    let text = copied(&app.update(key(KeyCode::Char('y'))))?;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("INFO  daemon::lifecycle"), "{text}");
    assert!(lines[0].contains("lane started"), "{text}");
    assert!(
        lines[1].ends_with('E') && lines[1].chars().count() == 10,
        "{text}"
    );

    // Y: the whole entries the selection touches, as in the file.
    app.update(key(KeyCode::Char('c')));
    app.update(key(KeyCode::Char('k')));
    app.update(key(KeyCode::Char('v')));
    app.update(key(KeyCode::Char('j')));
    let raw = copied(&app.update(key(KeyCode::Char('Y'))))?;
    let log = app.daemon.log.len();
    let expected: Vec<String> = app.daemon.log.range(log - 2..).cloned().collect();
    assert_eq!(raw, expected.join("\n"));
    assert!(!raw.contains('\x1b'), "colors are not copied");

    // Esc leaves without copying; q there does not quit.
    app.update(key(KeyCode::Char('c')));
    assert!(app.update(key(KeyCode::Char('q'))).is_empty());
    assert!(app.daemon.copy.is_none());
    Ok(())
}

#[tokio::test]
async fn dragging_over_the_daemon_log_copies_the_text() -> TestResult {
    use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
    let mouse = |kind, column, row| {
        Msg::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    };
    let mut app = daemon_app(100, 30).await?;
    let buffer = render(&app)?;
    let find = |text: &str| {
        (0..buffer.area.height)
            .find(|y| row(&buffer, *y).contains(text))
            .ok_or(format!("no {text} row"))
    };
    let (started, failed) = (find("lane started")?, find("compose failed")?);
    let left = app.daemon_log_view.get().x;
    let left_button = MouseButton::Left;

    app.update(mouse(MouseEventKind::Down(left_button), left, started));
    assert!(app.daemon.copy.is_none(), "a press alone selects nothing");
    app.update(mouse(MouseEventKind::Drag(left_button), left + 9, failed));
    assert!(app.daemon.copy.is_some_and(|copy| copy.anchor.is_some()));
    let text = copied(&app.update(mouse(MouseEventKind::Up(left_button), left + 9, failed)))?;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].contains("lane started"), "{text}");
    assert_eq!(lines[1].chars().count(), 10, "{text}");
    assert!(app.daemon.copy.is_none());

    // A click without a drag copies nothing.
    app.update(mouse(MouseEventKind::Down(left_button), left, started));
    let effects = app.update(mouse(MouseEventKind::Up(left_button), left, started));
    assert!(effects.is_empty(), "{effects:?}");
    Ok(())
}

/// The fake's doctor facts plus eight units whose stop timeouts outlast the
/// launchd agent's default, more checks than the box beside the facts shows
/// at 100x30.
fn crowded_doctor() -> serde_json::Value {
    let mut facts = doctor_facts();
    facts["service"] = json!({
        "manager": "launchd agent", "path": "/home/acme/Library/LaunchAgents/devlanes.plist",
        "installed": false, "stop_window_s": 5, "window_source": "default",
        "recommended_window_s": 60,
    });
    facts["stop_timeouts"] = (0..8)
        .map(|i| json!({"project": "acme", "unit": format!("api-{i}"), "timeout_ms": 10_000}))
        .collect();
    facts
}

/// The daemon tab at 100x30 with [`crowded_doctor`]'s checks, drawn once.
async fn crowded_app() -> Result<App, Box<dyn std::error::Error>> {
    let mut app = daemon_app(100, 30).await?;
    app.update(Msg::DaemonInfo {
        status: Ok(daemon_status()),
        doctor: Ok(crowded_doctor()),
    });
    render(&app)?;
    Ok(app)
}

/// A mouse event at screen `(column, row)`.
fn mouse(kind: crossterm::event::MouseEventKind, column: u16, row: u16) -> Msg {
    Msg::Mouse(crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

#[tokio::test]
async fn the_checks_box_scrolls_when_its_checks_do_not_fit() -> TestResult {
    use crossterm::event::MouseEventKind;
    let mut app = crowded_app().await?;
    let view = app.daemon_checks_view.get();
    assert!(view.max_scroll > 0, "{view:?}");
    let text = screen(&render(&app)?);
    assert!(!text.contains("i shows all"), "{text}");
    assert!(text.contains(&format!("rows 1-{}/", view.height)), "{text}");
    let last_row: String = app
        .daemon_check_blocks()
        .last()
        .and_then(|check| check.lines.last())
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .ok_or("no checks")?;
    let last_row = last_row.trim();
    assert!(!text.contains(last_row), "{last_row:?} is below: {text}");

    // Tab hands j k g G to the checks; the log stays where it is.
    app.update(key(KeyCode::Tab));
    assert_eq!(app.daemon.focus, DaemonBox::Checks);
    app.update(key(KeyCode::Char('j')));
    assert_eq!(app.daemon.checks_scroll, 1);
    assert_eq!((app.daemon.scroll, app.daemon.paused), (0, false));
    app.update(key(KeyCode::Char('G')));
    assert_eq!(app.daemon.checks_scroll, view.max_scroll);
    let text = screen(&render(&app)?);
    assert!(text.contains(last_row), "{last_row:?} shows: {text}");
    app.update(key(KeyCode::Char('g')));
    assert_eq!(app.daemon.checks_scroll, 0);
    // Shift+Tab goes back to the log, which k scrolls again.
    app.update(key(KeyCode::BackTab));
    assert_eq!(app.daemon.focus, DaemonBox::Log);
    let lines: Vec<String> = (0..60)
        .map(|i| format!("2026-10-21T10:00:{i:02}.000Z  INFO devlanes_daemon: entry {i}"))
        .collect();
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines,
        position: LogPosition {
            offset: 5000,
            file: (1, 2),
        },
        reset: false,
    })));
    render(&app)?;
    app.update(key(KeyCode::Char('k')));
    assert_eq!((app.daemon.scroll, app.daemon.checks_scroll), (1, 0));

    // The wheel scrolls the box under the pointer, whatever has focus.
    app.update(mouse(MouseEventKind::ScrollDown, view.x, view.y));
    assert_eq!(app.daemon.checks_scroll, 3.min(view.max_scroll));
    assert_eq!(app.daemon.scroll, 1);
    let log = app.daemon_log_view.get();
    app.update(mouse(MouseEventKind::ScrollUp, log.x, log.y));
    assert_eq!(app.daemon.scroll, 4);
    assert_eq!(app.daemon.checks_scroll, 3.min(view.max_scroll));
    // `i` lists every check from the top again.
    app.update(key(KeyCode::Char('i')));
    assert_eq!(app.daemon.checks_scroll, 0);
    Ok(())
}

#[tokio::test]
async fn checks_text_is_selected_and_copied_like_the_log() -> TestResult {
    let mut app = crowded_app().await?;
    app.update(key(KeyCode::Tab));
    assert!(app.update(key(KeyCode::Char('c'))).is_empty());
    let cursor = app
        .daemon
        .checks_copy
        .map(|c| c.cursor)
        .ok_or("no copy mode")?;
    assert_eq!((cursor.entry, cursor.line, cursor.column), (0, 0, 0));
    assert!(app.daemon.copy.is_none());
    let text = screen(&render(&app)?);
    assert!(text.contains("selecting · v mark · y copy"), "{text}");
    assert!(text.contains("Y copy whole checks"), "{text}");

    // y without a mark: the cursor's row as drawn.
    let row = copied(&app.update(key(KeyCode::Char('y'))))?;
    assert!(row.starts_with("fail tls"), "{row}");
    assert!(!row.contains('\n'), "{row}");
    assert!(app.daemon.checks_copy.is_none());

    // v, j and $: the first row and the whole fix row under it.
    app.update(key(KeyCode::Char('c')));
    app.update(key(KeyCode::Char('v')));
    app.update(key(KeyCode::Char('j')));
    app.update(key(KeyCode::Char('$')));
    let rows = copied(&app.update(key(KeyCode::Char('y'))))?;
    let lines: Vec<&str> = rows.lines().collect();
    assert_eq!(lines.len(), 2, "{rows}");
    assert_eq!(lines[1].trim_start(), "fix: devlanes certs trust", "{rows}");

    // Y: the whole check with its fix, unwrapped.
    app.update(key(KeyCode::Char('c')));
    let whole = copied(&app.update(key(KeyCode::Char('Y'))))?;
    assert!(whole.starts_with("fail tls: "), "{whole}");
    assert!(whole.ends_with("\n  fix: devlanes certs trust"), "{whole}");

    // The service check carries its units on a line of their own.
    let checks = app.daemon_check_blocks();
    let service = checks
        .iter()
        .find(|check| check.plain.starts_with("warn service: "))
        .ok_or("no service check")?;
    assert!(
        service.plain.contains("\n  units: acme:api-0 (10s), "),
        "{}",
        service.plain
    );

    // Esc leaves without copying; q there does not quit.
    app.update(key(KeyCode::Char('c')));
    assert!(app.update(key(KeyCode::Char('q'))).is_empty());
    assert!(app.daemon.checks_copy.is_none());
    Ok(())
}

#[tokio::test]
async fn dragging_over_the_checks_copies_their_text() -> TestResult {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut app = crowded_app().await?;
    let view = app.daemon_checks_view.get();
    let left = MouseButton::Left;
    app.update(mouse(MouseEventKind::Down(left), view.x, view.y));
    assert_eq!(
        app.daemon.focus,
        DaemonBox::Checks,
        "a press focuses the box"
    );
    assert!(
        app.daemon.checks_copy.is_none(),
        "a press alone selects nothing"
    );
    // Past the right edge: the selection ends at the box's last column.
    let right = view.x + u16::try_from(view.width)? + 5;
    app.update(mouse(MouseEventKind::Drag(left), right, view.y + 1));
    assert!(
        app.daemon
            .checks_copy
            .is_some_and(|copy| copy.anchor.is_some())
    );
    let text = copied(&app.update(mouse(MouseEventKind::Up(left), right, view.y + 1)))?;
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("fail tls"), "{text}");
    assert_eq!(lines[1].trim_start(), "fix: devlanes certs trust", "{text}");
    assert!(app.daemon.checks_copy.is_none() && app.daemon.copy.is_none());

    // A click without a drag copies nothing; one in the log focuses it.
    app.update(mouse(MouseEventKind::Down(left), view.x, view.y));
    let effects = app.update(mouse(MouseEventKind::Up(left), view.x, view.y));
    assert!(effects.is_empty(), "{effects:?}");
    let log = app.daemon_log_view.get();
    app.update(mouse(MouseEventKind::Down(left), log.x, log.y));
    app.update(mouse(MouseEventKind::Up(left), log.x, log.y));
    assert_eq!(app.daemon.focus, DaemonBox::Log);
    Ok(())
}

#[tokio::test]
async fn copy_mode_on_the_checks_scrolls_them_to_the_cursor() -> TestResult {
    let mut app = crowded_app().await?;
    let view = app.daemon_checks_view.get();
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Char('c')));
    app.update(key(KeyCode::Char('G')));
    assert_eq!(app.daemon.checks_scroll, view.max_scroll);
    let rows: usize = app
        .daemon_check_blocks()
        .iter()
        .map(|c| c.lines.len())
        .sum();
    // k up to the first row shown: the box stays; one more scrolls a row.
    for _ in 0..view.height - 1 {
        app.update(key(KeyCode::Char('k')));
    }
    assert_eq!(app.daemon.checks_scroll, rows - view.height);
    app.update(key(KeyCode::Char('k')));
    assert_eq!(app.daemon.checks_scroll, rows - view.height - 1);
    app.update(key(KeyCode::Char('g')));
    assert_eq!(app.daemon.checks_scroll, 0);
    Ok(())
}

#[tokio::test]
async fn a_refresh_with_other_checks_ends_their_copy_mode() -> TestResult {
    let mut app = crowded_app().await?;
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Char('G')));
    app.update(key(KeyCode::Char('c')));
    assert!(app.daemon.checks_copy.is_some());
    // The same checks again keep the selection.
    app.update(Msg::DaemonInfo {
        status: Ok(daemon_status()),
        doctor: Ok(crowded_doctor()),
    });
    assert!(app.daemon.checks_copy.is_some());
    // Fewer checks: the selection would point at other text, and the scroll
    // stays within the rows left.
    app.update(Msg::DaemonInfo {
        status: Ok(daemon_status()),
        doctor: Ok(doctor_facts()),
    });
    assert!(app.daemon.checks_copy.is_none());
    let rows: usize = app
        .daemon_check_blocks()
        .iter()
        .map(|c| c.lines.len())
        .sum();
    let height = app.daemon_checks_view.get().height;
    assert_eq!(app.daemon.checks_scroll, rows.saturating_sub(height));
    Ok(())
}

#[tokio::test]
async fn only_one_box_selects_at_a_time() -> TestResult {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut app = crowded_app().await?;
    let view = app.daemon_checks_view.get();
    let left = MouseButton::Left;
    // Log copy mode, then a drag over the checks: the checks select instead.
    app.update(key(KeyCode::Char('c')));
    assert!(app.daemon.copy.is_some());
    app.update(mouse(MouseEventKind::Down(left), view.x, view.y));
    assert!(app.daemon.copy.is_none());
    app.update(mouse(MouseEventKind::Drag(left), view.x + 4, view.y));
    assert!(app.daemon.checks_copy.is_some() && app.daemon.copy.is_none());
    // A drag into the log stops at the checks' last row shown.
    let log = app.daemon_log_view.get();
    app.update(mouse(MouseEventKind::Drag(left), view.x + 4, log.y + 2));
    let cursor = app
        .daemon
        .checks_copy
        .map(|c| c.cursor)
        .ok_or("no selection")?;
    let last = app.daemon_check_blocks().len() - 1;
    assert!(cursor.entry <= last, "{cursor:?}");
    assert!(app.daemon.copy.is_none());
    app.update(mouse(MouseEventKind::Up(left), view.x + 4, log.y + 2));
    // Checks copy mode, then a press in the log ends it.
    app.update(key(KeyCode::Char('c')));
    assert!(app.daemon.checks_copy.is_some());
    let buffer = render(&app)?;
    let compose = (0..buffer.area.height)
        .find(|y| row(&buffer, *y).contains("compose failed"))
        .ok_or("no compose row")?;
    app.update(mouse(MouseEventKind::Down(left), log.x, compose));
    assert!(app.daemon.checks_copy.is_none());
    assert_eq!(app.daemon.focus, DaemonBox::Log);
    Ok(())
}

#[tokio::test]
async fn below_100_columns_only_the_checks_under_the_facts_scroll() -> TestResult {
    use crossterm::event::{MouseButton, MouseEventKind};
    let mut app = daemon_app(80, 24).await?;
    app.update(Msg::DaemonInfo {
        status: Ok(daemon_status()),
        doctor: Ok(crowded_doctor()),
    });
    let text = screen(&render(&app)?);
    let view = app.daemon_checks_view.get();
    assert!(view.max_scroll > 0, "{view:?}");
    // The facts take the box's first two rows; the checks start under them.
    let box_area = app.daemon_checks_box.get();
    assert_eq!(view.y, box_area.y + 3, "{text}");
    assert!(text.contains("socket"), "{text}");
    // The wheel over the facts scrolls the checks; the facts stay.
    app.update(mouse(
        MouseEventKind::ScrollDown,
        box_area.x + 2,
        box_area.y + 1,
    ));
    assert_eq!(app.daemon.checks_scroll, 3.min(view.max_scroll));
    let text = screen(&render(&app)?);
    assert!(text.contains("socket"), "{text}");
    // A drag over the first row shown copies what it shows.
    app.update(key(KeyCode::Tab));
    app.update(key(KeyCode::Char('g')));
    let left = MouseButton::Left;
    app.update(mouse(MouseEventKind::Down(left), view.x, view.y));
    app.update(mouse(MouseEventKind::Drag(left), view.x + 7, view.y));
    let row = copied(&app.update(mouse(MouseEventKind::Up(left), view.x + 7, view.y)))?;
    assert_eq!(row, "fail tls");
    Ok(())
}

#[tokio::test]
async fn copy_mode_jumps_scroll_the_long_log_to_the_cursor() -> TestResult {
    let mut app = daemon_app(100, 30).await?;
    let lines: Vec<String> = (0..300)
        .map(|i| {
            format!(
                "2026-10-21T10:{:02}:{:02}.000Z  INFO devlanes_daemon: entry {i}",
                i / 60,
                i % 60
            )
        })
        .collect();
    app.update(Msg::DaemonLog(Ok(LogChunk {
        lines,
        position: LogPosition {
            offset: 9000,
            file: (1, 2),
        },
        reset: false,
    })));
    render(&app)?;
    let max = app.daemon_log_view.get().max_scroll;
    assert!(max > 200, "{max}");
    app.update(key(KeyCode::Char('c')));
    // g: the oldest line, with the oldest entries filling the box.
    app.update(key(KeyCode::Char('g')));
    assert_eq!(app.daemon.copy.map(|c| c.cursor.entry), Some(0));
    assert_eq!(app.daemon.scroll, max);
    app.update(key(KeyCode::Char('G')));
    assert_eq!(app.daemon.scroll, 0);
    // k past the top of the box scrolls by one entry, not more.
    let text = screen(&render(&app)?);
    let top = text
        .lines()
        .find_map(|line| line.split("entry ").nth(1))
        .and_then(|rest| rest.split_whitespace().next())
        .and_then(|n| n.parse::<usize>().ok())
        .ok_or("no entry on screen")?;
    let visible = 299 - top;
    for _ in 0..=visible {
        app.update(key(KeyCode::Char('k')));
    }
    assert_eq!(app.daemon.scroll, 1, "one entry above the old top");
    Ok(())
}

#[tokio::test]
async fn a_log_that_fits_does_not_scroll() -> TestResult {
    let mut app = daemon_app(120, 40).await?;
    let before = screen(&render(&app)?);
    for _ in 0..3 {
        app.update(key(KeyCode::Char('k')));
    }
    app.update(key(KeyCode::Char('g')));
    assert_eq!(app.daemon.scroll, 0, "everything is in view already");
    assert!(!app.daemon.paused);
    assert_eq!(screen(&render(&app)?), before);
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
    assert_screen!("clean_confirm_80x24", screen(&render(&app)?));
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
        "copy mode: v select, y copy, Y whole entries as in the file",
        "move j k g G and c between the checks and the log",
        "restart, stop or start the daemon",
        "clean every lane whose worktree is gone",
        "hide the version notice",
        "monitor, daemon; again or esc returns",
    ] {
        assert!(text.contains(expected), "missing {expected:?}:\n{text}");
    }
    Ok(())
}

// ---- full loop -------------------------------------------------------------------

/// One step of a [`run_keys`] script.
pub enum Step {
    /// Boxed: `Msg` is far larger than the other variant (over clippy's
    /// limit on Windows, where paths are wider).
    Key(Box<Msg>),
    /// Wait until the drawn screen contains this text, as after a key whose
    /// effect needs a daemon answer (the profile picker's list).
    Screen(&'static str),
}

/// Plain key presses as steps.
pub fn presses(keys: Vec<Msg>) -> Vec<Step> {
    keys.into_iter()
        .map(|key| Step::Key(Box::new(key)))
        .collect()
}

/// Runs `drive` against `api` through `steps`, then quits once every text of
/// `until` is on screen and `calls_done` holds for the calls the fake saw:
/// what a test asserts arrives on its own (answers, refreshes, log reads), so
/// it is waited for, never guessed with a sleep. Returns the last drawn screen.
pub async fn run_keys(
    api: Arc<FakeApi>,
    steps: Vec<Step>,
    until: &[&str],
    calls_done: impl Fn(&[String]) -> bool,
) -> Result<TestBackend, Box<dyn std::error::Error>> {
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(64);
    let input = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let backend = crate::SharedBackend::new(80, 24);
    let mut terminal = Terminal::new(backend.clone())?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = {
        let api = Arc::clone(&api);
        let backend = backend.clone();
        async move {
            crate::wait_for_calls(&api, |calls| calls.iter().any(|c| c == "status")).await;
            for step in steps {
                match step {
                    Step::Key(msg) => {
                        if let Msg::Key(event) = *msg {
                            keys_tx.send(Ok(TermEvent::Key(event))).await.ok();
                        }
                    }
                    Step::Screen(text) => backend.wait_for(text).await,
                }
                tokio::time::sleep(Duration::from_millis(15)).await;
            }
            for text in until {
                backend.wait_for(text).await;
            }
            crate::wait_for_calls(&api, calls_done).await;
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
        ..crate::hermetic_options()
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
    Ok(backend.lock().clone())
}

#[tokio::test]
async fn drive_cleans_only_after_the_name_is_typed() -> TestResult {
    let api = Arc::new(FakeApi::with_lanes(mixed_lanes()));
    let mut keys = vec![shifted('C'), key(KeyCode::Esc), shifted('C')];
    keys.extend("acme@0".chars().map(|c| key(KeyCode::Char(c))));
    keys.push(key(KeyCode::Enter));
    // The clean's notice, and the status refresh after it.
    let refreshed = |calls: &[String]| {
        calls
            .iter()
            .skip_while(|c| !c.starts_with("lane.clean"))
            .any(|c| c == "status")
    };
    let terminal = run_keys(
        Arc::clone(&api),
        presses(keys),
        &["cleaned acme@0"],
        refreshed,
    )
    .await?;
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
    let header = row(terminal.buffer(), 0);
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
    // The log read and the doctor answer arrive separately; both are waited for.
    let terminal = run_keys(
        Arc::clone(&api),
        presses(vec![shifted('D')]),
        &["compose failed", "1 fail · 1 warn"],
        |_| true,
    )
    .await?;
    let text = screen(terminal.buffer());
    assert!(text.contains("ERROR daemon::lifecycle"), "{text}");
    assert!(text.contains("compose failed"), "{text}");
    assert!(text.contains("fix: devlanes certs trust"), "{text}");
    assert_clean(terminal.buffer(), "drive daemon tab");
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

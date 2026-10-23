//! Units a previous daemon left running (`orphaned`): how they show and how
//! they are stopped.

use crossterm::event::KeyCode;
use devlanes_proto::{LaneSummary, Method, ORPHANED, RpcError, UnitSummary};
use devlanes_tui::{
    DaemonApi,
    app::{App, Effect, Msg},
};
use ratatui::style::Color;
use serde_json::json;

use crate::daemon::shifted;
use crate::errors::{key, render, row, screen};
use crate::fake::{FakeApi, lane, unit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// `acme@0:api` as a restarted daemon reports a process it found still running.
fn orphan() -> UnitSummary {
    UnitSummary {
        pid: Some(4242),
        started_at_ms: Some(1_700_000_000_000),
        orphaned_since_ms: Some(1_700_000_600_000),
        color: Some("blue".into()),
        ..unit("acme@0", "api", ORPHANED)
    }
}

/// Lane 0 runs an orphan next to a supervised unit; lane 1 is idle.
fn lanes() -> Vec<LaneSummary> {
    vec![
        lane(
            "acme",
            Some(0),
            "main",
            "running",
            vec![orphan(), unit("acme@0", "web", "ready")],
        ),
        lane("acme", Some(1), "feat", "idle", Vec::new()),
    ]
}

/// The loaded app with the orphan selected, and the effects of selecting it.
async fn orphan_selected(api: &FakeApi) -> (App, Vec<Effect>) {
    let mut app = App::new(80, 24);
    app.update(Msg::Status(api.status().await));
    let mut effects = Vec::new();
    for _ in 0..5 {
        if app.selected_unit().is_some_and(|u| u.name == "api") {
            break;
        }
        effects.extend(app.update(key(KeyCode::Char('j'))));
    }
    (app, effects)
}

/// Sends a control effect to the fake daemon and feeds the answer back.
async fn apply(app: &mut App, api: &FakeApi, effect: Effect) -> Result<(), String> {
    let Effect::Control {
        method,
        lane,
        selection,
        label,
        target,
    } = effect
    else {
        return Err(format!("not a control effect: {effect:?}"));
    };
    let result = api.control(method, lane, selection).await;
    app.update(Msg::ActionDone {
        label,
        target,
        result,
    });
    Ok(())
}

#[tokio::test]
async fn an_orphan_shows_its_pid_and_has_no_terminal() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, effects) = orphan_selected(&api).await;
    assert!(app.selected_unit().is_some_and(|u| u.name == "api"));
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Attach { .. })),
        "an orphan has no stream to attach to: {effects:?}"
    );
    assert!(app.attached.is_none());
    let buffer = render(&app)?;
    let text = screen(&buffer);
    for expected in [
        "◆ 1 orphaned",
        "1 orphaned",
        "◆ api",
        "orphaned · pid 4242",
        "┌ api  orphaned · pid 4242",
        "An earlier daemon started this",
        "x stops it.",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    assert_screen!("orphan_80x24", text);
    // Amber, not red and not the unit's own color: red stays for failures.
    // Off the selected row, which is plain reverse video in 16 colors.
    app.update(key(KeyCode::Char('g')));
    let buffer = render(&app)?;
    let y = (0..buffer.area.height)
        .find(|y| row(&buffer, *y).contains("◆ api"))
        .ok_or("no api row")?;
    let x = (0..buffer.area.width)
        .find(|x| buffer[(*x, y)].symbol() == "◆")
        .ok_or("no marker")?;
    let marker = buffer[(x, y)].fg;
    assert!(!matches!(marker, Color::Blue | Color::Reset), "{marker:?}");
    assert_ne!(marker, Color::Red, "not the failure red");
    Ok(())
}

#[tokio::test]
async fn enter_on_an_orphan_says_there_is_no_terminal() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = orphan_selected(&api).await;
    let effects = app.update(key(KeyCode::Enter));
    assert!(effects.is_empty(), "{effects:?}");
    assert_eq!(app.focus, devlanes_tui::app::Focus::List);
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text == "no terminal: an earlier daemon started this process")
    );
    // s does not start what already runs.
    assert!(app.update(key(KeyCode::Char('s'))).is_empty());
    Ok(())
}

#[tokio::test]
async fn x_stops_an_orphan_through_the_daemon() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = orphan_selected(&api).await;
    let mut effects = app.update(key(KeyCode::Char('x')));
    let effect = effects.pop().ok_or("x sent nothing")?;
    assert!(effects.is_empty(), "{effects:?}");
    apply(&mut app, &api, effect).await?;
    assert_eq!(
        api.calls().last().map(String::as_str),
        Some(r#"Stop acme@0 ["acme@0:api"]"#)
    );
    assert!(app.errors.is_empty(), "{:?}", app.errors);
    // The daemon reports the process gone; the unit attaches like any stopped one.
    let mut stopped = orphan();
    stopped.state = "stopped".into();
    stopped.pid = None;
    let effects = app.update(Msg::Event(devlanes_proto::Event {
        seq: 2,
        event: devlanes_proto::events::UNIT_CHANGED.into(),
        data: json!({ "unit": stopped }),
    }));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Attach { .. })),
        "{effects:?}"
    );
    assert!(!screen(&render(&app)?).contains("orphaned"));
    Ok(())
}

#[tokio::test]
async fn lane_stop_and_clean_cover_orphans() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let mut app = App::new(80, 24);
    app.update(Msg::Status(api.status().await));
    assert_eq!(
        app.selected_lane().map(|l| l.reference.as_str()),
        Some("acme@0")
    );
    // The lane opens on its problem; the whole-lane row is the first row.
    assert!(app.selected_unit().is_some_and(|u| u.name == "api"));
    app.update(key(KeyCode::Char('g')));
    assert!(app.selected_unit().is_none());
    // Stop on the lane: the daemon terminates its orphans with the rest.
    let effect = app
        .update(key(KeyCode::Char('x')))
        .pop()
        .ok_or("x sent nothing")?;
    apply(&mut app, &api, effect).await?;
    assert_eq!(
        api.calls().last().map(String::as_str),
        Some("Stop acme@0 []")
    );
    // Restart names running units, the orphan among them.
    let effect = app
        .update(key(KeyCode::Char('r')))
        .pop()
        .ok_or("r sent nothing")?;
    let Effect::Control {
        method, selection, ..
    } = &effect
    else {
        return Err(format!("unexpected {effect:?}").into());
    };
    assert_eq!(*method, Method::Restart);
    assert_eq!(selection.units, ["acme@0:api", "acme@0:web"]);
    // Clean asks first, then sends the normal clean.
    app.update(shifted('C'));
    let effects = app.update(key(KeyCode::Enter));
    assert!(
        matches!(effects.as_slice(), [Effect::CleanLane { lane }] if lane == "acme@0"),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn starting_an_orphan_shows_the_daemons_refusal() -> TestResult {
    let mut api = FakeApi::with_lanes(lanes());
    api.control_error = Some(RpcError::new(
        "unit_orphaned",
        json!({
            "unit": "api",
            "lane": "acme@0",
            "pid": 4242,
            "started_at_ms": 1_700_000_000_000_u64,
            "orphaned_since_ms": 1_700_000_600_000_u64,
            "command": "devlanes stop api --lane acme@0",
        }),
        1,
    ));
    let (mut app, _) = orphan_selected(&api).await;
    // `s` on an orphan sends nothing; a start from elsewhere (an older
    // client, a group) comes back refused.
    let effect = Effect::Control {
        method: Method::Start,
        lane: "acme@0".into(),
        selection: devlanes_proto::Selection {
            units: vec!["acme@0:api".into()],
            ..Default::default()
        },
        label: "start acme@0:api".into(),
        target: devlanes_tui::app::Target::Unit {
            lane: "acme@0".into(),
            unit: "api".into(),
        },
    };
    apply(&mut app, &api, effect).await?;
    // The status line carries the refusal; the pane keeps the full text.
    let notice = app.notice.as_ref().ok_or("no notice")?;
    assert!(notice.error);
    assert!(
        notice
            .text
            .contains("acme@0:api is already running as pid 4242"),
        "{}",
        notice.text
    );
    let header = row(&render(&app)?, 0);
    assert!(header.contains("✕ start acme@0:api "), "{header}");
    let entry = app.errors.front().ok_or("not in the error list")?;
    assert!(
        entry
            .worded
            .summary
            .ends_with("started by an earlier daemon; x stops it"),
        "{}",
        entry.worded.summary
    );
    assert_eq!(
        entry.worded.command.as_deref(),
        Some("devlanes stop api --lane acme@0")
    );
    assert!(matches!(
        app.errors.front().map(|e| &e.worded.code),
        Some(Some(code)) if code == "unit_orphaned"
    ));
    Ok(())
}

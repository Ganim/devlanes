//! Mouse clicks in the lane list, and resizing on zoom for PTY and pipe units.

use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use devlanes_proto::RpcError;
use devlanes_tui::{
    ApiError,
    app::{App, Effect, Msg, RowId, Section, row_indent},
};
use serde_json::json;

use super::{TestResult, attach_in, draw, feed, focus_terminal, key, loaded, select_unit};
use crate::fake::{FakeApi, mixed_lanes};

/// A left click at `(column, row)`.
fn click(column: u16, row: u16) -> Msg {
    Msg::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// A wheel step down (`down`) or up at `(column, row)`.
fn wheel(column: u16, row: u16, down: bool) -> Msg {
    Msg::Mouse(MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        column,
        row,
        modifiers: KeyModifiers::NONE,
    })
}

/// The screen row of list row `index`, and the columns of its text and of
/// its fold arrow.
fn place(app: &App, index: usize) -> Result<(u16, u16, u16), Box<dyn std::error::Error>> {
    let list = app.areas().and_then(|a| a.list).ok_or("no list")?;
    let row = app.rows().get(index).ok_or("no such row")?.clone();
    let y = list.y + u16::try_from(index - app.list_offset)?;
    let arrow = list.x + row_indent(row.depth);
    Ok((y, arrow + 4, arrow))
}

/// The list index of the row `id`.
fn row_of(app: &App, id: &RowId) -> Option<usize> {
    app.rows().iter().position(|row| row.id == *id)
}

/// A running lane of `count` services (`api-*` run, `wrk-*` do not) and a
/// finished setup step, folded under infra's `setup` group.
fn big_lane(count: usize) -> Vec<devlanes_proto::LaneSummary> {
    let mut units: Vec<_> = (0..count)
        .map(|i| {
            let (family, state) = if i % 2 == 0 {
                ("api", "ready")
            } else {
                ("wrk", "idle")
            };
            crate::fake::unit("acme@0", &format!("{family}-{i:02}"), state)
        })
        .collect();
    let mut setup = crate::fake::unit("acme@0", "setup.db", "exited");
    setup.kind = "task".into();
    setup.exit_code = Some(0);
    units.push(setup);
    vec![crate::fake::lane("acme", Some(0), "main", "running", units)]
}

#[tokio::test]
async fn clicks_select_rows_and_fold_sections_and_groups() -> TestResult {
    let api = FakeApi::with_lanes(big_lane(6));
    let (mut app, _) = loaded(&api, 100, 30).await;
    let services = RowId::Section(Section::Services);
    let index = row_of(&app, &services).ok_or("no services section")?;
    let (y, text, _) = place(&app, index)?;
    app.update(click(text, y));
    assert_eq!(app.selected_id(), Some(&services));
    assert!(
        row_of(&app, &RowId::Unit("api-00".into())).is_some(),
        "the first click only selects"
    );
    // A second click on the selected header folds it, like Enter; a third
    // unfolds it.
    app.update(click(text, y));
    assert!(row_of(&app, &RowId::Unit("api-00".into())).is_none());
    app.update(click(text, y));
    // A click on another header's arrow unfolds it at once.
    let setup = RowId::Group(Section::Infra, "setup".into());
    let index = row_of(&app, &setup).ok_or("no setup group")?;
    assert!(
        row_of(&app, &RowId::Unit("setup.db".into())).is_none(),
        "folded at first"
    );
    let (y, _, arrow) = place(&app, index)?;
    app.update(click(arrow, y));
    assert_eq!(app.selected_id(), Some(&setup));
    assert!(row_of(&app, &RowId::Unit("setup.db".into())).is_some());
    // A click on a unit selects it and the pane follows.
    let index = row_of(&app, &RowId::Unit("api-02".into())).ok_or("no api-02")?;
    let (y, text, _) = place(&app, index)?;
    let effects = app.update(click(text, y));
    assert_eq!(app.selected_unit().map(|u| u.name.as_str()), Some("api-02"));
    assert!(
        matches!(attach_in(&effects), Some((_, false))),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_double_click_on_a_running_unit_types_into_it() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"$ ");
    let index = row_of(&app, &RowId::Unit("api".into())).ok_or("no api row")?;
    let (y, text, _) = place(&app, index)?;
    app.update(click(text, y));
    let effects = app.update(click(text, y));
    let (generation, interactive) = attach_in(&effects).ok_or("no interactive attach")?;
    assert!(interactive);
    feed(&mut app, generation, b"$ ");
    assert_eq!(app.focus, devlanes_tui::app::Focus::Terminal);
    // A click on the list returns to watching.
    app.update(click(text, y));
    assert_eq!(app.focus, devlanes_tui::app::Focus::List);
    Ok(())
}

#[tokio::test]
async fn the_wheel_scrolls_the_list_and_keeps_the_selection() -> TestResult {
    let api = FakeApi::with_lanes(big_lane(60));
    let (mut app, _) = loaded(&api, 100, 20).await;
    let selected = app.selected.clone();
    let list = app.areas().and_then(|a| a.list).ok_or("no list")?;
    let before = app.list_offset;
    app.update(wheel(list.x + 2, list.y + 2, true));
    assert_eq!(app.list_offset, before + 3);
    assert_eq!(
        app.selected, selected,
        "the wheel moves the view, not the row"
    );
    app.update(wheel(list.x + 2, list.y + 2, false));
    assert_eq!(app.list_offset, before);
    Ok(())
}

#[tokio::test]
async fn tabs_chips_and_footer_hints_are_clickable() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 120, 30).await;
    // Hit areas come from the drawn frame.
    let text = draw(&app)?.backend().to_string();
    let rows: Vec<&str> = text.lines().collect();
    let column = |line: &str, needle: &str| -> Option<u16> {
        let byte = line.find(needle)?;
        u16::try_from(line[..byte].chars().count()).ok()
    };
    let top = rows.first().ok_or("no top bar")?.trim_matches('"');
    let shop = column(top, "shop").ok_or("no shop tab")?;
    app.update(click(shop, 0));
    assert_eq!(app.project(), Some("shop"));
    let text = draw(&app)?.backend().to_string();
    let top = text.lines().next().ok_or("no top bar")?.trim_matches('"');
    let monitor = column(top, "M monitor").ok_or("no monitor button")?;
    app.update(click(monitor, 0));
    assert_eq!(app.view, devlanes_tui::app::View::Monitor);
    // shop has one lane, so no lane bar: the list starts under the top bar.
    let text = draw(&app)?.backend().to_string();
    assert!(
        !text.lines().nth(1).unwrap_or_default().contains("lanes"),
        "{text}"
    );
    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Char('!')));
    let text = draw(&app)?.backend().to_string();
    let bar = text.lines().nth(1).ok_or("no lane bar")?.trim_matches('"');
    assert!(bar.contains("feat/login @1"), "{bar}");
    assert!(bar.contains("0-9 or [ ] switch lanes"), "{bar}");
    let chip = column(bar, "feat/login").ok_or("no chip")?;
    app.update(click(chip, 1));
    assert_eq!(
        app.selected_lane().map(|l| l.reference.as_str()),
        Some("acme@1")
    );
    // The chip selects the lane itself, as a click on the list's title does.
    assert_eq!(app.selected_id(), Some(&RowId::WholeLane));
    let index = row_of(&app, &RowId::Section(Section::Services)).ok_or("no services")?;
    let (y, text_x, _) = place(&app, index)?;
    app.update(click(text_x, y));
    assert_eq!(app.selected_id(), Some(&RowId::Section(Section::Services)));
    let list = app.areas().and_then(|a| a.list).ok_or("no list")?;
    draw(&app)?;
    app.update(click(list.x + 3, list.y - 1));
    assert_eq!(app.selected_id(), Some(&RowId::WholeLane));
    // The footer's `s start lane` presses s.
    let text = draw(&app)?.backend().to_string();
    let footer = text.lines().last().ok_or("no footer")?.trim_matches('"');
    let start = column(footer, "s start lane").ok_or(format!("no start hint: {footer}"))?;
    let effects = app.update(click(start, 29));
    assert!(
        matches!(effects.as_slice(), [Effect::Control { lane, .. }] if lane == "acme@1"),
        "{effects:?}"
    );
    Ok(())
}

/// The `ResizePty` sizes among `effects`.
fn resizes(effects: &[Effect]) -> Vec<(u16, u16)> {
    effects
        .iter()
        .filter_map(|e| match e {
            Effect::ResizePty { columns, rows } => Some((*columns, *rows)),
            _ => None,
        })
        .collect()
}

/// An opened stream for `generation`.
fn opened(generation: u64) -> Msg {
    Msg::AttachOpened {
        generation,
        result: Ok(devlanes_proto::AttachResult {
            stream_id: 2,
            unit: String::new(),
            lane: String::new(),
        }),
    }
}

#[tokio::test]
async fn zoom_resizes_and_a_refused_resize_does_not_stop_later_ones() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let generation = focus_terminal(&mut app, b"$ ").ok_or("no interactive attach")?;
    let normal = app.attach_size();
    // A PTY unit: zoom and unzoom both resize it (`z` from the list).
    app.update(key(KeyCode::Esc));
    let zoomed = app.update(key(KeyCode::Char('z')));
    let big = app.attach_size();
    assert_eq!(resizes(&zoomed), [big]);
    assert_eq!(resizes(&app.update(key(KeyCode::Char('z')))), [normal]);

    // A PTY unit whose resize failed once: the failure is reported, and the
    // next zoom still resizes it. It used to be taken for a pipe unit and
    // left at its old width for the rest of the session.
    app.update(Msg::ResizeRefused {
        generation,
        error: ApiError::Rpc(RpcError::new(
            "resize_failed",
            json!({"message": "Bad file descriptor"}),
            1,
        )),
    });
    assert!(app.notice.as_ref().is_some_and(|n| n.error));
    let effects = app.update(key(KeyCode::Char('z')));
    assert_eq!(resizes(&effects), [big]);
    Ok(())
}

#[tokio::test]
async fn zoom_from_watching_renders_output_at_the_zoomed_width() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 200, 40).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let normal = app.attach_size();
    // Watching: `z` reopens the stream as the PTY's owner at the zoomed size.
    let effects = app.update(key(KeyCode::Char('z')));
    let big = app.attach_size();
    assert!(big.0 > normal.0 + 40, "{normal:?} -> {big:?}");
    let reopened = effects
        .iter()
        .find_map(|e| match e {
            Effect::Attach {
                generation,
                columns,
                rows,
                interactive: true,
                ..
            } if (*columns, *rows) == big => Some(*generation),
            _ => None,
        })
        .ok_or(format!("no interactive attach at {big:?}: {effects:?}"))?;
    // Once open, the size is sent again in case another viewer changed it.
    assert_eq!(resizes(&app.update(opened(reopened))), [big]);
    // A line wider than the old pane fills one row of the zoomed pane.
    let line = format!("TRACE {}", "x".repeat(usize::from(big.0) - 10));
    feed(&mut app, reopened, format!("{line}\r\n").as_bytes());
    let text = draw(&app)?.backend().to_string();
    let rows: Vec<&str> = text.lines().filter(|row| row.contains("TRACE")).collect();
    assert_eq!(rows.len(), 1, "{text}");
    assert!(rows[0].contains(&line), "the whole line on one row: {text}");
    Ok(())
}

/// The output stream's text as drawn.
fn shown(app: &App) -> Result<String, Box<dyn std::error::Error>> {
    Ok(draw(app)?.backend().to_string())
}

#[tokio::test]
async fn zoom_keeps_the_last_frame_until_the_reopened_stream_draws() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let generation = focus_terminal(&mut app, b"OLD FRAME\r\n$ ").ok_or("no interactive attach")?;
    app.update(key(KeyCode::Esc));
    let effects = app.update(key(KeyCode::Char('z')));
    let token = effects
        .iter()
        .find_map(|e| match e {
            Effect::SettleResize { token, .. } => Some(*token),
            _ => None,
        })
        .ok_or(format!("no settle timer: {effects:?}"))?;
    // The shell redraws into the locally resized grid meanwhile; the pane
    // still shows the frame from before the zoom.
    app.update(Msg::AttachOutput {
        generation,
        bytes: b"\x1b[H\x1b[2Jlocal redraw".to_vec(),
    });
    let text = shown(&app)?;
    assert!(text.contains("OLD FRAME"), "{text}");
    assert!(!text.contains("local redraw"), "{text}");
    // Settled: the stream reopens, and the pane is not blank while it opens.
    let effects = app.update(Msg::ResizeSettled { token });
    let (reopened, interactive) = attach_in(&effects).ok_or(format!("no reopen: {effects:?}"))?;
    assert!(interactive && reopened > generation, "{effects:?}");
    app.update(opened(reopened));
    let text = shown(&app)?;
    assert!(text.contains("OLD FRAME"), "{text}");
    // The daemon's redraw at the new size replaces it.
    app.update(Msg::AttachOutput {
        generation: reopened,
        bytes: b"\x1b[H\x1b[2JNEW FRAME".to_vec(),
    });
    let text = shown(&app)?;
    assert!(text.contains("NEW FRAME"), "{text}");
    assert!(!text.contains("OLD FRAME"), "{text}");
    Ok(())
}

#[tokio::test]
async fn zoom_from_watching_keeps_the_last_frame_until_the_stream_draws() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"WATCHED FRAME");
    // Watching: `z` reopens at once, without a settle timer.
    let effects = app.update(key(KeyCode::Char('z')));
    let (reopened, _) = attach_in(&effects).ok_or(format!("no reopen: {effects:?}"))?;
    app.update(opened(reopened));
    let text = shown(&app)?;
    assert!(text.contains("WATCHED FRAME"), "{text}");
    app.update(Msg::AttachOutput {
        generation: reopened,
        bytes: b"\x1b[H\x1b[2JZOOMED FRAME".to_vec(),
    });
    let text = shown(&app)?;
    assert!(text.contains("ZOOMED FRAME"), "{text}");
    assert!(!text.contains("WATCHED FRAME"), "{text}");
    Ok(())
}

#[tokio::test]
async fn units_the_daemon_marks_without_a_pty_are_never_resized() -> TestResult {
    let mut lanes = mixed_lanes();
    for unit in &mut lanes[0].units {
        unit.pty = false;
    }
    let api = FakeApi::with_lanes(lanes);
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let effects = app.update(key(KeyCode::Enter));
    let (generation, _) = attach_in(&effects).ok_or("no interactive attach")?;
    let opened = app.update(Msg::AttachOpened {
        generation,
        result: Ok(devlanes_proto::AttachResult {
            stream_id: 2,
            unit: String::new(),
            lane: String::new(),
        }),
    });
    assert!(resizes(&opened).is_empty(), "{opened:?}");
    app.update(key(KeyCode::Esc));
    let zoomed = app.update(key(KeyCode::Char('z')));
    assert!(resizes(&zoomed).is_empty(), "{zoomed:?}");
    Ok(())
}

#[tokio::test]
async fn other_refused_resizes_are_reported_but_keep_the_stream() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    app.update(Msg::ResizeRefused {
        generation,
        error: ApiError::Rpc(RpcError::new("bad_request", json!({"message": "size"}), 2)),
    });
    assert!(app.notice.as_ref().is_some_and(|n| n.error));
    let attached = app.attached.as_ref().ok_or("detached")?;
    assert_eq!(attached.state, devlanes_tui::app::AttachState::Live);
    Ok(())
}

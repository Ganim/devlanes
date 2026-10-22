//! Unit order and colors in the lane tree, and the profile picker.

use std::sync::Arc;

use crossterm::event::KeyCode;
use devlanes_proto::{Event, LaneSummary, Method, RpcError, events};
use devlanes_tui::{
    ApiError, DaemonApi,
    app::{App, Dialog, Effect, Msg, ProfilesState},
    theme::palette_color,
};
use ratatui::{
    buffer::Buffer,
    style::{Color, Style},
};
use serde_json::json;

use crate::daemon::{Step, run_keys};
use crate::errors::{assert_clean, key, render, row, screen};
use crate::fake::{FakeApi, lane, mixed_lanes, unit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// One lane whose units arrive in declaration order with colors.
fn colored_lane() -> LaneSummary {
    let colored = |name: &str, kind: &str, state: &str, color: Option<&str>| {
        let mut summary = unit("acme@0", name, state);
        summary.kind = kind.into();
        summary.color = color.map(str::to_owned);
        summary
    };
    let mut worker = colored("worker", "service", "failed", Some("bright-magenta"));
    worker.exit_code = Some(1);
    lane(
        "acme",
        Some(0),
        "main",
        "running",
        vec![
            colored("web", "service", "ready", Some("green")),
            colored("zz-shell", "exec", "ready", None),
            colored("migrate", "task", "exited", None),
            colored("Api", "service", "ready", Some("blue")),
            worker,
            colored("adhoc", "exec", "ready", Some("chartreuse")),
        ],
    )
}

/// The cell where `text` starts on row `y`.
fn find(buffer: &Buffer, y: u16, text: &str) -> Option<u16> {
    let line = row(buffer, y);
    let byte = line.find(text)?;
    // Box-drawing and glyph cells are one column each; count characters.
    u16::try_from(line[..byte].chars().count()).ok()
}

/// The row whose text contains `needle`.
fn row_of(buffer: &Buffer, needle: &str) -> Option<u16> {
    (0..buffer.area.height).find(|y| row(buffer, *y).contains(needle))
}

#[tokio::test]
async fn display_names_and_running_replace_names_and_ready() -> TestResult {
    let mut gateway = unit("acme@0", "api-gateway", "ready");
    gateway.display_name = Some("Gateway API".into());
    let mut worker = unit("acme@0", "wrk-email", "ready");
    // No readiness check: a live process is all `ready` means.
    worker.ready_check = Some("started".into());
    let api = FakeApi::with_lanes(vec![lane(
        "acme",
        Some(0),
        "main",
        "running",
        vec![gateway, worker],
    )]);
    let mut app = App::new(100, 24);
    app.update(Msg::Status(api.status().await));
    let buffer = render(&app)?;
    // Ready (a check passed) and running (no check) are told apart by the
    // glyph alone; neither prints its word in the list.
    let list = |y: u16| row(&buffer, y).chars().take(40).collect::<String>();
    let gateway = row_of(&buffer, "● Gateway API").ok_or("no Gateway API row")?;
    assert!(!list(gateway).contains("ready"), "{}", list(gateway));
    let worker = row_of(&buffer, "◉ wrk-email").ok_or("no wrk-email row")?;
    assert!(!list(worker).contains("running"), "{}", list(worker));
    // The pane title shows the label, the real name and the state word.
    let text = screen(&buffer);
    assert!(text.contains("Gateway API  api-gateway · ready"), "{text}");
    assert_eq!(
        app.selected_unit().map(|u| u.name.as_str()),
        Some("api-gateway")
    );
    Ok(())
}

#[tokio::test]
async fn units_keep_the_daemon_order_with_exec_units_last() -> TestResult {
    let mut app = App::new(80, 24);
    app.update(Msg::Status(Ok(vec![colored_lane()])));
    let names: Vec<&str> = app.lanes[0].units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        ["web", "migrate", "Api", "worker", "zz-shell", "adhoc"],
        "devlanes.yaml order as the daemon sends it, exec units after"
    );
    // A unit that appears later goes after the others of its kind.
    app.update(Msg::Event(Event {
        seq: 2,
        event: events::UNIT_CHANGED.into(),
        data: json!({"unit": unit("acme@0", "cache", "starting")}),
    }));
    let names: Vec<&str> = app.lanes[0].units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "web", "migrate", "Api", "worker", "cache", "zz-shell", "adhoc"
        ]
    );
    // With `order` (its place in devlanes.yaml) a late unit takes that place.
    let mut lane = colored_lane();
    for (index, unit) in lane.units.iter_mut().enumerate() {
        if unit.kind != "exec" {
            unit.order = Some(u32::try_from(index)? * 2);
        }
    }
    app.update(Msg::Status(Ok(vec![lane])));
    let mut db = unit("acme@0", "db", "starting");
    db.order = Some(1);
    app.update(Msg::Event(Event {
        seq: 3,
        event: events::UNIT_CHANGED.into(),
        data: json!({ "unit": db }),
    }));
    let names: Vec<&str> = app.lanes[0].units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        ["web", "db", "migrate", "Api", "worker", "zz-shell", "adhoc"]
    );
    Ok(())
}

#[test]
fn palette_names_map_to_terminal_colors() {
    assert_eq!(palette_color("blue"), Some(Color::Blue));
    assert_eq!(palette_color("white"), Some(Color::Gray));
    assert_eq!(palette_color("gray"), Some(Color::DarkGray));
    assert_eq!(palette_color("bright-cyan"), Some(Color::LightCyan));
    assert_eq!(palette_color("chartreuse"), None);
}

#[tokio::test]
async fn unit_colors_are_names_and_state_is_glyph_and_word() -> TestResult {
    let mut app = App::new(60, 24);
    app.update(Msg::Status(Ok(vec![colored_lane()])));
    let buffer = render(&app)?;
    assert_clean(&buffer, "colored units");

    // The unit's color is its name's; the glyph is the state's; no bar.
    let y = row_of(&buffer, "● web").ok_or("web row")?;
    let x = find(&buffer, y, "web").ok_or("web name")?;
    assert_eq!(buffer[(x, y)].fg, Color::Green, "name in the family color");
    assert_eq!(buffer[(x - 2, y)].fg, Color::Green, "ready glyph");
    assert!(
        !row(&buffer, y).contains('▍'),
        "no color bar: {}",
        row(&buffer, y)
    );

    // The selected row (the failed worker, the lane's first problem) is plain
    // reverse video in 16 colors: a colored glyph would turn into a block.
    let y = row_of(&buffer, "✕ worker").ok_or("worker row")?;
    let x = find(&buffer, y, "worker").ok_or("worker name")?;
    for column in [x - 2, x] {
        let cell = &buffer[(column, y)];
        assert_eq!(cell.fg, Color::Reset, "column {column}");
        assert!(cell.modifier.contains(ratatui::style::Modifier::REVERSED));
    }
    insta::assert_snapshot!("colored_units_60x24", styled(&buffer, 2..15));

    // Unselected, a failed unit keeps its own name color, a red glyph and
    // state word.
    app.update(key(KeyCode::Char('g')));
    let buffer = render(&app)?;
    // The lane overview in the pane lists the worker too; take the list's row.
    let (y, x) = (0..buffer.area.height)
        .find_map(|y| {
            find(&buffer, y, "✕ worker")
                .filter(|x| *x < 20)
                .map(|x| (y, x + 2))
        })
        .ok_or("worker row")?;
    assert_eq!(buffer[(x, y)].fg, Color::LightMagenta, "name");
    assert_eq!(buffer[(x - 2, y)].fg, Color::Red, "failed glyph");
    let state = find(&buffer, y, "failed").ok_or("state word")?;
    assert_eq!(buffer[(state, y)].fg, Color::Red, "failed state word");

    // Tasks keep the default color; so does a color this client does not know.
    for name in ["migrate", "adhoc"] {
        let y = row_of(&buffer, name).ok_or("row")?;
        let x = find(&buffer, y, name).ok_or("name")?;
        assert_eq!(buffer[(x, y)].fg, Color::Reset, "{name}");
    }
    Ok(())
}

/// Rows with a compact style legend: each run of cells sharing a style is
/// listed as `x..x fg/bg`, so color choices show up in the snapshot.
fn styled(buffer: &Buffer, rows: std::ops::Range<u16>) -> String {
    let name = |color: Color| match color {
        Color::Reset => "-".to_owned(),
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        other => format!("{other:?}"),
    };
    let mut out = Vec::new();
    for y in rows {
        out.push(format!("\"{}\"", row(buffer, y)));
        let mut runs = Vec::new();
        let mut start = 0;
        let style_at = |x: u16| {
            let cell = &buffer[(x, y)];
            Style::new().fg(cell.fg).bg(cell.bg)
        };
        for x in 1..=buffer.area.width {
            if x == buffer.area.width || style_at(x) != style_at(start) {
                let style = style_at(start);
                let (fg, bg) = (
                    style.fg.unwrap_or(Color::Reset),
                    style.bg.unwrap_or(Color::Reset),
                );
                if fg != Color::Reset || bg != Color::Reset {
                    runs.push(format!("{start}..{} {}/{}", x - 1, name(fg), name(bg)));
                }
                start = x;
            }
        }
        out.push(format!("    {}", runs.join(", ")));
    }
    out.join("\n")
}

// ---- profiles ----------------------------------------------------------------------

/// The fixture lanes with `acme@0` running profile `web`.
fn profile_lanes() -> Vec<LaneSummary> {
    let mut lanes = mixed_lanes();
    lanes[0].profile = Some("web".into());
    lanes
}

/// An app with `acme@0` selected, running profile `web`.
async fn profiles_app(api: &FakeApi) -> App {
    *api.lanes
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = profile_lanes();
    let mut app = App::new(80, 24);
    app.update(Msg::Status(api.status().await));
    app
}

#[tokio::test]
async fn profile_picker_preselects_toggles_and_starts_checked_profiles() -> TestResult {
    let api = FakeApi::default();
    let mut app = profiles_app(&api).await;
    let effects = app.update(key(KeyCode::Char('p')));
    assert!(matches!(effects.as_slice(), [Effect::FetchProfiles { lane }] if lane == "acme@0"));
    assert!(matches!(
        &app.dialog,
        Some(Dialog::Profiles {
            state: ProfilesState::Loading,
            ..
        })
    ));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: api.profile_list("acme@0".into()).await,
    });
    let Some(Dialog::Profiles {
        state: ProfilesState::Ready { items, .. },
        ..
    }) = &app.dialog
    else {
        return Err(format!("picker not ready: {:?}", app.dialog).into());
    };
    let checked: Vec<bool> = items.iter().map(|item| item.checked).collect();
    assert_eq!(
        checked,
        [true, false, false],
        "the lane's current profile is checked"
    );
    // Move to e2e and check it; the picker shows it before starting.
    app.update(key(KeyCode::Char('j')));
    app.update(key(KeyCode::Char(' ')));
    let buffer = render(&app)?;
    assert_clean(&buffer, "profile picker");
    let text = screen(&buffer);
    let line = |needle: &str| {
        text.lines()
            .find(|line| line.contains(needle))
            .unwrap_or_default()
            .to_owned()
    };
    let web = line("[x] web");
    assert!(
        web.contains("default") && web.contains("web, api"),
        "{text}"
    );
    let e2e = line("▌[x] e2e");
    assert!(e2e.contains("+e2e, web, api, worker"), "{text}");
    let frontend = line("[ ] frontend");
    assert!(frontend.contains("no infra"), "{text}");
    insta::assert_snapshot!("profiles_80x24", text);
    let effects = app.update(key(KeyCode::Enter));
    let [
        Effect::Control {
            method,
            lane,
            selection,
            label,
            ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!((*method, lane.as_str()), (Method::Start, "acme@0"));
    assert_eq!(selection.profiles, ["web", "e2e"], "list order");
    assert!(
        selection.profile.is_none(),
        "the old single field stays unset"
    );
    assert_eq!(label, "start acme@0 with web, e2e");
    assert!(app.dialog.is_none());
    Ok(())
}

#[tokio::test]
async fn every_profile_of_the_last_start_is_preselected() -> TestResult {
    let mut lanes = mixed_lanes();
    lanes[0].profiles = vec!["e2e".into(), "frontend".into()];
    lanes[0].profile = Some("e2e".into());
    let api = FakeApi::with_lanes(lanes);
    let mut app = App::new(80, 24);
    app.update(Msg::Status(api.status().await));
    app.update(key(KeyCode::Char('g')));
    assert!(screen(&render(&app)?).contains("profile   e2e, frontend"));
    app.update(key(KeyCode::Char('p')));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: api.profile_list("acme@0".into()).await,
    });
    let Some(Dialog::Profiles {
        state: ProfilesState::Ready { items, .. },
        ..
    }) = &app.dialog
    else {
        return Err(format!("picker not ready: {:?}", app.dialog).into());
    };
    let checked: Vec<bool> = items.iter().map(|item| item.checked).collect();
    assert_eq!(checked, [false, true, true]);
    Ok(())
}

#[tokio::test]
async fn unchecking_everything_starts_the_default_profile() -> TestResult {
    let api = FakeApi::default();
    let mut app = profiles_app(&api).await;
    app.update(key(KeyCode::Char('p')));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: api.profile_list("acme@0".into()).await,
    });
    app.update(key(KeyCode::Char(' ')));
    let effects = app.update(key(KeyCode::Enter));
    let [
        Effect::Control {
            selection, label, ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    // Named explicitly: an empty list would make the daemon reuse the last profiles.
    assert_eq!(selection.profiles, ["web"], "the is_default profile");
    assert_eq!(label, "start acme@0 with the default profile");
    // Esc cancels without starting anything.
    app.update(key(KeyCode::Char('p')));
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.dialog.is_none());
    Ok(())
}

#[tokio::test]
async fn without_a_default_profile_nothing_checked_means_no_profile() -> TestResult {
    let api = FakeApi::default();
    let mut app = profiles_app(&api).await;
    let mut items = crate::fake::profiles();
    for item in &mut items {
        item.is_default = false;
    }
    app.update(key(KeyCode::Char('p')));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: Ok(devlanes_tui::api::ProfileList {
            lane: "acme@0".into(),
            items,
        }),
    });
    assert!(screen(&render(&app)?).contains("With none checked, enter starts the lane without"));
    // The lane last ran with web; uncheck it.
    app.update(key(KeyCode::Char(' ')));
    let effects = app.update(key(KeyCode::Enter));
    let [
        Effect::Control {
            selection, label, ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    // `default` is accepted even when undefined and then means no profile.
    assert_eq!(selection.profiles, ["default"]);
    assert_eq!(label, "start acme@0 without a profile");
    Ok(())
}

#[tokio::test]
async fn a_config_without_profiles_starts_nothing() -> TestResult {
    let api = FakeApi::default();
    let mut app = profiles_app(&api).await;
    app.update(key(KeyCode::Char('p')));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: Ok(devlanes_tui::api::ProfileList {
            lane: "acme@0".into(),
            items: Vec::new(),
        }),
    });
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    Ok(())
}

#[tokio::test]
async fn an_older_daemon_gets_a_plain_message() -> TestResult {
    let mut api = FakeApi::default();
    api.profiles_error = Some(RpcError::new(
        "unsupported_method",
        json!({"method": "profile.list"}),
        1,
    ));
    let mut app = profiles_app(&api).await;
    app.update(key(KeyCode::Char('p')));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: api.profile_list("acme@0".into()).await,
    });
    assert!(matches!(
        &app.dialog,
        Some(Dialog::Profiles {
            state: ProfilesState::Unsupported,
            ..
        })
    ));
    let text = screen(&render(&app)?);
    assert!(
        text.contains("This daemon is too old for profiles."),
        "{text}"
    );
    insta::assert_snapshot!("profiles_unsupported_80x24", text);
    assert!(app.errors.is_empty(), "an older daemon is not an error");
    // Space and enter do nothing there; esc closes.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert!(app.update(key(KeyCode::Char(' '))).is_empty());
    app.update(key(KeyCode::Esc));
    assert!(app.dialog.is_none());
    Ok(())
}

#[tokio::test]
async fn other_profile_list_failures_use_the_error_path() -> TestResult {
    let mut api = FakeApi::default();
    api.profiles_error = Some(RpcError::new(
        "config_error",
        json!({"message": "profiles.e2e: unknown unit worker"}),
        1,
    ));
    let mut app = profiles_app(&api).await;
    app.update(key(KeyCode::Char('p')));
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: api.profile_list("acme@0".into()).await,
    });
    assert!(app.dialog.is_none());
    let entry = app.errors.front().ok_or("no error")?;
    assert_eq!(entry.what, "load profiles of acme@0");
    assert_eq!(
        entry.worded.summary,
        "devlanes.yaml is invalid: profiles.e2e: unknown unit worker"
    );
    // A late answer after the picker was closed is dropped.
    app.update(Msg::Profiles {
        lane: "acme@0".into(),
        result: Err(ApiError::Disconnected("late".into())),
    });
    Ok(())
}

#[tokio::test]
async fn drive_starts_with_the_profiles_checked_in_the_picker() -> TestResult {
    // Lanes are set directly: the helper waits for the loop's own first status call.
    let api = Arc::new(FakeApi::with_lanes(profile_lanes()));
    // The picker's keys wait for its list; the start's notice is waited for
    // before quitting.
    let steps = vec![
        Step::Key(key(KeyCode::Char('p'))),
        Step::Screen("frontend"),
        Step::Key(key(KeyCode::Char('j'))),
        Step::Key(key(KeyCode::Char('j'))),
        Step::Key(key(KeyCode::Char(' '))),
        Step::Key(key(KeyCode::Enter)),
    ];
    let terminal = run_keys(Arc::clone(&api), steps, &["started acme@0 with"], |_| true).await?;
    let calls = api.calls();
    assert!(
        calls.contains(&"profile.list acme@0".to_owned()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"Start acme@0 [] profiles=[\"web\", \"frontend\"]".to_owned()),
        "{calls:?}"
    );
    let header = row(terminal.buffer(), 0);
    assert!(header.contains("started acme@0 with"), "{header}");
    Ok(())
}

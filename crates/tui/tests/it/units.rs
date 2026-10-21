//! Unit order and colors in the lane tree, and the profile picker.

use std::sync::Arc;

use crossterm::event::KeyCode;
use devlanes_proto::{Event, LaneSummary, Method, RpcError, events};
use devlanes_tui::{
    ApiError, DaemonApi,
    app::{App, Dialog, Effect, Msg, ProfilesState},
    ui::palette_color,
};
use ratatui::{
    buffer::Buffer,
    style::{Color, Style},
};
use serde_json::json;

use crate::daemon::run_keys;
use crate::errors::{assert_clean, key, render, row, screen};
use crate::fake::{FakeApi, lane, mixed_lanes, unit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Selection highlight (brand paint) and failure red, as `ui` draws them.
const PAINT: Color = Color::Rgb(0xf5, 0xc5, 0x42);
const ON_PAINT: Color = Color::Rgb(0x1a, 0x19, 0x17);
const FAILED: Color = Color::Rgb(0xd9, 0x4f, 0x3d);

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
async fn units_sort_by_name_with_exec_units_last() -> TestResult {
    let mut app = App::new(80, 24);
    app.update(Msg::Status(Ok(vec![colored_lane()])));
    let names: Vec<&str> = app.lanes[0].units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        ["Api", "migrate", "web", "worker", "adhoc", "zz-shell"],
        "services and tasks by name, exec units after"
    );
    // A unit that appears later lands in its sorted place.
    app.update(Msg::Event(Event {
        seq: 2,
        event: events::UNIT_CHANGED.into(),
        data: json!({"unit": unit("acme@0", "cache", "starting")}),
    }));
    let names: Vec<&str> = app.lanes[0].units.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "Api", "cache", "migrate", "web", "worker", "adhoc", "zz-shell"
        ]
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
async fn unit_colors_keep_failures_red_and_the_highlight_readable() -> TestResult {
    let mut app = App::new(60, 12);
    app.update(Msg::Status(Ok(vec![colored_lane()])));
    let buffer = render(&app)?;
    assert_clean(&buffer, "colored units");

    // Name and marker take the unit color.
    let y = row_of(&buffer, "web  ready").ok_or("web row")?;
    let x = find(&buffer, y, "web").ok_or("web name")?;
    assert_eq!(buffer[(x, y)].fg, Color::Green);
    assert_eq!(buffer[(x - 2, y)].fg, Color::Green, "marker");

    // A failed unit keeps its color on the name but a red marker and state word.
    let y = row_of(&buffer, "worker").ok_or("worker row")?;
    let x = find(&buffer, y, "worker").ok_or("worker name")?;
    assert_eq!(buffer[(x, y)].fg, Color::LightMagenta);
    assert_eq!(buffer[(x - 2, y)].fg, FAILED, "failed marker");
    let state = find(&buffer, y, "failed 1").ok_or("state word")?;
    assert_eq!(buffer[(state, y)].fg, FAILED, "failed state word");

    // No color, or one this client does not know, means the default style.
    for name in ["migrate", "adhoc"] {
        let y = row_of(&buffer, name).ok_or("row")?;
        let x = find(&buffer, y, name).ok_or("name")?;
        assert_eq!(buffer[(x, y)].fg, Color::Reset, "{name}");
    }

    // The selected row drops unit colors for one high-contrast style.
    app.update(key(KeyCode::Char('j')));
    let buffer = render(&app)?;
    let y = row_of(&buffer, "Api  ready").ok_or("Api row")?;
    let x = find(&buffer, y, "Api").ok_or("Api name")?;
    for column in (x - 2)..(x + 10) {
        let cell = &buffer[(column, y)];
        assert_eq!((cell.fg, cell.bg), (ON_PAINT, PAINT), "column {column}");
    }
    insta::assert_snapshot!("colored_units_60x12", styled(&buffer, 1..10));
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
    assert!(text.contains("[x] web  default  web, api"), "{text}");
    assert!(text.contains("> [x] e2e  +e2e, web, api, worker"), "{text}");
    assert!(text.contains("[ ] frontend  no infra  web"), "{text}");
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
    assert!(screen(&render(&app)?).contains("profiles  e2e, frontend"));
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
    assert!(selection.profiles.is_empty());
    assert_eq!(label, "start acme@0 with the default profile");
    // Esc cancels without starting anything.
    app.update(key(KeyCode::Char('p')));
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert!(app.dialog.is_none());
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
    assert!(text.contains("profiles need a newer daemon"), "{text}");
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
    assert_eq!(entry.what, "profiles of acme@0");
    assert_eq!(
        entry.worded.summary,
        "invalid config: profiles.e2e: unknown unit worker"
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
    let keys = vec![
        key(KeyCode::Char('p')),
        key(KeyCode::Char('j')),
        key(KeyCode::Char('j')),
        key(KeyCode::Char(' ')),
        key(KeyCode::Enter),
    ];
    let terminal = run_keys(Arc::clone(&api), keys).await?;
    let calls = api.calls();
    assert!(
        calls.contains(&"profile.list acme@0".to_owned()),
        "{calls:?}"
    );
    assert!(
        calls.contains(&"Start acme@0 [] profiles=[\"web\", \"frontend\"]".to_owned()),
        "{calls:?}"
    );
    let header = row(terminal.backend().buffer(), 0);
    assert!(
        header.contains("start acme@0 with web, frontend: ok"),
        "{header}"
    );
    Ok(())
}

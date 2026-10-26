//! TUI tests against a fake daemon: rendering snapshots, key handling and resync.

/// `insta::assert_snapshot!` with this build's version written as `X.Y.Z`:
/// the fake daemon reports it, and a release bump must not change a
/// snapshot. Versions a test sets on purpose must differ from it (`0.0.1`).
macro_rules! assert_screen {
    ($($arg:tt)*) => {{
        let version = format!(r"\b{}\b", devlanes_version::VERSION.replace('.', r"\."));
        let mut settings = insta::Settings::clone_current();
        settings.add_filter(&version, "X.Y.Z");
        let _bound = settings.bind_to_scope();
        insta::assert_snapshot!($($arg)*);
    }};
}

mod containers;
mod daemon;
mod errors;
mod fake;
mod interaction;
mod monitor;
mod navigation;
mod orphans;
mod progress;
mod release;
mod screens;
mod start;
mod terminal;
mod units;

use std::{sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::{Method, events};
use devlanes_tui::{
    DaemonApi, Options, UnitMetrics,
    app::{App, Connection, Dialog, Effect, Focus, Msg, Selected, Target, View},
    drive, ui,
};
use fake::{FakeApi, mixed_lanes};
use futures_util::stream;
use ratatui::{Terminal, backend::TestBackend};
use serde_json::json;
use tokio::sync::mpsc;

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Options for driving the loop in tests: an empty environment (no
/// `NO_COLOR`, locale or `TMUX` from the machine, no `HOME`), UTC and a
/// directory no lane uses, so what a test draws is the same everywhere.
/// The fake daemon answers everything else, the install method included.
fn hermetic_options() -> Options {
    Options {
        env: Some(std::collections::BTreeMap::new()),
        time_zone: Some(jiff::tz::TimeZone::UTC),
        cwd: Some("/nonexistent/devlanes-tests".into()),
        ..Options::default()
    }
}

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

/// Shift+Tab, the previous project; crossterm reports it as `BackTab`.
fn shift_tab() -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT))
}

/// Shift+`c`.
fn shifted(c: char) -> Msg {
    Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT))
}

/// Selects `target` as a click or `n` would; returns the effects.
fn select(app: &mut App, target: Selected) -> Vec<Effect> {
    let mut effects = Vec::new();
    app.select(target, &mut effects);
    effects
}

/// Selects unit `name`, in the shown lane when it has one; returns the
/// generation of the stream attached to it.
fn select_unit(app: &mut App, name: &str) -> Option<u64> {
    let has = |lane: &devlanes_proto::LaneSummary| lane.units.iter().any(|u| u.name == name);
    let lane = app
        .selected_lane()
        .filter(|lane| has(lane))
        .or_else(|| app.lanes.iter().find(|lane| has(lane)))?
        .reference
        .clone();
    let effects = select(app, Selected::unit(&lane, name));
    attach_in(&effects)
        .map(|(generation, _)| generation)
        .or_else(|| {
            app.attached
                .as_ref()
                .filter(|a| a.unit == name)
                .map(|a| a.generation)
        })
}

/// The generation and role of the attach in `effects`, if any.
fn attach_in(effects: &[Effect]) -> Option<(u64, bool)> {
    effects.iter().find_map(|effect| match effect {
        Effect::Attach {
            generation,
            interactive,
            ..
        } => Some((*generation, *interactive)),
        _ => None,
    })
}

/// Enter on a watched unit: the stream reopens as interactive, and typing
/// starts once that stream is open. Returns its generation.
fn focus_terminal(app: &mut App, bytes: &[u8]) -> Option<u64> {
    let effects = app.update(key(KeyCode::Enter));
    let (generation, interactive) = attach_in(&effects)?;
    assert!(interactive, "focus reopens the stream as interactive");
    assert_eq!(app.focus, Focus::List, "focus waits for the stream");
    feed(app, generation, bytes);
    assert_eq!(app.focus, Focus::Terminal);
    Some(generation)
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
    assert_screen!("mixed_lanes", draw(&app)?.backend());
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
    assert_screen!("failed_unit", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_idle_lane_details() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 20).await;
    select(&mut app, Selected::lane("acme@2"));
    assert_screen!("lane_details", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_empty_project() -> TestResult {
    let api = FakeApi::with_lanes(Vec::new());
    let (app, effects) = loaded(&api, 80, 12).await;
    assert!(effects.is_empty());
    assert_screen!("empty", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_tiny_terminal() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (app, _) = loaded(&api, 30, 6).await;
    assert_screen!("tiny", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_monitor_tab() -> TestResult {
    let mut api = FakeApi::with_lanes(mixed_lanes());
    api.metrics.units = vec![
        UnitMetrics {
            lane: "acme@0".into(),
            unit: "api".into(),
            cpu_percent: Some(42.0),
            memory_bytes: Some(3 * 1024 * 1024 * 1024),
            history: (0..200)
                .map(|i| (i * 1000, (i % 10) as f64 * 10.0, (1000 + i * 4) << 20))
                .collect(),
            ..UnitMetrics::default()
        },
        UnitMetrics {
            lane: "acme@0".into(),
            unit: "web".into(),
            cpu_percent: Some(3.0),
            memory_bytes: Some(900 * 1024 * 1024),
            ..UnitMetrics::default()
        },
    ];
    let (mut app, _) = loaded(&api, 100, 20).await;
    app.update(Msg::Subscribed(Ok(())));
    let effects = app.update(shifted('M'));
    assert_eq!(app.view, View::Monitor);
    // Before the first frame: two seconds of history per terminal column.
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMetrics(q) if q.range_ms == 200_000)),
        "{effects:?}"
    );
    let effects = app.update(Msg::Tick);
    assert!(effects.iter().any(|e| matches!(e, Effect::FetchMetrics(_))));
    app.update(Msg::Metrics(api.metrics(app.metrics_query()).await));
    app.update(key(KeyCode::Char('j')));
    let terminal = draw(&app)?;
    let text: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains("3.9G"), "3G + 900M summed: {text}");
    assert_screen!("monitor", terminal.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_monitor_without_metrics_uses_status_numbers() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 80, 12).await;
    app.update(shifted('M'));
    app.update(Msg::Metrics(api.metrics(app.metrics_query()).await));
    assert_screen!("monitor_fallback", draw(&app)?.backend());
    Ok(())
}

#[tokio::test]
async fn snapshot_run_confirm_and_reconnecting() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 80, 16).await;
    select(&mut app, Selected::lane("acme@1"));
    app.update(shifted('C'));
    app.update(Msg::EventsEnded(None));
    assert_screen!("confirm_reconnecting", draw(&app)?.backend());
    Ok(())
}

// ---- keys ----------------------------------------------------------------------

#[tokio::test]
async fn enter_types_into_a_running_unit_and_esc_returns() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    select(&mut app, Selected::lane("acme@0"));
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.focus, Focus::List, "the whole-lane row has no terminal");
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"$ ");
    focus_terminal(&mut app, b"$ ").ok_or("no interactive attach")?;
    // While typing, list keys go to the unit instead.
    let effects = app.update(key(KeyCode::Char('q')));
    assert!(matches!(effects.as_slice(), [Effect::Input(bytes)] if bytes == b"q"));
    // Esc is consumed: it ends typing and sends nothing to the unit.
    assert!(app.update(key(KeyCode::Esc)).is_empty());
    assert_eq!(app.focus, Focus::List);
    // The stream is interactive already, so typing again needs no reattach.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.focus, Focus::Terminal);
    // Ctrl+] always returns; crossterm may report it as Ctrl+5.
    app.update(ctrl(']'));
    assert_eq!(app.focus, Focus::List);
    app.update(key(KeyCode::Enter));
    app.update(ctrl('5'));
    assert_eq!(app.focus, Focus::List);
    Ok(())
}

#[tokio::test]
async fn esc_reaches_full_screen_programs() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    // vim switches to the alternate screen.
    focus_terminal(&mut app, b"\x1b[?1049h").ok_or("no interactive attach")?;
    let effects = app.update(key(KeyCode::Esc));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\x1b"));
    assert_eq!(app.focus, Focus::Terminal);
    let text = draw(&app)?.backend().to_string();
    assert!(text.contains("ctrl+] returns to the list"), "{text}");
    app.update(ctrl(']'));
    assert_eq!(app.focus, Focus::List);
    Ok(())
}

#[tokio::test]
async fn help_lists_every_key_with_this_screen_first() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 60).await;
    app.update(key(KeyCode::Char('?')));
    let text = draw(&app)?.backend().to_string();
    for expected in [
        "type into the unit",
        "esc  ctrl+]",
        "next, previous project",
        "previous, next lane",
        "next, previous problem",
        "filter by name or +tag",
        "reverse the sort",
        "clean every lane whose worktree is gone",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    let list = text.find("list").ok_or("no list section")?;
    let monitor = text.find("monitor (M)").ok_or("no monitor section")?;
    assert!(list < monitor, "the project view's keys come first");
    // On the monitor its own keys come first.
    app.update(key(KeyCode::Esc));
    app.update(shifted('M'));
    app.update(key(KeyCode::Char('?')));
    let text = draw(&app)?.backend().to_string();
    let monitor = text.find("monitor (M)").ok_or("no monitor section")?;
    let list = text.find("  j k  up down").ok_or("no list keys")?;
    assert!(monitor < list, "{text}");
    Ok(())
}

#[tokio::test]
async fn tab_switches_projects_and_reaches_the_unit_while_typing() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"$ ");
    // While watching, Tab and Shift+Tab switch projects, digits jump to one.
    app.update(key(KeyCode::Tab));
    assert_eq!(app.project(), Some("shop"));
    app.update(shift_tab());
    assert_eq!(app.project(), Some("acme"));
    assert_eq!(
        app.selected_unit().map(|u| u.name.as_str()),
        Some("api"),
        "a project comes back on the row selected there last"
    );
    // Shift+2 as a terminal without the kitty protocol sends it, then Shift+1
    // as the digit with SHIFT.
    app.update(key(KeyCode::Char('@')));
    assert_eq!(app.project(), Some("shop"));
    app.update(shifted('1'));
    assert_eq!(app.project(), Some("acme"));
    let generation = app
        .attached
        .as_ref()
        .map(|a| a.generation)
        .ok_or("detached")?;
    feed(&mut app, generation, b"$ ");
    app.update(ctrl('a'));
    assert_eq!(app.focus, Focus::List, "C-a does not switch focus");
    focus_terminal(&mut app, b"$ ").ok_or("no interactive attach")?;
    let effects = app.update(key(KeyCode::Tab));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\t"));
    let effects = app.update(shift_tab());
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\x1b[Z"));
    assert_eq!(
        app.project(),
        Some("acme"),
        "the project stays while typing"
    );
    let effects = app.update(ctrl('a'));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == &[1]));
    assert_eq!(app.focus, Focus::Terminal);
    Ok(())
}

#[tokio::test]
async fn a_cleaned_lane_leaves_the_selection_in_its_project() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    // Visit shop, then clean acme@2 from its lane.
    app.update(key(KeyCode::Char('@')));
    assert_eq!(app.project(), Some("shop"));
    app.update(key(KeyCode::Char('!')));
    app.update(key(KeyCode::Char('3')));
    assert_eq!(
        app.selected.as_ref().map(|s| s.lane.as_str()),
        Some("acme@2")
    );
    app.update(shifted('C'));
    let effects = app.update(key(KeyCode::Enter));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::CleanLane { lane } if lane == "acme@2")),
        "{effects:?}"
    );
    assert!(app.dialog.is_none());
    // The daemon reports the lane gone: the selection stays in acme, on
    // the lane numbered closest, although only shop runs anything now (the
    // first lane with something running is where a fresh TUI opens).
    let remaining: Vec<_> = mixed_lanes()
        .into_iter()
        .filter(|lane| lane.reference != "acme@2")
        .map(|mut lane| {
            if lane.project == "acme" {
                lane.state = "idle".into();
                for unit in &mut lane.units {
                    unit.state = "idle".into();
                }
            }
            lane
        })
        .collect();
    app.update(Msg::Status(Ok(remaining)));
    assert_eq!(app.project(), Some("acme"));
    assert_eq!(
        app.selected.as_ref().map(|s| s.lane.as_str()),
        Some("acme@1")
    );
    // Project keys still work, both ways, and acme opens on a lane that exists.
    app.update(key(KeyCode::Char('@')));
    assert_eq!(app.project(), Some("shop"));
    app.update(shifted('1'));
    assert_eq!(app.project(), Some("acme"));
    app.update(key(KeyCode::Tab));
    assert_eq!(app.project(), Some("shop"));
    Ok(())
}

#[tokio::test]
async fn a_project_whose_last_lane_went_away_still_opens() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    // acme was last shown on acme@2; then shop is on screen when acme@2
    // goes away (cleaned from the CLI, say).
    app.update(key(KeyCode::Char('3')));
    app.update(key(KeyCode::Char('@')));
    let remaining: Vec<_> = mixed_lanes()
        .into_iter()
        .filter(|lane| lane.reference != "acme@2")
        .collect();
    app.update(Msg::Status(Ok(remaining)));
    assert_eq!(app.project(), Some("shop"));
    // Shift+1 opens acme on a lane that exists instead of bouncing back.
    app.update(key(KeyCode::Char('!')));
    assert_eq!(app.project(), Some("acme"));
    assert_eq!(
        app.selected.as_ref().map(|s| s.lane.as_str()),
        Some("acme@0")
    );
    Ok(())
}

#[tokio::test]
async fn o_opens_the_public_url_first() -> TestResult {
    let mut lanes = mixed_lanes();
    let api_unit = lanes[0]
        .units
        .iter_mut()
        .find(|u| u.name == "api")
        .ok_or("no api")?;
    api_unit.urls = vec![
        "http://127.0.0.1:20001".into(),
        "https://api.acme.localhost:8443".into(),
    ];
    api_unit.public_url = Some("https://api.acme.localhost:8443".into());
    let api = FakeApi::with_lanes(lanes.clone());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let opened = |effects: Vec<Effect>| match effects.as_slice() {
        [Effect::OpenUrl(url)] => Some(url.clone()),
        _ => None,
    };
    select(&mut app, Selected::unit("acme@0", "api"));
    let url = opened(app.update(key(KeyCode::Char('o'))));
    assert_eq!(url.as_deref(), Some("https://api.acme.localhost:8443"));
    // An older daemon without `public_url`: the https entry of `urls`, else
    // the first one.
    lanes[0].units[0].public_url = None;
    app.update(Msg::Status(Ok(lanes.clone())));
    let url = opened(app.update(key(KeyCode::Char('o'))));
    assert_eq!(url.as_deref(), Some("https://api.acme.localhost:8443"));
    lanes[0].units[0].urls.truncate(1);
    app.update(Msg::Status(Ok(lanes)));
    let url = opened(app.update(key(KeyCode::Char('o'))));
    assert_eq!(url.as_deref(), Some("http://127.0.0.1:20001"));
    Ok(())
}

#[tokio::test]
async fn digits_pick_lanes_of_the_project_and_shifted_digits_projects() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    // A digit is the lane's place in the bar: 1 is the first lane (@0).
    app.update(key(KeyCode::Char('2')));
    let lane = |app: &App| app.selected.as_ref().map(|s| s.lane.clone());
    assert_eq!(lane(&app).as_deref(), Some("acme@1"));
    app.update(key(KeyCode::Char('3')));
    assert_eq!(lane(&app).as_deref(), Some("acme@2"));
    app.update(key(KeyCode::Char('1')));
    assert_eq!(lane(&app).as_deref(), Some("acme@0"));
    // A place past the last lane does nothing.
    app.update(key(KeyCode::Char('9')));
    assert_eq!(lane(&app).as_deref(), Some("acme@0"));
    // `(` is Shift+9 and there is no ninth project either.
    app.update(key(KeyCode::Char('(')));
    assert_eq!(app.project(), Some("acme"));
    app.update(key(KeyCode::Char('@')));
    assert_eq!(app.project(), Some("shop"));
    // The places follow the project.
    app.update(key(KeyCode::Char('1')));
    assert_eq!(lane(&app).as_deref(), Some("shop@0"));
    // A gap in the numbers leaves none in the places: with @1 gone, 2 is @2.
    app.update(key(KeyCode::Char('!')));
    let remaining: Vec<_> = mixed_lanes()
        .into_iter()
        .filter(|lane| lane.reference != "acme@1")
        .collect();
    app.update(Msg::Status(Ok(remaining)));
    app.update(key(KeyCode::Char('2')));
    assert_eq!(lane(&app).as_deref(), Some("acme@2"));
    Ok(())
}

#[tokio::test]
async fn monitor_and_daemon_open_from_anywhere_and_esc_returns() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    app.update(shifted('M'));
    assert_eq!(app.view, View::Monitor);
    let effects = app.update(shifted('D'));
    assert_eq!(app.view, View::Daemon);
    assert!(effects.iter().any(|e| matches!(e, Effect::FetchDaemon)));
    app.update(shifted('D'));
    assert_eq!(app.view, View::Project, "D again returns");
    app.update(shifted('M'));
    app.update(key(KeyCode::Esc));
    assert_eq!(app.view, View::Project);
    app.update(shifted('D'));
    app.update(shifted('2'));
    assert_eq!(
        (app.view, app.project()),
        (View::Project, Some("shop")),
        "a project number returns too"
    );
    Ok(())
}

#[tokio::test]
async fn start_stop_restart_dispatch_lane_and_unit_refs() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    select(&mut app, Selected::lane("acme@0"));
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
    // `]` shows the next lane, on its whole-lane row since it is stopped.
    app.update(key(KeyCode::Char(']')));
    assert_eq!(app.selected, Some(Selected::lane("acme@1")));
    let effects = app.update(key(KeyCode::Char('s')));
    let [
        Effect::Control {
            method: Method::Start,
            lane,
            selection,
            ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(lane, "acme@1");
    // Empty: the daemon brings back what the lane ran, as a plain `devlanes start`.
    assert!(selection.profiles.is_empty());
    // `start.restore: false` names the default profile instead.
    app.start_default = true;
    let effects = app.update(key(KeyCode::Char('s')));
    let [Effect::Control { selection, .. }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(selection.profiles, [devlanes_proto::DEFAULT_PROFILE]);
    Ok(())
}

#[tokio::test]
async fn arrows_follow_application_cursor_mode() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let generation = focus_terminal(&mut app, b"").ok_or("no interactive attach")?;
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
    focus_terminal(&mut app, b"\x1b[?2004h").ok_or("no interactive attach")?;
    let effects = app.update(Msg::Paste("ls\n".into()));
    assert!(matches!(effects.as_slice(), [Effect::Input(b)] if b == b"\x1b[200~ls\r\x1b[201~"));
    Ok(())
}

#[tokio::test]
async fn tasks_run_with_s_and_ask_only_when_their_config_does() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    app.update(Msg::Tasks {
        lane: "acme@0".into(),
        result: Ok(vec![
            devlanes_tui::api::TaskInfo {
                name: "migrate".into(),
                ..Default::default()
            },
            devlanes_tui::api::TaskInfo {
                name: "reset-db".into(),
                confirm: true,
                ..Default::default()
            },
        ]),
    });
    // A task that never ran is listed from task.list, and asks first.
    select(
        &mut app,
        Selected {
            lane: "acme@0".into(),
            row: devlanes_tui::app::RowId::Task("reset-db".into()),
        },
    );
    assert!(app.update(key(KeyCode::Char('s'))).is_empty());
    assert!(
        matches!(&app.dialog, Some(Dialog::Confirm { prompt, .. }) if prompt == "Run reset-db in acme@0?")
    );
    assert!(app.update(key(KeyCode::Char('n'))).is_empty());
    assert!(app.dialog.is_none());
    app.update(key(KeyCode::Char('s')));
    let effects = app.update(key(KeyCode::Enter));
    assert!(
        matches!(effects.as_slice(), [Effect::Run { params, yes: true, .. }] if params.task == "reset-db"),
        "the confirmed run answers the config's confirm: {effects:?}"
    );
    // One without `confirm` runs at once; r on a task runs it again.
    select_unit(&mut app, "migrate");
    let effects = app.update(key(KeyCode::Char('r')));
    let [Effect::Run { lane, params, yes }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!((lane.as_str(), params.task.as_str()), ("acme@0", "migrate"));
    assert!(params.terminal && params.detach && !yes);

    // An interactive task gets the terminal once it starts.
    let effects = app.update(Msg::RunDone {
        lane: "acme@0".into(),
        task: "migrate".into(),
        result: Ok(json!({"interactive": true})),
    });
    let interactive = effects.iter().rev().find_map(|e| match e {
        Effect::Attach {
            generation,
            unit,
            interactive: true,
            ..
        } if unit == "acme@0:migrate" => Some(*generation),
        _ => None,
    });
    let generation = interactive.ok_or_else(|| format!("no interactive attach: {effects:?}"))?;
    assert_eq!(app.focus, Focus::List, "focus waits for the stream");
    feed(&mut app, generation, b"");
    assert_eq!(app.focus, Focus::Terminal);
    Ok(())
}

#[tokio::test]
async fn zoom_resizes_the_pty() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let mut browsing = Vec::new();
    for _ in 0..20 {
        if app.selected_unit().is_some_and(|u| u.name == "api") {
            break;
        }
        // The lane opens on its failed worker; api is above it.
        browsing.extend(app.update(key(KeyCode::Char('k'))));
    }
    // Moving through the list only watches: no stream takes the PTY size.
    let (generation, interactive) = attach_in(&browsing).ok_or("no attach")?;
    assert!(!interactive, "browsing attaches as an observer");
    assert!(
        !browsing.iter().any(|e| matches!(
            e,
            Effect::Attach {
                interactive: true,
                ..
            }
        )),
        "{browsing:?}"
    );
    let opened = |app: &mut App, generation| {
        app.update(Msg::AttachOpened {
            generation,
            result: Ok(devlanes_proto::AttachResult {
                stream_id: 1,
                unit: String::new(),
                lane: String::new(),
            }),
        })
    };
    assert!(
        opened(&mut app, generation).is_empty(),
        "observers never resize"
    );
    let before = app.pane_size();
    // Zooming means looking at this terminal: it reopens as interactive at the new size.
    let effects = app.update(key(KeyCode::Char('z')));
    let [
        Effect::Detach,
        Effect::Attach {
            generation,
            columns,
            rows,
            interactive: true,
            ..
        },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    // 100x24 zoomed: the pane box takes the body, less its borders and the
    // column of margin inside it.
    assert_eq!((*columns, *rows), (97, 19));
    assert!(*columns > before.0);
    // Another viewer may have resized it meanwhile, so the size is sent again once open.
    let effects = opened(&mut app, *generation);
    assert!(
        matches!(
            effects.as_slice(),
            [Effect::ResizePty {
                columns: 97,
                rows: 19
            }]
        ),
        "{effects:?}"
    );
    // Unzooming resizes the interactive stream, then reopens it once resizing settles.
    let effects = app.update(key(KeyCode::Char('z')));
    let [
        Effect::ResizePty { columns, .. },
        Effect::SettleResize { token, .. },
    ] = effects.as_slice()
    else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(*columns, before.0);
    let stale = app.update(Msg::ResizeSettled { token: token - 1 });
    assert!(stale.is_empty(), "an older resize timer does nothing");
    let effects = app.update(Msg::ResizeSettled { token: *token });
    assert!(
        matches!(
            attach_in(&effects),
            Some((g, true)) if g > *generation
        ),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn the_pty_never_shrinks_below_the_minimum() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    focus_terminal(&mut app, b"").ok_or("no interactive attach")?;
    // The smallest window the layout still draws gives a tiny pane.
    let effects = app.update(Msg::Resize(
        devlanes_tui::app::MIN_WIDTH,
        devlanes_tui::app::MIN_HEIGHT,
    ));
    for effect in &effects {
        if let Effect::ResizePty { columns, rows } = effect {
            assert!(
                *columns >= devlanes_tui::app::MIN_PTY.0 && *rows >= devlanes_tui::app::MIN_PTY.1,
                "{effects:?}"
            );
        }
    }
    let (columns, rows) = app.attach_size();
    assert!(columns >= 20 && rows >= 5);
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
/// Scrolling past the pane's own scrollback shows the stored logs from about
/// where the screen was scrolled to, wrapped and in their colors, and `c`
/// does not jump back to the live screen.
async fn stored_logs_keep_their_colors_and_the_scroll_position() -> TestResult {
    use devlanes_proto::{LogLine, Page};
    use devlanes_tui::app::PaneMode;
    use ratatui::style::Color;
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"newest\r\n");
    let (width, height) = app.pane_size();
    let page = usize::from(height / 2);
    let effects = app.update(key(KeyCode::PageUp));
    let query = effects
        .iter()
        .find_map(|e| match e {
            Effect::FetchLogs { query, .. } => Some(query),
            _ => None,
        })
        .ok_or(format!("no fetch: {effects:?}"))?;
    assert!(query.styled, "colors are asked for");
    let text = draw(&app)?.backend().to_string();
    assert!(
        text.contains("newest"),
        "the live screen stays while loading: {text}"
    );
    let lines: Vec<LogLine> = (0..100)
        .map(|seq| {
            let text = if seq == 80 {
                format!("long {}", "x".repeat(usize::from(width) + 10))
            } else {
                format!("line {seq:03}")
            };
            LogLine {
                unit: "api".into(),
                seq,
                timestamp_ms: 0,
                styled: Some(format!("\x1b[32m{text}\x1b[0m")),
                text,
                level: None,
            }
        })
        .collect();
    app.update(Msg::Logs {
        generation,
        older: false,
        result: Ok(Page::complete(lines)),
    });
    let attached = app.attached.as_ref().ok_or("nothing attached")?;
    let PaneMode::Logs(view) = &attached.mode else {
        return Err(format!("not on stored logs: {:?}", attached.mode).into());
    };
    assert_eq!(view.offset, page);
    let terminal = draw(&app)?;
    let buffer = terminal.backend().buffer();
    let text = terminal.backend().to_string();
    assert!(
        !text.contains('…'),
        "lines wrap instead of being cut: {text}"
    );
    let xs = text.chars().filter(|&c| c == 'x').count();
    assert!(xs >= usize::from(width) + 10, "{text}");
    // The newest line shown sits `page` lines above the newest stored one.
    let newest = format!("line {:03}", 99 - page);
    let area = buffer.area;
    let place = (area.top()..area.bottom())
        .flat_map(|y| (area.left()..area.right()).map(move |x| (x, y)))
        .find(|&(x, y)| {
            newest.chars().enumerate().all(|(i, c)| {
                buffer
                    .cell((x + i as u16, y))
                    .is_some_and(|cell| cell.symbol() == c.to_string())
            })
        })
        .ok_or(format!("{newest} not shown: {text}"))?;
    assert_eq!(
        buffer[place].fg,
        Color::Indexed(2),
        "stored colors are drawn"
    );
    let below = format!("line {:03}", 100 - page);
    assert!(!text.contains(&below), "{text}");
    app.update(key(KeyCode::Char('c')));
    let attached = app.attached.as_ref().ok_or("nothing attached")?;
    assert!(
        matches!(attached.mode, PaneMode::Logs(_)),
        "{:?}",
        attached.mode
    );
    Ok(())
}

#[tokio::test]
async fn quit_asks_while_lanes_of_the_project_run() -> TestResult {
    use devlanes_tui::app::{Dialog, OnQuit};
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    // acme@0 runs: q and Ctrl+C ask instead of quitting.
    for press in [key(KeyCode::Char('q')), ctrl('c')] {
        assert!(app.update(press).is_empty());
        let Some(Dialog::Quit(dialog)) = &app.dialog else {
            return Err(format!("no quit dialog: {:?}", app.dialog).into());
        };
        assert_eq!(dialog.lanes, ["acme@0"]);
        // Esc stays.
        app.update(key(KeyCode::Esc));
        assert!(app.dialog.is_none());
    }
    let text = {
        app.update(key(KeyCode::Char('q')));
        draw(&app)?.backend().to_string()
    };
    for line in [
        "acme@0 is running.",
        "Close the TUI; lanes keep running",
        "Close and stop the running lanes",
        "[ ] remember this choice",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    // q again closes and keeps the lanes, remembering nothing.
    assert!(matches!(
        app.update(key(KeyCode::Char('q'))).as_slice(),
        [Effect::Quit]
    ));
    // Ctrl+C in the dialog closes at once.
    app.update(ctrl('c'));
    assert!(matches!(app.update(ctrl('c')).as_slice(), [Effect::Quit]));

    // Stop, remembered: the lanes stop and the choice is written first.
    app.update(key(KeyCode::Char('q')));
    app.update(key(KeyCode::Char('j')));
    app.update(key(KeyCode::Char(' ')));
    let effects = app.update(key(KeyCode::Enter));
    let [Effect::QuitWith { stop, remember }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(stop, &["acme@0"]);
    assert_eq!(*remember, Some(OnQuit::Stop));
    assert_eq!(app.on_quit, OnQuit::Stop);
    // A failed stop keeps the TUI open to say so; q then quits.
    app.update(Msg::QuitReady {
        error: Some("stop acme@0 failed: daemon gone".into()),
    });
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.error && n.text.contains("stop acme@0 failed"))
    );
    assert!(matches!(
        app.update(key(KeyCode::Char('q'))).as_slice(),
        [Effect::Quit]
    ));
    // Done: quit.
    assert!(matches!(
        app.update(Msg::QuitReady { error: None }).as_slice(),
        [Effect::Quit]
    ));

    // `tui.on_quit: stop` stops without asking; `keep` just closes.
    app.on_quit = OnQuit::Stop;
    let effects = app.update(key(KeyCode::Char('q')));
    assert!(
        matches!(effects.as_slice(), [Effect::QuitWith { stop, remember: None }] if stop == &["acme@0"]),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn quit_closes_at_once_when_nothing_runs() -> TestResult {
    let idle = vec![fake::lane(
        "acme",
        Some(0),
        "main",
        "idle",
        vec![fake::unit("acme@0", "api", "idle")],
    )];
    let api = FakeApi::with_lanes(idle);
    let (mut app, _) = loaded(&api, 100, 24).await;
    assert!(matches!(
        app.update(key(KeyCode::Char('q'))).as_slice(),
        [Effect::Quit]
    ));
    assert!(matches!(app.update(ctrl('c')).as_slice(), [Effect::Quit]));
    Ok(())
}

#[tokio::test]
async fn drive_stops_the_lanes_and_remembers_before_closing() -> TestResult {
    let home = tempfile::tempdir()?;
    std::fs::write(
        home.path().join("config.yaml"),
        "# mine\ntui:\n  theme: dark\n",
    )?;
    let mut fake = FakeApi::with_lanes(mixed_lanes());
    fake.home = Some(home.path().to_owned());
    let api = Arc::new(fake);
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let events = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let backend = SharedBackend::new(100, 24);
    let mut terminal = Terminal::new(backend.clone())?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = {
        let backend = backend.clone();
        async move {
            let press = |code| Ok(TermEvent::Key(KeyEvent::new(code, KeyModifiers::NONE)));
            backend.wait_for("┌ @0 · ").await;
            keys_tx.send(press(KeyCode::Char('q'))).await.ok();
            backend.wait_for("remember this choice").await;
            for code in [KeyCode::Char('j'), KeyCode::Char(' '), KeyCode::Enter] {
                keys_tx.send(press(code)).await.ok();
            }
            // The input stays open: the loop must end because the stop finished.
            keys_tx
        }
    };
    let options = hermetic_options();
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        events,
        Box::new(std::io::sink()),
    );
    let (result, _keys) =
        tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, script) }).await?;
    result?;
    let calls = api.calls();
    assert!(calls.contains(&"Stop acme@0 []".to_owned()), "{calls:?}");
    // Written through the comment-keeping editor.
    let config = std::fs::read_to_string(home.path().join("config.yaml"))?;
    assert!(config.starts_with("# mine\n"), "{config}");
    assert!(config.contains("on_quit: stop"), "{config}");
    assert!(config.contains("theme: dark"), "{config}");
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
    let effects = app.update(Msg::EventsEnded(None));
    assert!(matches!(
        effects.as_slice(),
        [Effect::ScheduleReconnect { after_ms: 250 }]
    ));
    assert_eq!(app.connection, Connection::Reconnecting { attempt: 0 });
    // While offline a closed stream waits for the reconnect.
    assert!(app.update(Msg::AttachClosed { generation }).is_empty());
    // A second failure before the retry fires does not schedule another one.
    assert!(
        app.update(Msg::Status(Err(devlanes_tui::ApiError::Disconnected(
            "gone".into()
        ))))
        .is_empty()
    );
    // Status is queried only once the subscription is open.
    let effects = app.update(Msg::Reconnect);
    assert!(matches!(effects.as_slice(), [Effect::Subscribe]));
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
        matches!(effects.first(), Some(Effect::Refresh)),
        "{effects:?}"
    );
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::Attach { unit, .. } if unit == "acme@0:api")),
        "the closed stream is reopened: {effects:?}"
    );
    Ok(())
}

/// A `unit.changed` event for `acme@0:<name>` in `state`.
fn unit_changed(name: &str, state: &str) -> Msg {
    Msg::Event(devlanes_proto::Event {
        seq: 3,
        event: events::UNIT_CHANGED.into(),
        data: json!({ "unit": fake::unit("acme@0", name, state) }),
    })
}

#[tokio::test]
async fn a_restarted_or_exited_unit_is_reattached() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let generation = focus_terminal(&mut app, b"$ ").ok_or("no interactive attach")?;
    // The daemon ends the stream while api is still listed as running: a restart.
    let effects = app.update(Msg::AttachClosed { generation });
    let (generation, interactive) = attach_in(&effects).ok_or("restart not reattached")?;
    assert!(interactive, "the reopened stream keeps its role");
    assert_eq!(app.focus, Focus::Terminal, "and the focus");
    feed(&mut app, generation, b"$ ");
    // An exit: the stream ends and the unit is no longer running.
    app.update(unit_changed("api", "exited"));
    let effects = app.update(Msg::AttachClosed { generation });
    assert!(attach_in(&effects).is_none(), "{effects:?}");
    // It runs again: the next unit.changed reopens it.
    let effects = app.update(unit_changed("api", "starting"));
    assert!(
        matches!(attach_in(&effects), Some((_, true))),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn a_failing_stream_is_not_reopened_in_a_loop() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    app.update(Msg::StreamError {
        generation,
        error: devlanes_tui::ApiError::Decode("bad frame".into()),
    });
    let effects = app.update(Msg::AttachClosed { generation });
    assert!(attach_in(&effects).is_none(), "{effects:?}");
    Ok(())
}

#[tokio::test]
async fn keys_in_a_dead_terminal_never_reach_list_shortcuts() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    feed(&mut app, generation, b"");
    let generation = focus_terminal(&mut app, b"$ ").ok_or("no interactive attach")?;
    app.update(unit_changed("api", "exited"));
    app.update(Msg::AttachClosed { generation });
    assert_eq!(
        app.focus,
        Focus::Terminal,
        "focus stays on the dead terminal"
    );
    // x stops, r restarts and q quits in the list; here they are dropped.
    for code in ['x', 'r', 's', 'q'] {
        let effects = app.update(key(KeyCode::Char(code)));
        assert!(effects.is_empty(), "{code}: {effects:?}");
    }
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("esc goes back to the list"))
    );
    app.update(key(KeyCode::Esc));
    assert_eq!(app.focus, Focus::List);
    Ok(())
}

#[tokio::test]
async fn focus_needs_a_live_stream() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 100, 24).await;
    let generation = select_unit(&mut app, "api").ok_or("api not reachable")?;
    // A unit without a terminal: the pane says why, and focus stays on the list.
    app.update(Msg::AttachOpened {
        generation,
        result: Err(devlanes_tui::ApiError::Rpc(devlanes_proto::RpcError::new(
            "not_running",
            json!({}),
            1,
        ))),
    });
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.focus, Focus::List);
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.starts_with("no terminal"))
    );
    // A stream that ended cleanly.
    let generation = select_unit(&mut app, "web").ok_or("web not reachable")?;
    feed(&mut app, generation, b"");
    app.update(unit_changed("web", "exited"));
    app.update(Msg::AttachClosed { generation });
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.focus, Focus::List);
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("comes back when the unit runs again"))
    );
    Ok(())
}

#[tokio::test]
async fn status_is_queried_after_the_subscription_opens() -> TestResult {
    let mut app = App::new(100, 24);
    assert!(matches!(app.start().as_slice(), [Effect::Subscribe]));
    let effects = app.update(Msg::Subscribed(Ok(())));
    assert!(
        matches!(effects.as_slice(), [Effect::Refresh]),
        "{effects:?}"
    );
    // A daemon that refuses subscriptions can still answer status.
    let effects = app.update(Msg::Subscribed(Err(devlanes_tui::ApiError::Rpc(
        devlanes_proto::RpcError::new("unknown_method", json!({}), 1),
    ))));
    assert!(
        effects.iter().any(|e| matches!(e, Effect::Refresh)),
        "{effects:?}"
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

/// A [`TestBackend`] the test can read while `drive` owns the terminal, so a
/// script waits for a drawn frame instead of guessing how long drawing takes.
#[derive(Clone)]
struct SharedBackend(Arc<std::sync::Mutex<TestBackend>>);

impl SharedBackend {
    /// A `width` x `height` backend.
    fn new(width: u16, height: u16) -> Self {
        Self(Arc::new(std::sync::Mutex::new(TestBackend::new(
            width, height,
        ))))
    }

    /// The backend, whatever a panicking holder left behind.
    fn lock(&self) -> std::sync::MutexGuard<'_, TestBackend> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The last drawn screen as text.
    fn screen(&self) -> String {
        self.lock().to_string()
    }

    /// Waits up to 4 s until the drawn screen contains `text`; panics with the
    /// last screen otherwise, so a test never goes on with a frame it did
    /// not wait for.
    async fn wait_for(&self, text: &str) {
        for _ in 0..400 {
            if self.screen().contains(text) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("never drawn: {text:?}\n{}", self.screen());
    }
}

impl ratatui::backend::Backend for SharedBackend {
    type Error = <TestBackend as ratatui::backend::Backend>::Error;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.lock().draw(content)
    }
    fn hide_cursor(&mut self) -> Result<(), Self::Error> {
        self.lock().hide_cursor()
    }
    fn show_cursor(&mut self) -> Result<(), Self::Error> {
        self.lock().show_cursor()
    }
    fn get_cursor_position(&mut self) -> Result<ratatui::layout::Position, Self::Error> {
        self.lock().get_cursor_position()
    }
    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> Result<(), Self::Error> {
        self.lock().set_cursor_position(position)
    }
    fn clear(&mut self) -> Result<(), Self::Error> {
        self.lock().clear()
    }
    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> Result<(), Self::Error> {
        self.lock().clear_region(clear_type)
    }
    fn size(&self) -> Result<ratatui::layout::Size, Self::Error> {
        self.lock().size()
    }
    fn window_size(&mut self) -> Result<ratatui::backend::WindowSize, Self::Error> {
        self.lock().window_size()
    }
    fn flush(&mut self) -> Result<(), Self::Error> {
        self.lock().flush()
    }
}

/// Polls `api`'s call log until `done` holds, for at most 4 s (the test's own
/// timeout reports a hang).
async fn wait_for_calls(api: &FakeApi, done: impl Fn(&[String]) -> bool) {
    for _ in 0..400 {
        if done(&api.calls()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn drive_loop_resyncs_and_quits_without_stopping() -> TestResult {
    let api = Arc::new(FakeApi::with_lanes(mixed_lanes()));
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let events = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let backend = SharedBackend::new(100, 24);
    let mut terminal = Terminal::new(backend.clone())?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = {
        let api = Arc::clone(&api);
        let backend = backend.clone();
        async move {
            // Wait for the subscription, then ask the TUI to resync.
            for _ in 0..100 {
                if api.emit(events::RESYNC, json!({})).await {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            // Each key waits for the effect of the step before it, not a fixed delay.
            wait_for_calls(&api, |calls| {
                calls.iter().filter(|c| *c == "status").count() >= 2
            })
            .await;
            let press = |c| {
                Ok(TermEvent::Key(KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                )))
            };
            backend.wait_for("┌ @0 · ").await;
            // The lane opens on its failed worker; api is two rows up.
            keys_tx.send(press('k')).await.ok();
            keys_tx.send(press('k')).await.ok();
            // Quit only once the attached pane and the doctor's badge (the
            // fake's checks, fetched on the first tick) are on screen: `q`
            // ends the loop without another frame, so a slower machine would
            // otherwise snapshot an earlier one.
            backend.wait_for("┌ api  ready").await;
            backend.wait_for("D daemon · 2 issues").await;
            keys_tx.send(press('q')).await.ok();
            // Returned, so the input stays open: the loop must end on q, not
            // because the stream ended.
            keys_tx
        }
    };
    // `tui.on_quit: keep`: q closes without asking; the dialog has its own tests.
    let options = Options {
        on_quit: Some("keep".into()),
        ..hermetic_options()
    };
    let run = drive(
        &mut terminal,
        driver_api,
        &options,
        events,
        Box::new(std::io::sink()),
    );
    let (result, _keys) =
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
    // The spinner advances with real time; any frame reads as `⠋`.
    let screen: String = backend
        .lock()
        .to_string()
        .chars()
        .map(|c| {
            if "⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c) {
                '⠋'
            } else {
                c
            }
        })
        .collect();
    assert_screen!("drive_after_resync", screen);
    Ok(())
}

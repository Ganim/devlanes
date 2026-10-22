//! The monitor page: sorting, the selection across re-sorts, the filter,
//! the range, actions on units and lanes, and the graphs on screen.

use crossterm::event::KeyCode;
use devlanes_tui::{
    Metrics, SystemMetrics, UnitMetrics,
    app::{App, Effect, MonitorId, Msg, Selected, SortKey, View},
};
use ratatui::style::Color;

use super::{TestResult, draw, key, loaded, shifted};
use crate::fake::{FakeApi, mixed_lanes};

/// 15 minutes of samples every 5 s ending at `now`, around `cpu`.
fn history(now: u64, cpu: f64, memory: u64) -> Vec<(u64, f64, u64)> {
    (0..180)
        .map(|i| {
            (
                now - (179 - i) * 5_000,
                cpu * (0.5 + (i % 10) as f64 / 10.0),
                memory,
            )
        })
        .collect()
}

const NOW: u64 = 1_800_000_000_000;

/// acme@0's api and web with numbers, and the machine.
fn metrics(api_cpu: f64, web_cpu: f64) -> Metrics {
    let unit = |name: &str, cpu: f64, memory: u64| UnitMetrics {
        lane: "acme@0".into(),
        unit: name.into(),
        kind: "service".into(),
        cpu_percent: Some(cpu),
        memory_bytes: Some(memory),
        history: history(NOW, cpu, memory),
        ..UnitMetrics::default()
    };
    Metrics {
        range_ms: Some(900_000),
        units: vec![
            unit("api", api_cpu, 100 << 20),
            unit("web", web_cpu, 900 << 20),
        ],
        lanes: Vec::new(),
        system: Some(SystemMetrics {
            cpus: Some(4),
            memory_total_bytes: Some(16 << 30),
            available_memory_bytes: Some(8 << 30),
            cpu_percent: Some(60.0),
            memory_pressure: Some("normal".into()),
            cpu_stall_percent: None,
            history: history(NOW, 60.0, 8 << 30),
        }),
    }
}

/// The unit names in table order.
fn order(app: &App) -> Vec<String> {
    app.monitor_rows()
        .iter()
        .filter_map(|row| match row.id() {
            MonitorId::Row { unit, .. } => Some(unit),
            MonitorId::Group(_) => None,
        })
        .collect()
}

/// The monitor open with `metrics`.
async fn monitor(width: u16, height: u16, metrics: Metrics) -> App {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, width, height).await;
    app.update(shifted('M'));
    app.update(Msg::Metrics(Ok(metrics)));
    app
}

#[tokio::test]
async fn the_sort_steps_reverses_and_the_selection_follows_its_row() -> TestResult {
    let mut app = monitor(120, 40, metrics(80.0, 5.0)).await;
    assert_eq!(app.monitor.sort, SortKey::Cpu);
    assert_eq!(order(&app), ["api", "web"], "highest cpu first");
    app.update(key(KeyCode::Char('j')));
    app.update(key(KeyCode::Char('j')));
    let web = MonitorId::Row {
        lane: "acme@0".into(),
        unit: "web".into(),
    };
    assert_eq!(app.monitor.selected, Some(web.clone()));
    // web gets busy and moves up; the cursor goes with it.
    app.update(Msg::Metrics(Ok(metrics(5.0, 80.0))));
    assert_eq!(order(&app), ["web", "api"]);
    assert_eq!(app.monitor.selected, Some(web.clone()));
    assert_eq!(app.monitor.index, 1, "right under the lane row now");
    // `>` sorts by memory, `i` reverses it; `<` goes back.
    app.update(key(KeyCode::Char('>')));
    assert_eq!(app.monitor.sort, SortKey::Mem);
    assert_eq!(order(&app), ["web", "api"]);
    app.update(key(KeyCode::Char('i')));
    assert_eq!(order(&app), ["api", "web"]);
    app.update(key(KeyCode::Char('<')));
    assert_eq!(app.monitor.sort, SortKey::Cpu);
    let text = draw(&app)?.backend().to_string();
    assert!(text.contains("<- cpu ->"), "{text}");
    assert!(text.contains("cpu ▲"), "reversed: {text}");
    Ok(())
}

#[tokio::test]
async fn the_filter_takes_the_keys_and_esc_steps_back() -> TestResult {
    let mut app = monitor(120, 40, metrics(80.0, 5.0)).await;
    app.update(key(KeyCode::Char('/')));
    for c in "web".chars() {
        app.update(key(KeyCode::Char(c)));
    }
    assert_eq!(app.monitor.filter, "web", "e and b went to the filter");
    assert!(!app.monitor.flat);
    assert_eq!(order(&app), ["web"]);
    app.update(key(KeyCode::Enter));
    assert!(!app.monitor.editing);
    let text = draw(&app)?.backend().to_string();
    assert!(text.contains("units /web 1 of 2"), "{text}");
    // Esc clears the kept filter first, then leaves the page.
    app.update(key(KeyCode::Esc));
    assert_eq!(app.view, View::Monitor);
    assert!(app.monitor.filter.is_empty());
    app.update(key(KeyCode::Esc));
    assert_eq!(app.view, View::Project);
    Ok(())
}

#[tokio::test]
async fn the_range_steps_and_refetches() -> TestResult {
    let mut app = monitor(120, 40, metrics(80.0, 5.0)).await;
    let effects = app.update(key(KeyCode::Char('+')));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMetrics(q) if q.range_ms == 3_600_000)),
        "{effects:?}"
    );
    app.update(key(KeyCode::Char('-')));
    let effects = app.update(key(KeyCode::Char('-')));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::FetchMetrics(q) if q.range_ms == 120_000))
    );
    Ok(())
}

#[tokio::test]
async fn s_x_r_act_on_the_unit_or_the_lane_and_enter_opens_it() -> TestResult {
    let mut app = monitor(120, 40, metrics(80.0, 5.0)).await;
    // The lane row: the whole lane.
    let effects = app.update(key(KeyCode::Char('x')));
    assert!(
        matches!(effects.as_slice(), [Effect::Control { lane, selection, .. }]
            if lane == "acme@0" && selection.units.is_empty()),
        "{effects:?}"
    );
    // A unit: r restarts it, s says it already runs.
    app.update(key(KeyCode::Char('j')));
    let effects = app.update(key(KeyCode::Char('r')));
    assert!(
        matches!(effects.as_slice(), [Effect::Control { selection, .. }]
            if selection.units == ["acme@0:api"]),
        "{effects:?}"
    );
    assert!(app.update(key(KeyCode::Char('s'))).is_empty());
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("already running"))
    );
    // Enter opens it in its project with its terminal.
    app.update(key(KeyCode::Enter));
    assert_eq!(app.view, View::Project);
    assert_eq!(app.selected, Some(Selected::unit("acme@0", "api")));
    Ok(())
}

#[tokio::test]
async fn graphs_are_braille_in_gradient_colors_with_the_system_line() -> TestResult {
    let mut app = monitor(120, 40, metrics(80.0, 5.0)).await;
    app.update(key(KeyCode::Char('j')));
    let terminal = draw(&app)?;
    let buffer = terminal.backend().buffer();
    let braille: Vec<Color> = buffer
        .content()
        .iter()
        .filter(|cell| {
            cell.symbol()
                .chars()
                .next()
                .is_some_and(|c| ('\u{2801}'..='\u{28ff}').contains(&c))
        })
        .map(|cell| cell.fg)
        .collect();
    assert!(braille.len() > 50, "braille graphs: {}", braille.len());
    // 16 colors: green for low load, red where one core is exceeded.
    assert!(braille.contains(&Color::Green));
    let text = terminal.backend().to_string();
    // The detail's stats come from the history; its last sample is 112%.
    for expected in [
        "┌ cpu ",
        "of 4 cores · 15m",
        "┌ mem ",
        "of 16.0G",
        "system",
        "-15m",
        "now 112%",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    Ok(())
}

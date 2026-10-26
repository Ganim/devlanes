//! The monitor page: sorting, the selection across re-sorts, the filter,
//! the history asked for, actions on units and lanes, and the graphs on screen.

use crossterm::event::KeyCode;
use devlanes_tui::{
    DaemonMetrics, Metrics, SystemMetrics, UnitMetrics,
    app::{App, Effect, MonitorId, Msg, Selected, SortKey, View},
};
use ratatui::style::Color;

use super::{TestResult, draw, key, loaded, shifted};
use crate::fake::{FakeApi, mixed_lanes};

/// 15 minutes of samples, one a second as the daemon sends them, ending at
/// `now`, around `cpu`.
fn history(now: u64, cpu: f64, memory: u64) -> Vec<(u64, f64, u64)> {
    (0..900)
        .map(|i| {
            (
                now - (899 - i) * 1_000,
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
            core_percents: vec![10.0, 95.0, 40.0, 60.0],
            history: history(NOW, 60.0, 8 << 30),
            available_history: history(NOW, 0.0, 8 << 30)
                .into_iter()
                .map(|p| (p.0, p.2))
                .collect(),
            pressure_history: Vec::new(),
        }),
        daemon: None,
    }
}

/// [`metrics`] with the daemon's own row.
fn with_daemon(mut metrics: Metrics) -> Metrics {
    metrics.daemon = Some(Box::new(DaemonMetrics {
        pid: Some(4242),
        started_at_ms: Some(NOW - 3_600_000),
        metrics: UnitMetrics {
            lane: "devlanes".into(),
            unit: "daemon".into(),
            kind: "daemon".into(),
            cpu_percent: Some(1.5),
            memory_bytes: Some(48 << 20),
            history: history(NOW, 1.5, 48 << 20),
            ..UnitMetrics::default()
        },
    }));
    metrics
}

/// The unit names in table order.
fn order(app: &App) -> Vec<String> {
    app.monitor_rows()
        .iter()
        .filter_map(|row| match row.id() {
            MonitorId::Row { unit, .. } => Some(unit),
            MonitorId::Group(_) | MonitorId::Daemon => None,
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
async fn the_history_asked_for_is_as_many_seconds_as_the_widest_graph() -> TestResult {
    let app = monitor(120, 40, metrics(80.0, 5.0)).await;
    // Before a frame: two seconds per terminal column, raw samples.
    let query = app.metrics_query();
    assert_eq!((query.range_ms, query.points), (240_000, 0));
    // After one: the widest graph's dot columns, one second each.
    draw(&app)?;
    let seconds = app.monitor_seconds.get();
    assert!(seconds > 60, "{seconds}");
    assert_eq!(app.metrics_query().range_ms, seconds as u64 * 1000);
    // The range keys are gone: + and - do nothing.
    let mut app = app;
    assert!(app.update(key(KeyCode::Char('+'))).is_empty());
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

/// A wheel step down (`down`) or up over the screen.
fn wheel(down: bool) -> Msg {
    use crossterm::event::{KeyModifiers, MouseEvent, MouseEventKind};
    Msg::Mouse(MouseEvent {
        kind: if down {
            MouseEventKind::ScrollDown
        } else {
            MouseEventKind::ScrollUp
        },
        column: 10,
        row: 20,
        modifiers: KeyModifiers::NONE,
    })
}

#[tokio::test]
async fn the_wheel_scrolls_the_table_and_leaves_the_selection() -> TestResult {
    let mut many = metrics(80.0, 5.0);
    let template = many.units[0].clone();
    many.units = (0..40)
        .map(|i| UnitMetrics {
            unit: format!("svc{i:02}"),
            cpu_percent: Some(f64::from(40 - i)),
            ..template.clone()
        })
        .collect();
    let mut app = monitor(120, 40, many).await;
    draw(&app)?;
    let selected = app.monitor.selected.clone();
    for _ in 0..3 {
        app.update(wheel(true));
    }
    assert_eq!(
        app.monitor.selected, selected,
        "the wheel moves no selection"
    );
    assert_eq!(app.monitor.offset, 9);
    let text = draw(&app)?.backend().to_string();
    assert!(
        !text.contains("● svc00") && text.contains("● svc08"),
        "{text}"
    );
    // Past the last row it stops; up goes back.
    for _ in 0..30 {
        app.update(wheel(true));
    }
    let last = app.monitor_rows().len() - app.monitor_table_rows.get();
    assert_eq!(app.monitor.offset, last);
    app.update(wheel(false));
    assert_eq!(app.monitor.offset, last - 3);
    // A key brings the selection back into view.
    app.update(key(KeyCode::Char('j')));
    assert_eq!(app.monitor.index, 1);
    assert_eq!(app.monitor.offset, 1);
    Ok(())
}

#[tokio::test]
async fn m_puts_memory_in_the_big_box_and_cpu_shows_its_cores() -> TestResult {
    let mut app = monitor(120, 40, metrics(80.0, 5.0)).await;
    app.update(key(KeyCode::Char('m')));
    assert!(app.monitor.memory_first);
    let text = draw(&app)?.backend().to_string();
    let top = text
        .lines()
        .find(|line| line.contains("┌ mem"))
        .unwrap_or_default();
    let (mem, cpu) = (top.find("┌ mem"), top.find("┌ cpu"));
    assert!(mem.is_some() && cpu.is_some() && mem < cpu, "{text}");
    assert!(text.contains("■ ■ ■ ■"), "{text}");
    assert!(text.contains("c cpu graph"), "{text}");
    // Four cores, four squares in the cpu box.
    let squares: usize = text
        .lines()
        .filter(|line| line.contains('■') && !line.contains('%'))
        .map(|line| line.matches('■').count())
        .sum();
    assert_eq!(squares, 4, "{text}");
    app.update(key(KeyCode::Char('c')));
    assert!(!app.monitor.memory_first);
    let text = draw(&app)?.backend().to_string();
    assert!(!text.contains("■ ■ ■ ■"), "{text}");
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
    // The detail's `now` is the current reading (80%); avg, min and max
    // come from the history.
    for expected in [
        "┌ cpu ",
        "of 4 cores ",
        "┌ mem ",
        "of 16.0G",
        "system",
        // The axis says how far back the graph's columns reach.
        "└-1m",
        "now 80%",
        "max 112%",
    ] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    Ok(())
}

#[tokio::test]
async fn the_daemon_row_stays_under_the_table_and_shows_its_own_graphs() -> TestResult {
    let mut many = with_daemon(metrics(80.0, 5.0));
    let template = many.units[0].clone();
    many.units = (0..40)
        .map(|i| UnitMetrics {
            unit: format!("svc{i:02}"),
            cpu_percent: Some(f64::from(40 - i)),
            ..template.clone()
        })
        .collect();
    let mut app = monitor(120, 40, many).await;
    let text = draw(&app)?.backend().to_string();
    assert!(
        text.contains("devlanes daemon"),
        "pinned before scrolling: {text}"
    );
    // The wheel scrolls the units; the daemon's row stays.
    for _ in 0..3 {
        app.update(wheel(true));
    }
    let text = draw(&app)?.backend().to_string();
    assert!(
        !text.contains("● svc00") && text.contains("devlanes daemon"),
        "{text}"
    );
    // `G` selects it without scrolling, and the detail shows its graphs.
    app.update(key(KeyCode::Char('G')));
    assert_eq!(app.monitor.selected, Some(MonitorId::Daemon));
    assert_eq!(app.monitor.offset, 9);
    let text = draw(&app)?.backend().to_string();
    for expected in ["devlanes daemon · cpu", "pid 4242", "now 1.5%"] {
        assert!(text.contains(expected), "{expected}: {text}");
    }
    // Nothing to open or control from here.
    assert!(app.update(key(KeyCode::Enter)).is_empty());
    assert_eq!(app.view, View::Monitor);
    assert!(app.update(key(KeyCode::Char('x'))).is_empty());
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("daemon page"))
    );
    // The daemon is in no total: devlanes in the mem box is api and web only.
    assert!(!order(&app).contains(&"daemon".to_owned()));
    Ok(())
}

#[tokio::test]
async fn the_memory_graph_is_the_hosts_memory_colored_by_its_pressure() -> TestResult {
    let mut data = metrics(80.0, 5.0);
    // Pressure `warn` from 40 to 20 seconds ago, normal before and after.
    if let Some(system) = data.system.as_mut() {
        system.pressure_history = (0..900)
            .map(|i| {
                let level = if (860..880).contains(&i) { 1.0 } else { 0.0 };
                (NOW - (899 - i) * 1_000, level)
            })
            .collect();
    }
    let mut app = monitor(120, 40, data).await;
    app.update(key(KeyCode::Char('m')));
    let terminal = draw(&app)?;
    let text = terminal.backend().to_string();
    // 16 GiB of host memory at the top, half of it in the middle.
    assert!(text.contains(" 16G┤") && text.contains("8.0G┤"), "{text}");
    // The mem box, in the top-left corner: from its top border to its
    // bottom one and its right edge.
    let buffer = terminal.backend().buffer();
    let size = buffer.area;
    let top = (0..size.height)
        .find(|y| buffer[(0, *y)].symbol() == "┌")
        .unwrap_or(0);
    let bottom = (top + 1..size.height)
        .find(|y| buffer[(0, *y)].symbol() == "└")
        .unwrap_or(top);
    let right = (1..size.width)
        .find(|x| buffer[(*x, top)].symbol() == "┐")
        .unwrap_or(0);
    let mut columns: Vec<(u16, Color)> = Vec::new();
    for y in top..bottom {
        for x in 0..right {
            let cell = &buffer[(x, y)];
            let braille = cell
                .symbol()
                .chars()
                .next()
                .is_some_and(|c| ('\u{2801}'..='\u{28ff}').contains(&c));
            if braille && cell.fg != Color::Reset {
                columns.push((x, cell.fg));
            }
        }
    }
    let colored: Vec<Color> = columns.iter().map(|c| c.1).collect();
    // The warn seconds are orange-red in 16 colors; the rest keep the
    // load colors, so the newest column (normal) is green.
    assert!(colored.contains(&Color::LightRed), "{colored:?}\n{text}");
    assert!(colored.contains(&Color::Green), "{colored:?}");
    let newest = columns.iter().max_by_key(|c| c.0).map(|c| c.1);
    assert_eq!(newest, Some(Color::Green));
    Ok(())
}

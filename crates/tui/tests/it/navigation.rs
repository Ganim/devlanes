//! Moving around a project: lanes, groups, the filter and problems.

use crossterm::event::KeyCode;
use devlanes_proto::LaneSummary;
use devlanes_tui::app::{App, Msg, RowId, Section, Selected};

use super::{TestResult, draw, key, loaded};
use crate::fake::{FakeApi, lane, unit};

/// `acme@0` with api, web and wrk families, one failed worker; `acme@1`
/// with an orphan; `acme@2` stopped.
fn lanes() -> Vec<LaneSummary> {
    let mut units: Vec<_> = [
        "api-a", "api-b", "api-c", "web-a", "web-b", "wrk-a", "wrk-b",
    ]
    .iter()
    .map(|name| unit("acme@0", name, "ready"))
    .collect();
    units.push(unit("acme@0", "docs", "idle"));
    units[6].state = "failed".into();
    units[6].exit_code = Some(1);
    let orphan = devlanes_proto::UnitSummary {
        pid: Some(7),
        ..unit("acme@1", "api-a", devlanes_proto::ORPHANED)
    };
    vec![
        lane("acme", Some(0), "main", "running", units),
        lane("acme", Some(1), "feat", "running", vec![orphan]),
        lane(
            "acme",
            Some(2),
            "old",
            "idle",
            vec![unit("acme@2", "api-a", "idle")],
        ),
    ]
}

/// The ids of the visible rows.
fn ids(app: &App) -> Vec<RowId> {
    app.rows().into_iter().map(|row| row.id).collect()
}

#[tokio::test]
async fn a_lane_opens_on_its_first_problem_and_n_walks_the_project() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    assert_eq!(app.selected, Some(Selected::unit("acme@0", "wrk-b")));
    app.update(key(KeyCode::Char('n')));
    assert_eq!(
        app.selected,
        Some(Selected::unit("acme@1", "api-a")),
        "n crosses to the next lane's problem"
    );
    app.update(key(KeyCode::Char('n')));
    assert_eq!(
        app.selected,
        Some(Selected::unit("acme@0", "wrk-b")),
        "and wraps"
    );
    app.update(key(KeyCode::Char('N')));
    assert_eq!(app.selected, Some(Selected::unit("acme@1", "api-a")));
    Ok(())
}

#[tokio::test]
async fn lanes_switch_with_brackets_and_keep_their_selection() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    app.update(key(KeyCode::Char('k')));
    let before = app.selected.clone();
    app.update(key(KeyCode::Char(']')));
    app.update(key(KeyCode::Char(']')));
    assert_eq!(
        app.selected,
        Some(Selected::lane("acme@2")),
        "a stopped lane opens on its whole-lane row"
    );
    app.update(key(KeyCode::Char(']')));
    assert_eq!(
        app.selected,
        Some(Selected::lane("acme@2")),
        "] stops at the end"
    );
    app.update(key(KeyCode::Char('[')));
    app.update(key(KeyCode::Char('[')));
    assert_eq!(app.selected, before, "the row selected there last");
    Ok(())
}

#[tokio::test]
async fn services_are_one_list_in_config_order_with_stopped_ones_inline() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (app, _) = loaded(&api, 100, 30).await;
    let rows = ids(&app);
    // The daemon's order (devlanes.yaml), and docs stays in it although it
    // is idle while the lane runs.
    let units: Vec<&RowId> = rows
        .iter()
        .filter(|r| matches!(r, RowId::Unit(_)))
        .collect();
    let names = [
        "api-a", "api-b", "api-c", "web-a", "web-b", "wrk-a", "wrk-b", "docs",
    ];
    assert_eq!(
        units,
        names
            .map(|n| RowId::Unit(n.into()))
            .iter()
            .collect::<Vec<_>>()
    );
    assert!(!rows.iter().any(|r| matches!(r, RowId::Group(..))));
    assert!(!rows.contains(&RowId::WholeLane));
    // Normal states are a glyph alone; a failure says so; a stopped service
    // is dimmed with its stopped glyph.
    let text = draw(&app)?.backend().to_string();
    let line = |name: &str| {
        text.lines()
            .find(|l| l.contains(name))
            .unwrap_or_default()
            .to_owned()
    };
    assert!(!line("● api-a").contains("ready"), "{text}");
    assert!(line("✕ wrk-b").contains("failed"), "{text}");
    assert!(line("docs").contains("· docs"), "{text}");
    Ok(())
}

#[tokio::test]
async fn a_unit_that_stops_stays_in_sight_while_selected() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    let mut effects = Vec::new();
    app.select(Selected::unit("acme@0", "web-a"), &mut effects);
    app.update(Msg::Event(devlanes_proto::Event {
        seq: 2,
        event: devlanes_proto::events::UNIT_CHANGED.into(),
        data: serde_json::json!({ "unit": unit("acme@0", "web-a", "stopped") }),
    }));
    assert_eq!(app.selected, Some(Selected::unit("acme@0", "web-a")));
    assert!(ids(&app).contains(&RowId::Unit("web-a".into())));
    Ok(())
}

#[tokio::test]
async fn a_lane_that_does_not_fit_folds_tasks_and_infra_never_services() -> TestResult {
    let mut units: Vec<_> = (0..6)
        .map(|i| unit("acme@0", &format!("api-{i:02}"), "ready"))
        .chain((0..8).map(|i| {
            let mut task = unit("acme@0", &format!("task-{i}"), "exited");
            task.kind = "task".into();
            task.exit_code = Some(0);
            task
        }))
        .collect();
    units[0].state = "failed".into();
    let api = FakeApi::with_lanes(vec![lane("acme", Some(0), "main", "running", units)]);
    let (app, _) = loaded(&api, 100, 20).await;
    let folded = |section: Section| {
        app.rows()
            .iter()
            .find(|row| row.id == RowId::Section(section))
            .and_then(|row| row.folded)
    };
    assert_eq!(
        folded(Section::Tasks),
        Some(true),
        "healthy tasks fold to make room"
    );
    assert_eq!(folded(Section::Services), Some(false), "services stay open");
    assert!(
        app.rows().len() <= app.list_height(),
        "{}",
        app.rows().len()
    );
    Ok(())
}

#[tokio::test]
async fn h_and_l_fold_and_unfold_and_h_climbs_to_the_section() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    let services = RowId::Section(Section::Services);
    app.update(key(KeyCode::Char('h')));
    assert_eq!(
        app.selected_id(),
        Some(&services),
        "h on a unit selects its section"
    );
    app.update(key(KeyCode::Char('h')));
    assert!(!ids(&app).contains(&RowId::Unit("wrk-a".into())), "h folds");
    app.update(key(KeyCode::Char('l')));
    assert!(
        ids(&app).contains(&RowId::Unit("wrk-a".into())),
        "l unfolds"
    );
    app.update(key(KeyCode::Enter));
    assert!(
        !ids(&app).contains(&RowId::Unit("wrk-a".into())),
        "Enter folds"
    );
    // j and k step over the blank row between sections.
    app.update(key(KeyCode::Char('k')));
    assert_eq!(app.selected_id(), Some(&RowId::WholeLane));
    app.update(key(KeyCode::Char('j')));
    assert_eq!(app.selected_id(), Some(&services));
    Ok(())
}

#[tokio::test]
async fn the_filter_narrows_opens_and_clears() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    app.update(key(KeyCode::Char('/')));
    for c in "web".chars() {
        app.update(key(KeyCode::Char(c)));
    }
    let rows = ids(&app);
    assert!(rows.contains(&RowId::Unit("web-a".into())));
    assert!(!rows.contains(&RowId::Unit("api-a".into())));
    // Letters went to the filter, not to `w`/`e`/`b` shortcuts.
    assert_eq!(app.filter.as_ref().map(|f| f.text.as_str()), Some("web"));
    let text = draw(&app)?.backend().to_string();
    assert!(text.contains("main @0 · filter: web"), "{text}");
    app.update(key(KeyCode::Enter));
    assert!(
        app.filter.as_ref().is_some_and(|f| !f.editing),
        "enter keeps it"
    );
    app.update(key(KeyCode::Char('j')));
    assert!(app.filter.is_some(), "j moves in the filtered list");
    app.update(key(KeyCode::Esc));
    assert!(app.filter.is_none(), "esc clears it");
    assert!(ids(&app).contains(&RowId::Unit("api-a".into())));
    Ok(())
}

#[tokio::test]
async fn section_actions_send_their_units() -> TestResult {
    let api = FakeApi::with_lanes(lanes());
    let (mut app, _) = loaded(&api, 100, 30).await;
    let mut effects = Vec::new();
    app.select(
        Selected {
            lane: "acme@0".into(),
            row: RowId::Section(Section::Services),
        },
        &mut effects,
    );
    // Restart sends what runs.
    let effects = app.update(key(KeyCode::Char('r')));
    let [devlanes_tui::app::Effect::Control { selection, .. }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(
        selection.units,
        ["api-a", "api-b", "api-c", "web-a", "web-b", "wrk-a"].map(|n| format!("acme@0:{n}"))
    );
    // Start sends only what does not run yet: the failed one and docs.
    let effects = app.update(key(KeyCode::Char('s')));
    let [devlanes_tui::app::Effect::Control { selection, .. }] = effects.as_slice() else {
        return Err(format!("unexpected {effects:?}").into());
    };
    assert_eq!(selection.units, ["acme@0:wrk-b", "acme@0:docs"]);
    // A unit that runs: start says so and sends nothing.
    app.select(Selected::unit("acme@0", "api-a"), &mut Vec::new());
    assert!(app.update(key(KeyCode::Char('s'))).is_empty());
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("already running"))
    );
    Ok(())
}

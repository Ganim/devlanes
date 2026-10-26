//! Infra container rows show the container's Docker logs in the pane.

use std::collections::BTreeMap;

use crossterm::event::KeyCode;
use devlanes_proto::{ContainerSummary, RpcError};
use devlanes_tui::{
    ApiError,
    app::{App, AttachState, Effect, Msg, RowId, Selected},
};
use serde_json::json;

use crate::errors::{key, render, row};
use crate::fake::{FakeApi, lane, unit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A running lane with its own `cache` container and the shared `postgres`.
fn lanes() -> Vec<devlanes_proto::LaneSummary> {
    let mut acme = lane(
        "acme",
        Some(0),
        "main",
        "running",
        vec![unit("acme@0", "api", "ready")],
    );
    let container = |name: &str, shared: bool, port: u16| ContainerSummary {
        name: name.into(),
        ports: BTreeMap::from([("main".to_owned(), port)]),
        shared,
        state: Some("healthy".into()),
        ..ContainerSummary::default()
    };
    acme.containers = vec![
        container("cache", false, 6379),
        container("postgres", true, 5432),
    ];
    vec![acme]
}

/// An app with the lanes loaded and `postgres` selected; returns the
/// generation of the logs it follows.
async fn on_postgres(app: &mut App) -> Result<u64, Box<dyn std::error::Error>> {
    let api = FakeApi::with_lanes(lanes());
    app.update(Msg::Status(devlanes_tui::DaemonApi::status(&api).await));
    let mut effects = Vec::new();
    app.select(
        Selected {
            lane: "acme@0".into(),
            row: RowId::Container("postgres".into()),
        },
        &mut effects,
    );
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::FollowContainer {
                generation,
                lane,
                container,
                tail,
            } if lane == "acme@0" && container == "postgres" && *tail > 0 => Some(*generation),
            _ => None,
        })
        .ok_or_else(|| format!("no FollowContainer: {effects:?}").into())
}

#[tokio::test]
/// Selecting a container follows its logs; the lines land in the pane like a
/// unit's output, and the title keeps its state, scope and ports.
async fn a_container_row_shows_its_docker_logs() -> TestResult {
    let mut app = App::new(120, 30);
    let generation = on_postgres(&mut app).await?;
    app.update(Msg::ContainerLogsOpened {
        generation,
        result: Ok(()),
    });
    app.update(Msg::AttachOutput {
        generation,
        bytes: b"LOG:  database system is ready to accept connections\r\n".to_vec(),
    });
    let attached = app.attached.as_ref().ok_or("nothing attached")?;
    assert!(attached.container && !attached.interactive);
    assert_eq!(attached.state, AttachState::Live);
    let buffer = render(&app)?;
    let text: Vec<String> = (0..buffer.area.height).map(|y| row(&buffer, y)).collect();
    let text = text.join("\n");
    assert!(text.contains("ready to accept connections"), "{text}");
    assert!(text.contains("shared") && text.contains("5432"), "{text}");
    assert!(text.contains("docker logs"), "{text}");
    assert!(
        !text.contains("Containers start and stop"),
        "the logs replace the facts: {text}"
    );

    // Read-only: Enter does not type into it.
    let effects = app.update(key(KeyCode::Enter));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::Attach { .. } | Effect::Input(_))),
        "{effects:?}"
    );
    assert!(
        app.notice
            .as_ref()
            .is_some_and(|n| n.text.contains("read-only")),
        "{:?}",
        app.notice
    );

    // Another row ends the stream.
    let mut effects = Vec::new();
    app.select(
        Selected {
            lane: "acme@0".into(),
            row: RowId::Container("cache".into()),
        },
        &mut effects,
    );
    assert!(
        matches!(effects.first(), Some(Effect::Detach)),
        "{effects:?}"
    );
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::FollowContainer { container, .. } if container == "cache")
        ),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
/// A daemon without `infra.logs` leaves the facts in the pane with a line
/// saying why, and reports nothing. Its refusal ends the stream, after the
/// request went out, as the runtime delivers it.
async fn an_older_daemon_keeps_the_facts() -> TestResult {
    let mut app = App::new(120, 30);
    let generation = on_postgres(&mut app).await?;
    app.update(Msg::ContainerLogsOpened {
        generation,
        result: Ok(()),
    });
    app.update(Msg::StreamError {
        generation,
        error: ApiError::Rpc(RpcError::new(
            "unknown_method",
            json!({"method": "infra.logs"}),
            2,
        )),
    });
    app.update(Msg::AttachClosed { generation });
    assert!(app.errors.is_empty(), "{:?}", app.errors);
    let footer = {
        let buffer = render(&app)?;
        row(&buffer, buffer.area.height - 1)
    };
    assert!(
        !footer.contains("select text"),
        "nothing to select: {footer}"
    );
    let buffer = render(&app)?;
    let text: Vec<String> = (0..buffer.area.height).map(|y| row(&buffer, y)).collect();
    let text = text.join("\n");
    assert!(text.contains("cannot show container logs"), "{text}");
    assert!(text.contains("shared by every lane"), "{text}");
    Ok(())
}

#[tokio::test]
/// A real failure, such as Docker being down, goes to the error list.
async fn a_failed_follow_is_reported() -> TestResult {
    let mut app = App::new(120, 30);
    let generation = on_postgres(&mut app).await?;
    app.update(Msg::ContainerLogsOpened {
        generation,
        result: Ok(()),
    });
    app.update(Msg::StreamError {
        generation,
        error: ApiError::Rpc(RpcError::new(
            "docker_unavailable",
            json!({"message": "Cannot connect to the Docker daemon"}),
            1,
        )),
    });
    assert_eq!(app.errors.len(), 1);
    assert!(
        app.errors[0].what.contains("container postgres"),
        "{:?}",
        app.errors[0]
    );
    // A connection that cannot open at all says so the same way.
    let mut fresh = App::new(120, 30);
    let generation = on_postgres(&mut fresh).await?;
    fresh.update(Msg::ContainerLogsOpened {
        generation,
        result: Err(ApiError::Rpc(RpcError::new(
            "docker_unavailable",
            json!({"message": "Cannot connect to the Docker daemon"}),
            1,
        ))),
    });
    assert_eq!(fresh.errors.len(), 1);
    Ok(())
}

#[tokio::test]
/// Scrolling a container's logs stays in the pane's scrollback: devlanes
/// keeps no stored history for it to fetch.
async fn scrolling_a_container_never_asks_for_stored_logs() -> TestResult {
    let mut app = App::new(120, 30);
    let generation = on_postgres(&mut app).await?;
    app.update(Msg::ContainerLogsOpened {
        generation,
        result: Ok(()),
    });
    let lines: String = (0..200).map(|i| format!("line {i}\r\n")).collect();
    app.update(Msg::AttachOutput {
        generation,
        bytes: lines.into_bytes(),
    });
    for _ in 0..40 {
        let effects = app.update(key(KeyCode::PageUp));
        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::FetchLogs { .. })),
            "{effects:?}"
        );
    }
    let attached = app.attached.as_ref().ok_or("nothing attached")?;
    assert!(attached.screen.scroll_offset() > 0);
    Ok(())
}

#[tokio::test]
/// Zoom reopens the logs at the new size; it never turns into a terminal.
async fn zoom_reopens_the_logs() -> TestResult {
    let mut app = App::new(120, 30);
    let generation = on_postgres(&mut app).await?;
    app.update(Msg::ContainerLogsOpened {
        generation,
        result: Ok(()),
    });
    let mut effects = app.update(key(KeyCode::Char('z')));
    // A resize settles before the stream reopens.
    let settle = effects.iter().find_map(|e| match e {
        Effect::SettleResize { token, .. } => Some(*token),
        _ => None,
    });
    if let Some(token) = settle {
        effects.extend(app.update(Msg::ResizeSettled { token }));
    }
    assert!(
        effects.iter().any(
            |e| matches!(e, Effect::FollowContainer { container, .. } if container == "postgres")
        ),
        "{effects:?}"
    );
    assert!(
        !effects.iter().any(|e| matches!(e, Effect::Attach { .. })),
        "{effects:?}"
    );
    assert!(
        app.attached
            .as_ref()
            .is_some_and(|a| a.container && !a.interactive)
    );
    Ok(())
}

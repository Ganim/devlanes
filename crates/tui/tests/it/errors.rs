//! Errors stay inside their widgets, attach to what failed and stay readable.

use std::{sync::Arc, time::Duration};

use crossterm::event::{Event as TermEvent, KeyCode, KeyEvent, KeyModifiers};
use devlanes_proto::{AttachResult, RpcError, UnitSummary, events};
use devlanes_tui::{
    ApiError, DaemonApi, Options,
    app::{App, Dialog, Msg, Tab, Target, areas, error_panel_area},
    drive, ui,
};
use futures_util::stream;
use ratatui::{Terminal, backend::TestBackend, buffer::Buffer};
use serde_json::json;
use tokio::sync::mpsc;

use crate::fake::{FakeApi, lane, unit};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// A key press without modifiers.
pub fn key(code: KeyCode) -> Msg {
    Msg::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Renders into a fresh buffer of the app's size.
pub fn render(app: &App) -> Result<Buffer, Box<dyn std::error::Error>> {
    let mut terminal = Terminal::new(TestBackend::new(app.size.0, app.size.1))?;
    terminal.draw(|frame| ui::render(app, frame))?;
    Ok(terminal.backend().buffer().clone())
}

/// One buffer row as text.
pub fn row(buffer: &Buffer, y: u16) -> String {
    (0..buffer.area.width)
        .map(|x| buffer[(x, y)].symbol())
        .collect()
}

/// The whole buffer, one quoted row per line, for snapshots.
pub fn screen(buffer: &Buffer) -> String {
    (0..buffer.area.height)
        .map(|y| format!("\"{}\"", row(buffer, y)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A compose failure as the daemon sends it: one `message` with the command,
/// the exit status and multi-line stderr, plus ANSI colors and tabs.
fn compose_error() -> RpcError {
    let path = "/work/acme/worktrees/portal-checkout-with-a-long-directory-name";
    let message = format!(
        "`docker compose -p acme -f {path}/compose.yaml -f {path}/.devlanes/override.yaml --project-directory {path} up -d --wait --remove-orphans` failed (exit status: 1): \x1b[31merror\x1b[0m: open {path}/compose.yaml: no such file or directory\n\tcompose could not load the project files listed in COMPOSE_FILE\r\nrun docker compose config to check the files\nlast line of stderr"
    );
    assert!(message.len() >= 400, "{}", message.len());
    RpcError::new(
        "compose_failed",
        json!({"message": message, "lane": "acme@1", "started": []}),
        1,
    )
}

/// One project: a running lane 0 and an idle lane 1 with the `portal` unit.
fn lanes() -> Vec<devlanes_proto::LaneSummary> {
    vec![
        lane(
            "acme",
            Some(0),
            "main",
            "running",
            vec![unit("acme@0", "api", "ready")],
        ),
        lane(
            "acme",
            Some(1),
            "feat/portal",
            "idle",
            vec![unit("acme@1", "portal", "idle")],
        ),
    ]
}

/// An app showing `acme@1:portal` with no terminal (it is not running).
async fn portal_app(width: u16, height: u16) -> Result<App, Box<dyn std::error::Error>> {
    let api = FakeApi::with_lanes(lanes());
    let mut app = App::new(width, height);
    app.update(Msg::Status(api.status().await));
    for _ in 0..10 {
        if app.selected_lane().is_some_and(|l| l.reference == "acme@1") {
            break;
        }
        app.update(key(KeyCode::Char('j')));
    }
    app.update(key(KeyCode::Char('l')));
    app.update(key(KeyCode::Char('j')));
    let generation = app
        .attached
        .as_ref()
        .filter(|a| a.unit == "portal")
        .map(|a| a.generation)
        .ok_or("portal is not selected")?;
    app.update(Msg::AttachOpened {
        generation,
        result: Err(ApiError::Rpc(RpcError::new(
            "not_running",
            json!({"lane": "acme@1", "unit": "portal"}),
            1,
        ))),
    });
    assert!(
        app.errors.is_empty(),
        "a unit that is not running is not an error"
    );
    Ok(app)
}

/// The failed start the owner reported, pressed as `s` on the unit.
fn fail_start(app: &mut App) {
    app.update(Msg::ActionDone {
        label: "start acme@1:portal".into(),
        target: Target::Unit {
            lane: "acme@1".into(),
            unit: "portal".into(),
        },
        result: Err(ApiError::Rpc(compose_error())),
    });
}

/// Checks that the notice changed nothing outside the header and that the
/// details pane holds the wrapped error inside its border.
fn check_layout(app: &mut App) -> Result<Buffer, Box<dyn std::error::Error>> {
    let with_notice = render(app)?;
    let notice = app.notice.take();
    let without = render(app)?;
    app.notice = notice;
    let (width, height) = app.size;
    for y in 1..height {
        assert_eq!(
            row(&with_notice, y),
            row(&without, y),
            "row {y} changed with the notice"
        );
    }
    assert!(row(&with_notice, 0).contains("! start acme@1:portal"));
    assert!(
        row(&with_notice, 0).ends_with("… "),
        "notice is clipped: {}",
        row(&with_notice, 0)
    );
    let layout = areas(width, height, false).ok_or("too small")?;
    let pane = layout.pane;
    for y in pane.y + 1..pane.bottom() - 1 {
        assert_eq!(
            with_notice[(pane.x, y)].symbol(),
            "│",
            "left pane border at row {y}"
        );
        assert_eq!(
            with_notice[(pane.right() - 1, y)].symbol(),
            "│",
            "right pane border at row {y}"
        );
    }
    let pane_text: Vec<String> = (layout.pane_inner.y..layout.pane_inner.bottom())
        .map(|y| {
            (layout.pane_inner.x..layout.pane_inner.right())
                .map(|x| with_notice[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let joined = pane_text.join("\n");
    assert!(joined.contains("start acme@1:portal failed"), "{joined}");
    assert!(
        joined.contains("compose failed (exit status: 1)"),
        "{joined}"
    );
    // stderr comes before the command: it says what went wrong.
    assert!(joined.contains("error: open"), "{joined}");
    assert!(joined.contains("file or directory"), "{joined}");
    if height >= 40 {
        assert!(
            joined.contains("command: docker compose -p acme"),
            "{joined}"
        );
        assert!(joined.contains("hint: devlanes infra status"), "{joined}");
        assert!(
            joined.contains("    compose could not load"),
            "stderr indent: {joined}"
        );
    } else {
        assert!(joined.contains("… e shows the full error"), "{joined}");
    }
    // The badge may be clipped by a narrow list, never pushed past its border.
    let list = layout.list.ok_or("no list")?;
    let rows: Vec<String> = (list.y..list.bottom())
        .map(|y| row(&with_notice, y))
        .collect();
    assert!(
        rows.iter().any(|r| r.contains("portal  idle  ! start")),
        "row badge missing: {rows:#?}"
    );
    Ok(with_notice)
}

#[tokio::test]
async fn compose_failure_at_80x24() -> TestResult {
    let mut app = portal_app(80, 24).await?;
    fail_start(&mut app);
    let buffer = check_layout(&mut app)?;
    insta::assert_snapshot!("compose_failure_80x24", screen(&buffer));
    Ok(())
}

#[tokio::test]
async fn compose_failure_at_160x45() -> TestResult {
    let mut app = portal_app(160, 45).await?;
    fail_start(&mut app);
    let buffer = check_layout(&mut app)?;
    insta::assert_snapshot!("compose_failure_160x45", screen(&buffer));
    Ok(())
}

#[tokio::test]
async fn error_list_shows_full_text_and_scrolls() -> TestResult {
    let mut app = portal_app(80, 24).await?;
    fail_start(&mut app);
    app.update(Msg::Failed {
        what: "open https://acme.localhost".into(),
        message: "xdg-open exited with exit status: 3".into(),
    });
    app.update(key(KeyCode::Char('e')));
    assert!(matches!(app.dialog, Some(Dialog::Errors { scroll: 0 })));
    let buffer = render(&app)?;
    let panel = error_panel_area(app.size);
    assert_eq!(buffer[(panel.x, panel.y)].symbol(), "┌");
    assert_eq!(
        buffer[(panel.right() - 1, panel.bottom() - 1)].symbol(),
        "┘"
    );
    insta::assert_snapshot!("error_list_80x24", screen(&buffer));
    // Enough errors to need scrolling; it stops once the last line is at the bottom.
    for index in 0..6 {
        app.update(Msg::Failed {
            what: format!("open terminal {index}"),
            message: "set terminal in the user config".into(),
        });
    }
    app.update(key(KeyCode::Esc));
    app.update(key(KeyCode::Char('e')));
    for _ in 0..200 {
        app.update(key(KeyCode::Char('j')));
    }
    let Some(Dialog::Errors { scroll }) = app.dialog else {
        return Err("error list closed".into());
    };
    let total = app.error_panel_lines(usize::from(panel.width - 2)).len();
    let height = usize::from(panel.height - 2);
    assert!(total > height, "the list must overflow for this test");
    assert_eq!(scroll, total - height);
    let bottom = render(&app)?;
    let last = row(&bottom, panel.bottom() - 2);
    assert!(last.contains("hint: devlanes infra status"), "{last}");
    app.update(key(KeyCode::Esc));
    assert!(app.dialog.is_none());
    assert_eq!(app.errors.len(), 8, "closing keeps the list");
    Ok(())
}

#[tokio::test]
async fn badge_stays_until_the_target_recovers_and_the_list_keeps_it() -> TestResult {
    let mut app = portal_app(100, 24).await?;
    fail_start(&mut app);
    let target = Target::Unit {
        lane: "acme@1".into(),
        unit: "portal".into(),
    };
    assert!(app.target_error(&target).is_some());
    // Ticks age the header notice, not the badge or the list.
    for _ in 0..30 {
        app.update(Msg::Tick);
    }
    assert!(app.notice.is_none());
    assert!(app.target_error(&target).is_some());
    let changed = |state: &str| {
        Msg::Event(devlanes_proto::Event {
            seq: 3,
            event: events::UNIT_CHANGED.into(),
            data: json!({ "unit": UnitSummary { state: state.into(), ..unit("acme@1", "portal", state) } }),
        })
    };
    app.update(changed("starting"));
    assert!(
        app.target_error(&target).is_some(),
        "starting is not recovery"
    );
    app.update(changed("ready"));
    assert!(app.target_error(&target).is_none());
    assert_eq!(app.errors.len(), 1);
    Ok(())
}

#[tokio::test]
async fn every_failure_goes_to_the_list() -> TestResult {
    let mut app = portal_app(100, 24).await?;
    let rpc = |code: &str| ApiError::Rpc(RpcError::new(code, json!({"message": "boom"}), 1));
    app.update(Msg::Subscribed(Err(rpc("unsupported_method"))));
    app.update(Msg::Reconnect);
    app.update(Msg::Subscribed(Ok(())));
    app.update(Msg::Status(Err(rpc("state_error"))));
    app.update(Msg::Metrics(Err(rpc("not_supported_yet"))));
    // The same metrics failure every tick is reported once.
    app.update(Msg::Metrics(Err(rpc("not_supported_yet"))));
    app.update(Msg::RunDone {
        lane: "acme@1".into(),
        task: "seed".into(),
        result: Err(rpc("unit_failed")),
    });
    app.update(Msg::Failed {
        what: "copy".into(),
        message: "broken pipe".into(),
    });
    let generation = app
        .attached
        .as_ref()
        .map(|a| a.generation)
        .ok_or("no attach")?;
    app.update(Msg::StreamError {
        generation,
        error: rpc("bad_request"),
    });
    app.update(Msg::EventsEnded(Some(ApiError::Disconnected(
        "client I/O: connection reset".into(),
    ))));
    let whats: Vec<&str> = app.errors.iter().map(|e| e.what.as_str()).collect();
    assert_eq!(
        whats,
        [
            "daemon connection",
            "terminal acme@1:portal",
            "copy",
            "run seed in acme@1",
            "metrics",
            "status",
            "daemon connection",
            "subscribe",
        ]
    );
    for entry in &app.errors {
        assert!(!entry.worded.summary.contains("RpcError"), "{entry:?}");
        assert!(!entry.worded.summary.contains('{'), "{entry:?}");
    }
    // A task that is not a listed unit attaches its failure to the lane.
    assert!(app.target_error(&Target::Lane("acme@1".into())).is_some());
    Ok(())
}

// ---- long and hostile text everywhere -------------------------------------------

/// xorshift64: deterministic randomness without a dependency.
struct Rng(u64);

impl Rng {
    /// Next value.
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// A value below `n`.
    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap_or(1)).unwrap_or(0)
    }

    /// A string of up to `max` pieces mixing words, wide characters,
    /// control bytes, escape sequences and long unbroken tokens.
    fn text(&mut self, max: usize) -> String {
        const PIECES: &[&str] = &[
            "a",
            "portal",
            " ",
            "  ",
            "feat/very-long-branch-name",
            "日本語",
            "🚀",
            "é",
            "\n",
            "\r\n",
            "\t",
            "\x1b[31m",
            "\x1b[0m",
            "\x1b]0;title\x07",
            "\x07",
            "\x00",
            "\x1b[2J",
            "\x1b[H",
            "/work/acme/a/b/c/d/e/f/g",
            "https://acme.localhost/x?y=z",
            "\u{301}",
            "ｗｉｄｅ",
            "-",
            "@",
            ":",
        ];
        let count = 1 + self.below(max);
        let mut out = String::new();
        for _ in 0..count {
            if self.below(10) == 0 {
                out.push_str(&"x".repeat(1 + self.below(300)));
            } else {
                out.push_str(PIECES[self.below(PIECES.len())]);
            }
        }
        out
    }
}

/// No cell may hold a control character or an escape byte.
pub fn assert_clean(buffer: &Buffer, context: &str) {
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let symbol = buffer[(x, y)].symbol();
            assert!(
                !symbol.chars().any(char::is_control),
                "control character at ({x}, {y}) in {context}: {symbol:?}"
            );
        }
    }
}

/// Block corners and side borders of the list and the pane.
fn assert_borders(buffer: &Buffer, width: u16, height: u16, context: &str) {
    let Some(layout) = areas(width, height, false) else {
        return;
    };
    let list = layout.list.unwrap_or_default();
    for rect in [list, layout.pane] {
        let (left, right) = (rect.x, rect.right() - 1);
        let (top, bottom) = (rect.y, rect.bottom() - 1);
        assert_eq!(
            buffer[(left, top)].symbol(),
            "┌",
            "{context}: top-left of {rect:?}"
        );
        assert_eq!(
            buffer[(right, top)].symbol(),
            "┐",
            "{context}: top-right of {rect:?}"
        );
        assert_eq!(
            buffer[(left, bottom)].symbol(),
            "└",
            "{context}: bottom-left of {rect:?}"
        );
        assert_eq!(
            buffer[(right, bottom)].symbol(),
            "┘",
            "{context}: bottom-right of {rect:?}"
        );
        for y in top + 1..bottom {
            assert_eq!(
                buffer[(left, y)].symbol(),
                "│",
                "{context}: left border row {y}"
            );
            assert_eq!(
                buffer[(right, y)].symbol(),
                "│",
                "{context}: right border row {y}"
            );
        }
    }
}

#[tokio::test]
async fn random_long_text_never_breaks_borders() -> TestResult {
    let mut rng = Rng(0x5eed_cafe_f00d_d00d);
    let sizes = [(40, 8), (80, 24), (123, 31), (160, 45)];
    for iteration in 0..120 {
        let (width, height) = sizes[iteration % sizes.len()];
        let project = rng.text(4);
        let reference = format!("{project}@1");
        let mut units: Vec<UnitSummary> = (0..1 + rng.below(4))
            .map(|i| {
                let mut summary = unit(&reference, &format!("{}{i}", rng.text(4)), &rng.text(2));
                summary.urls = vec![rng.text(6)];
                summary.kind = if i == 0 { "task".into() } else { rng.text(1) };
                summary.reason = Some(RpcError::new(
                    rng.text(2),
                    json!({"message": rng.text(30)}),
                    1,
                ));
                summary
            })
            .collect();
        units[0].state = "idle".into();
        let mut lane_summary = lane(&project, Some(1), &rng.text(5), &rng.text(2), units);
        lane_summary.reference = reference.clone();
        lane_summary.path = rng.text(8).into();
        lane_summary.profile = Some(rng.text(3));
        lane_summary.domain = Some(rng.text(5));
        lane_summary.reason = Some(RpcError::new(
            "compose_failed",
            json!({"message": format!("`{}` failed ({}): {}", rng.text(10), rng.text(2), rng.text(40))}),
            1,
        ));
        let mut app = App::new(width, height);
        app.update(Msg::Status(Ok(vec![lane_summary])));
        let context = format!("iteration {iteration} at {width}x{height}");

        // The lane row with its details pane and a failed lane start.
        app.update(Msg::ActionDone {
            label: rng.text(6),
            target: Target::Lane(reference.clone()),
            result: Err(ApiError::Rpc(RpcError::new(
                rng.text(2),
                json!({"message": rng.text(40), "stderr": rng.text(20)}),
                1,
            ))),
        });
        let buffer = render(&app)?;
        assert_clean(&buffer, &context);
        assert_borders(&buffer, width, height, &context);

        // A unit row whose terminal received random bytes.
        app.update(key(KeyCode::Char('l')));
        app.update(key(KeyCode::Char('j')));
        if let Some(generation) = app.attached.as_ref().map(|a| a.generation) {
            app.update(Msg::AttachOpened {
                generation,
                result: Ok(AttachResult {
                    stream_id: 1,
                    unit: String::new(),
                    lane: String::new(),
                }),
            });
            app.update(Msg::AttachOutput {
                generation,
                bytes: rng.text(40).into_bytes(),
            });
            app.update(Msg::StreamError {
                generation,
                error: ApiError::Disconnected(rng.text(20)),
            });
        }
        app.update(Msg::Failed {
            what: rng.text(5),
            message: rng.text(30),
        });
        let buffer = render(&app)?;
        assert_clean(&buffer, &context);
        assert_borders(&buffer, width, height, &context);

        // The task prompt, the error list and the monitor with the same text.
        app.update(key(KeyCode::Char('t')));
        assert_clean(&render(&app)?, &context);
        app.update(key(KeyCode::Esc));
        app.update(key(KeyCode::Char('e')));
        let buffer = render(&app)?;
        assert_clean(&buffer, &context);
        let panel = error_panel_area(app.size);
        assert_eq!(
            buffer[(panel.x, panel.y)].symbol(),
            "┌",
            "{context}: error list"
        );
        assert_eq!(
            buffer[(panel.right() - 1, panel.bottom() - 1)].symbol(),
            "┘",
            "{context}"
        );
        app.update(key(KeyCode::Esc));
        app.tab = Tab::Monitor;
        assert_clean(&render(&app)?, &context);
    }
    Ok(())
}

// ---- full loop -------------------------------------------------------------------

#[tokio::test]
async fn drive_shows_a_failed_start_in_the_header_and_on_the_row() -> TestResult {
    let mut fake = FakeApi::with_lanes(lanes());
    fake.control_error = Some(compose_error());
    let api = Arc::new(fake);
    let (keys_tx, keys_rx) = mpsc::channel::<std::io::Result<TermEvent>>(8);
    let input = Box::pin(stream::unfold(keys_rx, |mut rx| async move {
        rx.recv().await.map(|event| (event, rx))
    }));
    let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
    let driver_api: Arc<dyn DaemonApi> = api.clone();
    let script = {
        let api = Arc::clone(&api);
        async move {
            for _ in 0..100 {
                if api.calls().iter().any(|c| c == "status") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
            let press = |c| {
                Ok(TermEvent::Key(KeyEvent::new(
                    KeyCode::Char(c),
                    KeyModifiers::NONE,
                )))
            };
            for c in ['j', 'j', 'l', 'j', 's'] {
                keys_tx.send(press(c)).await.ok();
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            tokio::time::sleep(Duration::from_millis(80)).await;
            keys_tx.send(press('q')).await.ok();
        }
    };
    let options = Options::default();
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
    assert!(
        api.calls()
            .iter()
            .any(|c| c.starts_with("Start acme@1 [\"acme@1:portal\"]")),
        "{:?}",
        api.calls()
    );
    let buffer = terminal.backend().buffer().clone();
    assert!(
        row(&buffer, 0).contains("! start acme@1:portal"),
        "{}",
        row(&buffer, 0)
    );
    assert_clean(&buffer, "drive");
    assert_borders(&buffer, 80, 24, "drive");
    Ok(())
}

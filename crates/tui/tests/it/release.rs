//! The header's release notice: a newer release with its upgrade command, or a
//! daemon that runs another version. `u` hides it for good. The welcome of
//! the first launch is kept with the same dismissals.

use crossterm::event::KeyCode;
use devlanes_client::update::InstallMethod;
use devlanes_tui::app::{Dialog, Effect, Msg};
use devlanes_tui::welcome::{DOCS_URL, KEY as WELCOME};
use serde_json::json;

use super::{TestResult, draw, key, loaded};
use crate::fake::{FakeApi, daemon_status, mixed_lanes};

#[tokio::test]
async fn the_welcome_shows_once_per_home() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 120, 40).await;
    assert!(app.dialog.is_none(), "not before the dismissals load");
    app.update(Msg::Notices {
        method: InstallMethod::Npm,
        dismissed: Vec::new(),
    });
    assert!(matches!(app.dialog, Some(Dialog::Welcome(_))));
    let screen = draw(&app)?.backend().to_string();
    assert!(screen.contains(DOCS_URL), "{screen}");
    assert_eq!(app.link_targets(), [DOCS_URL]);
    // `o` opens the docs and keeps the welcome; other keys do nothing.
    let effects = app.update(key(KeyCode::Char('o')));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::OpenUrl(url) if url == DOCS_URL)),
        "{effects:?}"
    );
    app.update(key(KeyCode::Char('j')));
    assert!(matches!(app.dialog, Some(Dialog::Welcome(_))));
    let effects = app.update(key(KeyCode::Enter));
    assert!(app.dialog.is_none());
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::DismissNotice(k) if k == WELCOME)),
        "{effects:?}"
    );
    // A later status does not bring it back.
    app.update(Msg::Status(Ok(mixed_lanes())));
    assert!(app.dialog.is_none());

    // Dismissed in this home before: it never opens.
    let (mut app, _) = loaded(&api, 120, 40).await;
    app.update(Msg::Notices {
        method: InstallMethod::Npm,
        dismissed: vec![WELCOME.into()],
    });
    assert!(app.dialog.is_none());
    Ok(())
}

/// `daemon.status` from a daemon of this version, with `update` when given.
fn status(version: &str, latest: Option<&str>) -> serde_json::Value {
    let mut status = daemon_status();
    status["version"] = json!(version);
    if let Some(latest) = latest {
        status["update"] = json!({"latest": latest, "checked_at": 1});
    }
    status
}

#[tokio::test]
async fn newer_release_shows_until_dismissed() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 140, 24).await;
    let own = devlanes_version::VERSION;
    app.update(Msg::DaemonInfo {
        status: Ok(status(own, Some("99.0.0"))),
        doctor: Ok(json!({})),
    });
    assert_eq!(
        app.release_badge(),
        None,
        "no command before the install method is known"
    );
    app.update(Msg::Notices {
        method: InstallMethod::Homebrew,
        dismissed: vec![WELCOME.into()],
    });
    let (notice, text) = app.release_badge().ok_or("no notice")?;
    assert_eq!(notice, "update:99.0.0");
    assert_eq!(
        text,
        "99.0.0 available: brew upgrade devlanes-run/tap/devlanes"
    );
    // The top bar marks the daemon button; the daemon page says what to run.
    let screen = draw(&app)?.backend().to_string();
    let top = screen.lines().next().unwrap_or_default();
    // The empty doctor reply has one problem by `devlanes doctor`'s rules:
    // the state schema is unknown.
    assert!(top.contains("D daemon · 1 issue, update"), "{top}");
    app.update(Msg::Key(crossterm::event::KeyEvent::new(
        KeyCode::Char('D'),
        crossterm::event::KeyModifiers::SHIFT,
    )));
    let page = draw(&app)?.backend().to_string();
    assert!(
        page.contains("warn update  99.0.0 available · u hides it"),
        "{page}"
    );
    assert!(
        page.contains("fix: brew upgrade devlanes-run/tap/devlanes"),
        "{page}"
    );

    let effects = app.update(key(KeyCode::Char('u')));
    assert!(
        effects
            .iter()
            .any(|e| matches!(e, Effect::DismissNotice(k) if k == "update:99.0.0")),
        "{effects:?}"
    );
    assert_eq!(app.release_badge(), None);

    // A dismissal loaded from disk hides the same release, not a newer one.
    app.update(Msg::Notices {
        method: InstallMethod::Homebrew,
        dismissed: vec![WELCOME.into(), "update:99.0.0".into()],
    });
    assert_eq!(app.release_badge(), None);
    app.update(Msg::DaemonInfo {
        status: Ok(status(own, Some("99.1.0"))),
        doctor: Ok(json!({})),
    });
    assert!(app.release_badge().is_some());
    Ok(())
}

#[tokio::test]
async fn older_or_equal_release_and_matching_daemon_show_nothing() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 140, 24).await;
    app.update(Msg::Notices {
        method: InstallMethod::Npm,
        dismissed: vec![WELCOME.into()],
    });
    app.update(Msg::DaemonInfo {
        status: Ok(status(devlanes_version::VERSION, Some("0.0.1"))),
        doctor: Ok(json!({})),
    });
    assert_eq!(app.release_badge(), None);
    // Without a notice, `u` does nothing.
    let effects = app.update(key(KeyCode::Char('u')));
    assert!(
        !effects
            .iter()
            .any(|e| matches!(e, Effect::DismissNotice(_))),
        "{effects:?}"
    );
    Ok(())
}

#[tokio::test]
async fn daemon_on_another_version_is_reported() -> TestResult {
    let api = FakeApi::with_lanes(mixed_lanes());
    let (mut app, _) = loaded(&api, 140, 24).await;
    app.update(Msg::DaemonInfo {
        status: Ok(status("0.0.9", None)),
        doctor: Ok(json!({})),
    });
    let (notice, text) = app.release_badge().ok_or("no notice")?;
    assert_eq!(notice, format!("skew:0.0.9:{}", devlanes_version::VERSION));
    assert!(text.starts_with("daemon 0.0.9, TUI"), "{text}");
    assert!(text.contains("(stops units)"), "{text}");
    Ok(())
}

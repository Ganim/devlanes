//! Who sizes a unit's PTY while several viewers are attached.

use std::time::Duration;

use devlanes_daemon::{Handler, Session};
use devlanes_proto::Method;
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

const CONFIG: &str = "project: viewers\nservices:\n  app: { run: sleep }\n";

/// The size of the last resize, as `<columns>x<rows>`.
fn last_size(h: &crate::Harness) -> Option<String> {
    h.runner
        .resizes
        .entries()
        .last()
        .and_then(|entry| entry.split_once(' ').map(|(_, size)| size.to_owned()))
}

/// Waits until the last resize is `size`.
async fn sized(h: &crate::Harness, size: &str) {
    let reached = eventually(Duration::from_secs(5), || async {
        (last_size(h).as_deref() == Some(size)).then_some(())
    })
    .await;
    assert!(
        reached.is_some(),
        "expected {size}, resizes {:?}",
        h.runner.resizes.entries()
    );
}

#[tokio::test(flavor = "multi_thread")]
/// When the active viewer leaves, the newest remaining interactive viewer takes
/// over with its own size; 120x40 returns only when no interactive viewer is
/// left, and sizes below 20x5 are raised to it.
async fn ownership_passes_to_the_newest_interactive_viewer() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let attach = |session: Session, interactive: bool, columns: u16, rows: u16| {
            let h = &h;
            let main = main.clone();
            async move {
                let value: Value = h
                    .call_on(
                        Method::Attach,
                        json!({"unit": "app", "interactive": interactive, "columns": columns, "rows": rows}),
                        cx(&main),
                        &session,
                    )
                    .await
                    .expect("attach");
                value["stream_id"].as_u64().expect("stream id")
            }
        };

        let (first, _first_frames) = h.session();
        let first_stream = attach(first.clone(), true, 100, 30).await;
        sized(&h, "100x30").await;
        let (second, _second_frames) = h.session();
        attach(second.clone(), true, 90, 25).await;
        sized(&h, "90x25").await;
        let (observer, _observer_frames) = h.session();
        attach(observer.clone(), false, 200, 60).await;

        // Not the owner: remembered, not applied.
        let resized = h
            .call_on(
                Method::Resize,
                json!({"stream_id": first_stream, "columns": 110, "rows": 35}),
                cx(&main),
                &first,
            )
            .await
            .expect("resize");
        assert_eq!(resized["applied"], false);
        assert_eq!(last_size(&h).as_deref(), Some("90x25"));

        second.close();
        sized(&h, "110x35").await;
        let resized = h
            .call_on(
                Method::Resize,
                json!({"stream_id": first_stream, "columns": 3, "rows": 1}),
                cx(&main),
                &first,
            )
            .await
            .expect("resize");
        assert_eq!(resized["applied"], true, "the first viewer owns it now");
        sized(&h, "20x5").await;

        first.close();
        sized(&h, "120x40").await;

        let (tiny, _tiny_frames) = h.session();
        attach(tiny.clone(), true, 4, 2).await;
        sized(&h, "20x5").await;
        tiny.close();
        observer.close();
        h.call(Method::Stop, json!({}), cx(&main)).await.expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

/// A unit whose output renders its terminal: two 50-column lines, then input
/// echoed back.
const RENDERED: &str = "project: viewers\nservices:\n  app: \
    { run: 'render; echo L01 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; \
    echo L02 xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; sleep' }\n";

/// Feeds every frame of `stream` into `screen` until `done` holds for it.
/// Returns whether any frame was not a redraw, which always starts with a
/// cursor move or a clear.
async fn watch(
    frames: &mut tokio::sync::mpsc::Receiver<devlanes_ipc::Frame>,
    stream: u64,
    screen: &mut vt100::Parser,
    done: impl Fn(&vt100::Screen) -> bool,
) -> bool {
    let mut raw = false;
    let watched = tokio::time::timeout(Duration::from_secs(5), async {
        while !done(screen.screen()) {
            match frames.recv().await {
                Some(devlanes_ipc::Frame::Bytes { stream_id, data }) if stream_id == stream => {
                    raw |= !data.starts_with(b"\x1b[");
                    screen.process(&data);
                }
                Some(_) => {}
                None => panic!("session closed"),
            }
        }
    })
    .await;
    assert!(
        watched.is_ok(),
        "screen never matched:\n{}",
        screen.screen().contents()
    );
    raw
}

#[tokio::test(flavor = "multi_thread")]
/// Viewers of a unit whose output renders its terminal get redraws of the
/// model at their own size, never its raw bytes; input still reaches it. With
/// no interactive viewer left, its terminal goes back to the wide headless
/// size instead of 120x40.
async fn rendered_units_are_redrawn_at_each_viewer_size() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", RENDERED);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let attach = |session: Session, interactive: bool, columns: u16, rows: u16| {
            let h = &h;
            let main = main.clone();
            async move {
                let value: Value = h
                    .call_on(
                        Method::Attach,
                        json!({"unit": "app", "interactive": interactive, "columns": columns, "rows": rows}),
                        cx(&main),
                        &session,
                    )
                    .await
                    .expect("attach");
                value["stream_id"].as_u64().expect("stream id")
            }
        };
        // An observer narrower than the unit's 300 columns.
        let (observer, mut frames) = h.session();
        let watching = attach(observer.clone(), false, 30, 10).await;
        let cells = |n| std::num::NonZeroU16::new(n).expect("nonzero");
        let mut screen = vt100::Parser::new(cells(10), cells(30), 0);
        // Each 50-column line wraps at the viewer's 30 columns.
        let wrapped = |screen: &vt100::Screen| {
            let rows: Vec<String> = screen.rows(0, 30).collect();
            rows[0].starts_with("L01") && rows[2].starts_with("L02")
        };
        let mut raw = watch(&mut frames, watching, &mut screen, wrapped).await;

        let (typist, _typist_frames) = h.session();
        let typing = attach(typist.clone(), true, 50, 12).await;
        sized(&h, "50x12").await;
        h.engine
            .input(typing, bytes::Bytes::from_static(b"typed"), typist.clone())
            .await
            .expect("input");
        raw |= watch(&mut frames, watching, &mut screen, |screen| {
            screen.contents().contains("typed")
        })
        .await;
        assert!(!raw, "a frame was not a redraw");
        typist.close();
        sized(&h, "300x40").await;
        observer.close();
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// A client reopening its stream at a new size (the TUI's zoom) closes the old
/// connection while the new one attaches. Whatever order the daemon sees them
/// in, the unit ends at the new size, never at the 120x40 of "nobody owns it".
async fn a_reopened_stream_keeps_its_new_size_whatever_the_order() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        let attach = |session: Session, columns: u16| {
            let h = &h;
            let main = main.clone();
            async move {
                h.call_on(
                    Method::Attach,
                    json!({"unit": "app", "interactive": true, "columns": columns, "rows": 30}),
                    cx(&main),
                    &session,
                )
                .await
                .expect("attach");
            }
        };
        let (mut old, _frames) = h.session();
        attach(old.clone(), 105).await;
        sized(&h, "105x30").await;
        for round in 0..20u16 {
            let columns = 150 + round;
            let (new, _new_frames) = h.session();
            let closing = old.clone();
            tokio::join!(async move { closing.close() }, attach(new.clone(), columns));
            sized(&h, &format!("{columns}x30")).await;
            // Nothing late overrides it.
            tokio::time::sleep(Duration::from_millis(20)).await;
            assert_eq!(
                last_size(&h),
                Some(format!("{columns}x30")),
                "round {round}"
            );
            old = new;
        }
        old.close();
        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        h.close().await;
    })
    .await
    .expect("timeout");
}

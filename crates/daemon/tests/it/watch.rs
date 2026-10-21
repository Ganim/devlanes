//! Subscriptions and `status --watch` start without gaps or side effects, and
//! `status --all` never registers a project.

use std::time::Duration;

use devlanes_daemon::Handler;
use devlanes_ipc::Frame;
use devlanes_proto::{Context, Control, Event, LaneSummary, Method, Page, Request, events};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{LIMIT, Setup, cx, harness, repo, temp};

const CONFIG: &str = "project: watched\nservices:\n  web: { run: sleep }\n";

/// Runs `method` on its own session in the background; returns its frames.
fn background(
    h: &crate::Harness,
    method: Method,
    params: Value,
    context: Context,
    id: u64,
) -> (devlanes_daemon::Session, mpsc::Receiver<Frame>) {
    let (session, frames) = h.session();
    let engine = h.engine.clone();
    let request = Request {
        id,
        method,
        params,
        context,
    };
    let running = session.clone();
    tokio::spawn(async move {
        let _ = engine.handle(request, running).await;
    });
    (session, frames)
}

/// The next event on `frames`, if one arrives within `limit`.
async fn next_event(frames: &mut mpsc::Receiver<Frame>, limit: Duration) -> Option<Event> {
    loop {
        match tokio::time::timeout(limit, frames.recv()).await {
            Ok(Some(Frame::Control(Control::Event(event)))) => return Some(event),
            Ok(Some(_)) => {}
            _ => return None,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
/// A new subscriber first gets a resync meant for it alone; a watcher gets its
/// snapshot first, and other subscribers do not see that snapshot.
async fn subscribers_start_with_their_own_resync_or_snapshot() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Status, json!({}), cx(&main))
            .await
            .expect("register");

        let (subscriber, mut events) = background(&h, Method::Subscribe, json!({}), cx(&main), 900);
        let first = next_event(&mut events, Duration::from_secs(2))
            .await
            .expect("resync");
        assert_eq!(first.event, events::RESYNC);
        assert_eq!(first.data["reason"], "subscribed");

        let (watcher, mut watched) =
            background(&h, Method::Status, json!({"watch": true}), cx(&main), 901);
        let snapshot = next_event(&mut watched, Duration::from_secs(2))
            .await
            .expect("snapshot");
        assert_eq!(snapshot.event, events::LANE_CHANGED);
        assert_eq!(snapshot.data["lane"]["reference"], "watched@0");
        assert!(
            next_event(&mut events, Duration::from_millis(300))
                .await
                .is_none(),
            "the watcher's snapshot is not broadcast"
        );

        // Changes after subscribing reach both.
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");
        assert!(
            next_event(&mut events, Duration::from_secs(2))
                .await
                .is_some()
        );
        assert!(
            next_event(&mut watched, Duration::from_secs(2))
                .await
                .is_some()
        );
        subscriber.close();
        watcher.close();
        h.close().await;
    })
    .await
    .expect("timeout");
}

#[tokio::test(flavor = "multi_thread")]
/// `status --all` (what completion sends, cwd or not) lists known lanes only;
/// it never registers the project of its cwd.
async fn status_all_never_registers_a_project() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "r", CONFIG);
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        let lanes = |value: Value| {
            serde_json::from_value::<Page<LaneSummary>>(value)
                .expect("page")
                .items
                .len()
        };
        for context in [cx(&main), Context::default()] {
            let all = h
                .call(Method::Status, json!({"all": true}), context)
                .await
                .expect("status");
            assert_eq!(lanes(all), 0);
        }
        let bare = h
            .call(Method::Status, json!({}), Context::default())
            .await
            .expect("status");
        assert_eq!(lanes(bare), 0, "no cwd registers nothing");
        let store = devlanes_state::Store::open(tmp.path.join("home/state.db"))
            .await
            .expect("store");
        assert!(store.projects().await.expect("projects").is_empty());

        let here = h
            .call(Method::Status, json!({}), cx(&main))
            .await
            .expect("status");
        assert_eq!(
            lanes(here),
            1,
            "a plain status from the repo still registers it"
        );
        h.close().await;
    })
    .await
    .expect("timeout");
}

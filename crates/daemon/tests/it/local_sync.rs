//! Local `services.<name>.sync`: a running service's folder is mirrored to its
//! `to`, deletions included, and the mirroring stops with the unit.

use std::{path::Path, time::Duration};

use devlanes_proto::Method;
use serde_json::{Value, json};

use crate::{LIMIT, Setup, cx, eventually, harness, repo, temp};

/// True once `path` exists with `contents`, or is gone when `contents` is `None`.
/// Generous: a loaded macOS can take many seconds to register a file watch.
async fn becomes(path: &Path, contents: Option<&str>) -> bool {
    eventually(Duration::from_secs(40), || async {
        let found = std::fs::read_to_string(path).ok();
        (found.as_deref() == contents).then_some(())
    })
    .await
    .is_some()
}

#[tokio::test(flavor = "multi_thread")]
/// Files appear in `to`, edits and deletions follow, `sync.status` lists the
/// local copy, and stopping the unit stops the mirroring.
async fn service_sync_mirrors_while_the_unit_runs() {
    tokio::time::timeout(LIMIT, async {
        let tmp = temp();
        // `~` is the request's HOME, which `cx` keeps inside this test's directory.
        let dest = tmp.path.join("user-home").join("mirror");
        let config = "project: mirror\nservices:\n  ext:\n    run: sleep\n    ready: started\n    sync:\n      - from: out\n        to: ~/mirror\n        ignore: [\"*.map\"]\n";
        let main = repo(&tmp.path, "r", config);
        let out = main.join("out");
        std::fs::create_dir_all(&out).expect("source");
        std::fs::write(out.join("a.txt"), "one").expect("a");
        std::fs::write(out.join("a.map"), "ignored").expect("map");
        let h = harness(&tmp.path.join("home"), Setup::default()).await;
        h.call(Method::Start, json!({}), cx(&main))
            .await
            .expect("start");

        assert!(becomes(&dest.join("a.txt"), Some("one")).await, "initial copy");
        assert!(!dest.join("a.map").exists(), "ignored files stay behind");
        std::fs::write(out.join("b.txt"), "two").expect("b");
        assert!(becomes(&dest.join("b.txt"), Some("two")).await, "new file");
        std::fs::remove_file(out.join("a.txt")).expect("remove a");
        assert!(becomes(&dest.join("a.txt"), None).await, "deletion");

        let status = h
            .call(Method::SyncStatus, json!({}), cx(&main))
            .await
            .expect("sync status");
        let item = &status["items"][0];
        assert_eq!(item["kind"], "local", "{status}");
        assert_eq!(item["unit"], "ext");
        assert_eq!(item["local"]["to"], json!(dest));
        assert_eq!(item["local"]["status"]["state"], "watching", "{status}");

        h.call(Method::Stop, json!({}), cx(&main))
            .await
            .expect("stop");
        std::fs::write(out.join("c.txt"), "three").expect("c");
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(!dest.join("c.txt").exists(), "nothing syncs after the stop");
        let status = h
            .call(Method::SyncStatus, json!({}), cx(&main))
            .await
            .expect("sync status");
        assert_eq!(status["items"], Value::Array(Vec::new()), "{status}");
        h.close().await;
    })
    .await
    .expect("timeout");
}

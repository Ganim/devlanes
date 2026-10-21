//! Large remote syncs: the client's manifest travels in batches, so a reconnect
//! over 50,000 files sends only what changed and stays within time and memory
//! bounds. A child of `remote`, whose box and client harness it reuses.

use std::{
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};

use super::{BoxSide, ClientSide, ipv6_loopback};
use crate::{LIMIT, cx, repo, temp};
use devlanes_proto::{Context, Method};

const BIG: &str = r#"project: bigsync
services:
  web:
    run: tcp-echo
    sync:
      - from: big
"#;

/// Files in the synced folder.
const FILES: usize = 50_000;

/// Longest a reconnect over [`FILES`] unchanged files may take until the sync is
/// watching again (debug build, box and client in one process).
const RECONNECT_LIMIT: Duration = Duration::from_secs(60);

/// Most the test process's resident memory may grow during such a reconnect.
/// Holding file contents or several copies of the manifest would exceed it.
const RECONNECT_RSS_GROWTH: u64 = 96 * 1024 * 1024;

/// Writes the source tree with mtimes old enough to be trusted for skipping.
fn write_tree(root: &Path) {
    let settled = SystemTime::now() - Duration::from_secs(60);
    for i in 0..FILES {
        let path = root.join(format!("d{:03}/f{i:05}.txt", i % 500));
        if i < 500 {
            std::fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
        }
        std::fs::write(&path, format!("file {i}\n")).expect("write");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .and_then(|file| file.set_modified(settled))
            .expect("mtime");
    }
}

/// Resident set size of this process in bytes.
fn rss() -> u64 {
    let output = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .expect("ps");
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .unwrap_or(0)
        * 1024
}

/// The box's one sync status entry on the client.
async fn sync(client: &ClientSide) -> Value {
    let host = client.host().await;
    host["syncs"]
        .as_array()
        .and_then(|syncs| syncs.first())
        .cloned()
        .unwrap_or(Value::Null)
}

/// Waits until the sync watches again after a fresh subscription has compared
/// every file, sampling RSS meanwhile; returns the status and the peak RSS.
async fn resynced(client: &ClientSide, limit: Duration) -> (Value, u64) {
    let deadline = Instant::now() + limit;
    let mut peak = 0;
    loop {
        peak = peak.max(tokio::task::spawn_blocking(rss).await.unwrap_or(0));
        let status = sync(client).await;
        if status["state"] == "watching" && status["compared"] == FILES as u64 {
            return (status, peak);
        }
        assert!(
            Instant::now() < deadline,
            "no resync within {limit:?}: {status}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Drops the box link and waits until the client noticed (its sync stopped
/// watching), so the next status read is never the old subscription's.
async fn drop_link(side: &BoxSide, client: &ClientSide) {
    side.transport.refuse(false);
    side.transport.last_link().expect("link").sever();
    let deadline = Instant::now() + Duration::from_secs(10);
    while sync(client).await["state"] == "watching" {
        assert!(Instant::now() < deadline, "the sync never noticed the drop");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drops the box link and lets the client reconnect.
async fn reconnect(side: &BoxSide, client: &ClientSide) {
    drop_link(side, client).await;
    side.transport.accept();
}

#[tokio::test(flavor = "multi_thread")]
/// A 50,000-file destination: a reconnect rewrites nothing, a change in the middle
/// of the tree is the only file sent, a deletion propagates, and a link dropped
/// in the middle of the manifest recovers.
async fn large_sync_reconnects_send_only_differences() {
    if !ipv6_loopback().await {
        eprintln!("skipping large_sync_reconnects_send_only_differences: no IPv6 loopback");
        return;
    }
    tokio::time::timeout(LIMIT * 6, async {
        let tmp = temp();
        let main = repo(&tmp.path, "bigrepo", BIG);
        tokio::task::block_in_place(|| write_tree(&main.join("big")));
        let side = BoxSide::start(&tmp, "box", "26000-26499").await;
        // The box also syncs the folder locally, below its user's home.
        let mut context = cx(&main);
        if let Some(env) = context.env.as_mut() {
            env.insert(
                "HOME".into(),
                tmp.path.join("box-user").display().to_string(),
            );
        }
        side.h
            .call(Method::Start, json!({}), context)
            .await
            .expect("box start");
        let client = ClientSide::start(&tmp, "client", "26500-26999", &side).await;
        client
            .h
            .call(
                Method::Connect,
                json!({"host": "box", "background": true}),
                Context::default(),
            )
            .await
            .expect("connect");
        let out = client.h.home.join("sync/box/bigsync/bigsync@0/big");

        // First connection: a full copy.
        let first = Instant::now();
        loop {
            let status = sync(&client).await;
            if status["state"] == "watching" {
                assert_eq!(status["written"], FILES as u64, "{status}");
                eprintln!("initial copy of {FILES} files: {:?}", first.elapsed());
                break;
            }
            assert!(first.elapsed() < LIMIT * 3, "no initial copy: {status}");
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Reconnect: nothing rewritten, within the time and memory bounds.
        let baseline = tokio::task::spawn_blocking(rss).await.unwrap_or(0);
        let started = Instant::now();
        reconnect(&side, &client).await;
        let (status, peak) = resynced(&client, RECONNECT_LIMIT).await;
        let took = started.elapsed();
        assert_eq!(status["written"], 0, "unchanged files resent: {status}");
        assert_eq!(status["pruned"], 0, "{status}");
        let growth = peak.saturating_sub(baseline);
        eprintln!(
            "reconnect over {FILES} files: {took:?}, RSS {} MiB -> peak {} MiB",
            baseline >> 20,
            peak >> 20
        );
        assert!(
            growth < RECONNECT_RSS_GROWTH,
            "RSS grew by {} MiB during the reconnect",
            growth >> 20
        );

        // While disconnected, one file in the middle changes and one goes away.
        drop_link(&side, &client).await;
        let changed = format!("d250/f{:05}.txt", FILES / 2 + 250);
        let removed = format!("d499/f{:05}.txt", FILES - 1);
        let (changed, removed) = (changed.as_str(), removed.as_str());
        std::fs::write(main.join("big").join(changed), b"changed on the box\n").expect("change");
        std::fs::remove_file(main.join("big").join(removed)).expect("remove");
        side.transport.accept();
        let deadline = Instant::now() + RECONNECT_LIMIT;
        let status = loop {
            let status = sync(&client).await;
            if status["state"] == "watching" && status["compared"] == FILES as u64 {
                break status;
            }
            assert!(Instant::now() < deadline, "no resync: {status}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(
            status["written"], 1,
            "only the changed file is sent: {status}"
        );
        assert_eq!(status["pruned"], 1, "the deletion propagates: {status}");
        assert_eq!(
            std::fs::read(out.join(changed)).ok().as_deref(),
            Some(&b"changed on the box\n"[..])
        );
        assert!(!out.join(removed).exists());

        // A link dropped in the middle of the manifest: the next subscription
        // starts over and still sends nothing.
        reconnect(&side, &client).await;
        let deadline = Instant::now() + RECONNECT_LIMIT;
        loop {
            let status = sync(&client).await;
            let compared = status["compared"].as_u64().unwrap_or(0);
            if status["state"] == "comparing" && compared > 0 && compared < FILES as u64 - 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "never caught mid-manifest: {status}"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        side.transport.refuse(false);
        side.transport.last_link().expect("link").sever();
        let deadline = Instant::now() + Duration::from_secs(10);
        while sync(&client).await["state"] == "comparing" {
            assert!(Instant::now() < deadline, "the drop went unnoticed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        side.transport.accept();
        let deadline = Instant::now() + RECONNECT_LIMIT;
        let status = loop {
            let status = sync(&client).await;
            if status["state"] == "watching" && status["compared"] == FILES as u64 - 1 {
                break status;
            }
            assert!(Instant::now() < deadline, "no recovery: {status}");
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(status["written"], 0, "{status}");
        assert_eq!(status["pruned"], 0, "{status}");

        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}

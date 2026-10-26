//! Large remote syncs: the client's manifest travels in batches, so a reconnect
//! over 50,000 files sends only what changed and stays within a memory bound. A
//! child of `remote`, whose box and client harness it reuses.

use std::{
    net::Ipv4Addr,
    path::Path,
    time::{Duration, Instant, SystemTime},
};

use serde_json::{Value, json};

use super::{BoxSide, ClientSide, ipv6_loopback};
use crate::{cx, repo, temp};
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

/// Longest the client's sync status may stay the same while the test waits on
/// it. Each change is a file written, a manifest batch compared or a new state,
/// so a status unchanged this long means a stuck sync. One step publishes
/// nothing however long it takes, the box's pass over its tree once the whole
/// manifest is compared; [`Progress::check`] leaves it to [`WHOLE_LIMIT`].
const STALL_LIMIT: Duration = Duration::from_secs(180);

/// Bound on the whole test, a hang guard only: every wait inside fails on a
/// stall. It stays under the CI job's timeout, and this test's nextest
/// slow-timeout in `.config/nextest.toml` ends after it.
const WHOLE_LIMIT: Duration = Duration::from_secs(15 * 60);

/// Most the test process's resident memory may grow during such a reconnect.
/// Holding file contents or several copies of the manifest would exceed it.
const RECONNECT_RSS_GROWTH: u64 = 96 * 1024 * 1024;

/// First port of the box's pool windows. Below the default `ports.range`
/// (20000 up), the e2e slots (26000 up) and every OS's ephemeral ports, and
/// clear of the DNS server's 15353, so only other runs of this test share it.
const BOX_PORTS: u16 = 17_000;

/// The client's pool. It has no lanes and binds nothing in it (its mirror
/// listens on the box's port numbers on `[::1]`), so a fixed range is enough.
const CLIENT_PORTS: &str = "17500-17999";

/// Ports per window: the smallest lane block, of which the box's `web` binds the first.
const WINDOW: u16 = 10;

/// Windows of the box's pool, which ends where [`CLIENT_PORTS`] begins.
const WINDOWS: u16 = 50;

/// A window of the box's ports, locked for one run of this test.
struct PortWindow {
    /// The window as a `ports.range`.
    range: String,
    /// Held until the test ends; the OS releases it if the process dies.
    _lock: std::fs::File,
}

/// The directory of the window locks, private to the user as the e2e slots'
/// is, so another user's directory in a shared `/tmp` cannot block it.
fn lock_dir() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        let dir = Path::new("/tmp").join(format!(
            "devlanes-daemon-large-sync-ports-{}",
            rustix::process::geteuid().as_raw()
        ));
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .expect("port lock dir");
        dir
    }
    // The temporary directory is per user on Windows.
    #[cfg(windows)]
    {
        let dir = std::env::temp_dir().join("devlanes-daemon-large-sync-ports");
        std::fs::create_dir_all(&dir).expect("port lock dir");
        dir
    }
}

/// Locks the first window that no other run of this test holds and whose ports
/// all bind on `127.0.0.1` now. A fixed range failed when another checkout ran
/// this test at the same time: the box's `web` could not bind its port, so the
/// box listed no sync entry and the client waited for a copy that never began.
fn box_ports() -> PortWindow {
    let dir = lock_dir();
    for window in 0..WINDOWS {
        let start = BOX_PORTS + window * WINDOW;
        let end = start + WINDOW - 1;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(format!("{start}.lock")))
            .expect("port lock file");
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => continue,
            Err(std::fs::TryLockError::Error(error)) => panic!("port lock {start}: {error}"),
        }
        // Another program, or a range Windows reserved, can hold a port too.
        if (start..=end)
            .all(|port| std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok())
        {
            return PortWindow {
                range: format!("{start}-{end}"),
                _lock: lock,
            };
        }
    }
    panic!(
        "no free box port window in {BOX_PORTS}-{}",
        BOX_PORTS + WINDOWS * WINDOW - 1
    );
}

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

/// When the client's sync status last changed, so waits fail on a stall instead
/// of after a fixed time.
struct Progress {
    last: Value,
    since: Instant,
}

impl Progress {
    /// Starts from no status.
    fn new() -> Self {
        Self {
            last: Value::Null,
            since: Instant::now(),
        }
    }

    /// Notes `status`; fails, naming what the test is `waiting` for, once the
    /// status stayed the same for [`STALL_LIMIT`].
    fn check(&mut self, status: &Value, waiting: &str) {
        let mut seen = status.clone();
        // Refreshed on every publish, also when nothing else moved.
        if let Some(fields) = seen.as_object_mut() {
            fields.remove("updated_ms");
        }
        if seen != self.last {
            self.last = seen;
            self.since = Instant::now();
        }
        // With the whole manifest compared the box walks its tree and the
        // client publishes nothing until it is done, so a slow machine looks
        // stalled there. Batches are 2,000 files, so only the full tree
        // (`FILES`, or `FILES - 1` after the deletion) reaches these counts.
        let compared = status["compared"].as_u64().unwrap_or(0);
        if status["state"] == "comparing" && compared >= FILES as u64 - 1 {
            self.since = Instant::now();
            return;
        }
        assert!(
            self.since.elapsed() < STALL_LIMIT,
            "{waiting}: no progress for {STALL_LIMIT:?}: {status}"
        );
    }
}

/// Polls the sync status every `every` until `done` accepts it and returns it;
/// fails once the status stops changing (see [`Progress`]).
async fn until(
    client: &ClientSide,
    waiting: &str,
    every: Duration,
    done: impl Fn(&Value) -> bool,
) -> Value {
    let mut progress = Progress::new();
    loop {
        let status = sync(client).await;
        if done(&status) {
            return status;
        }
        progress.check(&status, waiting);
        tokio::time::sleep(every).await;
    }
}

/// Waits until the sync watches again after a fresh subscription has compared
/// every file, sampling RSS meanwhile; returns the status and the peak RSS.
async fn resynced(client: &ClientSide) -> (Value, u64) {
    let mut progress = Progress::new();
    let mut peak = 0;
    loop {
        peak = peak.max(tokio::task::spawn_blocking(rss).await.unwrap_or(0));
        let status = sync(client).await;
        if status["state"] == "watching" && status["compared"] == FILES as u64 {
            return (status, peak);
        }
        progress.check(&status, "no resync");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Drops the box link and waits until the client noticed (its sync stopped
/// watching), so the next status read is never the old subscription's.
async fn drop_link(side: &BoxSide, client: &ClientSide) {
    side.transport.refuse(false);
    side.transport.last_link().expect("link").sever();
    until(
        client,
        "the sync never noticed the drop",
        Duration::from_millis(20),
        |status| status["state"] != "watching",
    )
    .await;
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
    tokio::time::timeout(WHOLE_LIMIT, async {
        let tmp = temp();
        let main = repo(&tmp.path, "bigrepo", BIG);
        tokio::task::block_in_place(|| write_tree(&main.join("big")));
        let ports = tokio::task::block_in_place(box_ports);
        let side = BoxSide::start(&tmp, "box", &ports.range).await;
        // The box also syncs the folder locally, below its user's home.
        let mut context = cx(&main);
        if let Some(env) = context.env.as_mut() {
            env.insert(
                "HOME".into(),
                tmp.path.join("box-user").display().to_string(),
            );
        }
        let started = side
            .h
            .call(Method::Start, json!({}), context)
            .await
            .expect("box start");
        // The box lists sync entries only for lanes whose services run.
        assert!(
            started["failures"].as_array().is_some_and(Vec::is_empty),
            "box start: {started}"
        );
        let client = ClientSide::start(&tmp, "client", CLIENT_PORTS, &side).await;
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
        let status = until(
            &client,
            "no initial copy",
            Duration::from_millis(200),
            |status| status["state"] == "watching",
        )
        .await;
        assert_eq!(status["written"], FILES as u64, "{status}");
        eprintln!("initial copy of {FILES} files: {:?}", first.elapsed());

        // Reconnect: nothing rewritten, within the memory bound. Its duration is
        // only printed: a fixed bound would fail on a slow machine that still
        // makes progress, and `written` already shows nothing was resent.
        let baseline = tokio::task::spawn_blocking(rss).await.unwrap_or(0);
        let started = Instant::now();
        reconnect(&side, &client).await;
        let (status, peak) = resynced(&client).await;
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
        let status = until(&client, "no resync", Duration::from_millis(100), |status| {
            status["state"] == "watching" && status["compared"] == FILES as u64
        })
        .await;
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
        until(
            &client,
            "never caught mid-manifest",
            Duration::from_millis(5),
            |status| {
                // Past the window the sync settles, and only a stall report
                // would end the wait, minutes later and blaming the sync.
                assert!(
                    status["state"] != "watching",
                    "missed the manifest window: {status}"
                );
                let compared = status["compared"].as_u64().unwrap_or(0);
                status["state"] == "comparing" && compared > 0 && compared < FILES as u64 - 1
            },
        )
        .await;
        side.transport.refuse(false);
        side.transport.last_link().expect("link").sever();
        until(
            &client,
            "the drop went unnoticed",
            Duration::from_millis(20),
            |status| status["state"] != "comparing",
        )
        .await;
        side.transport.accept();
        let status = until(
            &client,
            "no recovery",
            Duration::from_millis(100),
            |status| status["state"] == "watching" && status["compared"] == FILES as u64 - 1,
        )
        .await;
        assert_eq!(status["written"], 0, "{status}");
        assert_eq!(status["pruned"], 0, "{status}");

        client.stop().await;
        side.stop().await;
    })
    .await
    .expect("timeout");
}

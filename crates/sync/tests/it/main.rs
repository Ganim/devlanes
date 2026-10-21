//! Syncer behavior in tempdirs with the real filesystem watcher.

use std::fs;
use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _, symlink};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use devlanes_sync::{
    LocalTarget, Manifest, RelPath, SyncError, SyncOptions, SyncState, SyncTarget, Syncer,
};

/// Generous because FSEvents and inotify latency varies on loaded CI machines.
const WAIT: Duration = Duration::from_secs(10);

/// Polls `check` until it holds or [`WAIT`] passes.
async fn eventually(what: &str, check: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !check() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("parent");
    }
    fs::write(path, contents).expect("write");
}

fn read(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).expect("metadata").permissions().mode() & 0o777
}

fn options(ignore: &[&str]) -> SyncOptions {
    SyncOptions {
        ignore: ignore.iter().map(|pattern| (*pattern).to_owned()).collect(),
        debounce: Duration::from_millis(30),
        ..SyncOptions::default()
    }
}

struct Dirs {
    _temp: tempfile::TempDir,
    src: std::path::PathBuf,
    dst: std::path::PathBuf,
}

fn dirs() -> Dirs {
    let temp = tempfile::tempdir().expect("tempdir");
    let src = temp.path().join("src");
    let dst = temp.path().join("dst");
    fs::create_dir_all(&src).expect("src");
    Dirs {
        _temp: temp,
        src,
        dst,
    }
}

fn syncer(dirs: &Dirs, ignore: &[&str]) -> Syncer {
    Syncer::new(
        &dirs.src,
        Arc::new(LocalTarget::new(&dirs.dst)),
        options(ignore),
    )
    .expect("syncer")
}

#[tokio::test]
async fn files_over_the_limit_are_reported_and_skipped() {
    let d = dirs();
    write(&d.src.join("small.txt"), "ok");
    write(&d.src.join("big.bin"), &"x".repeat(64));
    let limited = SyncOptions {
        max_file: Some(16),
        ..options(&[])
    };
    let status = Syncer::new(&d.src, Arc::new(LocalTarget::new(&d.dst)), limited)
        .expect("syncer")
        .sync_once()
        .await
        .expect("sync");
    assert_eq!(read(&d.dst.join("small.txt")).as_deref(), Some("ok"));
    assert!(!d.dst.join("big.bin").exists());
    assert_eq!(status.errors.len(), 1, "{:?}", status.errors);
    assert!(status.errors[0].message.contains("over the sync limit"));
}

#[tokio::test]
async fn initial_copy_keeps_modes_skips_symlinks_and_ignores() {
    let d = dirs();
    write(&d.src.join("index.html"), "<html>");
    write(&d.src.join("js/app.js"), "app");
    write(&d.src.join("js/app.js.map"), "map");
    write(&d.src.join("node_modules/x/index.js"), "dep");
    write(&d.src.join("bin/run.sh"), "#!/bin/sh");
    fs::set_permissions(d.src.join("bin/run.sh"), fs::Permissions::from_mode(0o750))
        .expect("chmod");
    symlink(d.src.join("index.html"), d.src.join("link.html")).expect("file symlink");
    symlink(d.src.join("js"), d.src.join("jslink")).expect("dir symlink");
    fs::create_dir_all(d.src.join("empty")).expect("empty dir");

    let handle = syncer(&d, &["*.map", "node_modules"])
        .start()
        .await
        .expect("start");
    let status = handle.status();
    assert_eq!(status.state, SyncState::Watching);
    assert_eq!((status.files, status.written, status.passes), (3, 3, 1));
    assert!(status.last_sync.is_some());
    assert!(status.errors.is_empty(), "{:?}", status.errors);

    assert_eq!(read(&d.dst.join("index.html")).as_deref(), Some("<html>"));
    assert_eq!(read(&d.dst.join("js/app.js")).as_deref(), Some("app"));
    assert_eq!(mode(&d.dst.join("bin/run.sh")), 0o750);
    for absent in [
        "js/app.js.map",
        "node_modules",
        "link.html",
        "jslink",
        "empty",
    ] {
        assert!(!d.dst.join(absent).exists(), "{absent} must not be synced");
    }
    handle.stop().await;
}

#[tokio::test]
async fn follows_create_modify_delete() {
    let d = dirs();
    write(&d.src.join("keep.txt"), "keep");
    let handle = syncer(&d, &[]).start().await.expect("start");

    write(&d.src.join("deep/new/file.txt"), "one");
    eventually("create", || {
        read(&d.dst.join("deep/new/file.txt")).as_deref() == Some("one")
    })
    .await;

    write(&d.src.join("deep/new/file.txt"), "two!");
    eventually("modify", || {
        read(&d.dst.join("deep/new/file.txt")).as_deref() == Some("two!")
    })
    .await;

    fs::remove_file(d.src.join("deep/new/file.txt")).expect("rm");
    eventually("delete with parent pruning", || {
        !d.dst.join("deep").exists()
    })
    .await;

    write(&d.src.join("dir/a.txt"), "a");
    write(&d.src.join("dir/sub/b.txt"), "b");
    eventually("dir create", || d.dst.join("dir/sub/b.txt").exists()).await;
    fs::remove_dir_all(d.src.join("dir")).expect("rm -r");
    eventually("dir delete", || !d.dst.join("dir").exists()).await;

    assert_eq!(read(&d.dst.join("keep.txt")).as_deref(), Some("keep"));
    let status = handle.status();
    assert_eq!(status.files, 1);
    assert!(status.removed >= 3);
    handle.stop().await;
}

#[tokio::test]
async fn rapid_edits_converge_on_the_last_write() {
    let d = dirs();
    let handle = syncer(&d, &[]).start().await.expect("start");
    let file = d.src.join("hot.txt");
    for i in 0..200 {
        // Same length every time, so only content (not size) tells versions apart.
        fs::write(&file, format!("v{i:04}")).expect("write");
    }
    eventually("last write", || {
        read(&d.dst.join("hot.txt")).as_deref() == Some("v0199")
    })
    .await;
    // Rename-over writes, as editors do.
    for i in 0..20 {
        let temp = d.src.join(format!(".hot.{i}.swp"));
        fs::write(&temp, format!("r{i:04}")).expect("temp");
        fs::rename(&temp, &file).expect("rename");
    }
    eventually("last rename", || {
        read(&d.dst.join("hot.txt")).as_deref() == Some("r0019")
    })
    .await;
    eventually("swap files gone", || {
        fs::read_dir(&d.dst)
            .map(|entries| entries.count() == 1)
            .unwrap_or(false)
    })
    .await;
    assert!(handle.status().written < 200, "edits were debounced");
    handle.stop().await;
}

#[tokio::test]
async fn ignored_and_symlinked_paths_created_later_stay_out() {
    let d = dirs();
    write(&d.src.join("real.txt"), "real");
    let handle = syncer(&d, &["*.map", "/cache"])
        .start()
        .await
        .expect("start");

    write(&d.src.join("a.js.map"), "map");
    write(&d.src.join("cache/blob"), "blob");
    write(&d.src.join("sub/cache/kept"), "kept");
    symlink(d.src.join("real.txt"), d.src.join("alias.txt")).expect("symlink");
    eventually("non-ignored file", || d.dst.join("sub/cache/kept").exists()).await;
    // A replacement of a synced file by a symlink removes it.
    fs::remove_file(d.src.join("real.txt")).expect("rm");
    symlink("/etc/hosts", d.src.join("real.txt")).expect("symlink over file");
    eventually("symlinked file removed", || {
        !d.dst.join("real.txt").exists()
    })
    .await;

    for absent in ["a.js.map", "cache", "alias.txt"] {
        assert!(!d.dst.join(absent).exists(), "{absent} must not be synced");
    }
    handle.stop().await;
}

#[tokio::test]
async fn chmod_alone_updates_the_mode() {
    let d = dirs();
    write(&d.src.join("tool"), "#!/bin/sh");
    fs::set_permissions(d.src.join("tool"), fs::Permissions::from_mode(0o644)).expect("chmod");
    let handle = syncer(&d, &[]).start().await.expect("start");
    assert_eq!(mode(&d.dst.join("tool")), 0o644);
    fs::set_permissions(d.src.join("tool"), fs::Permissions::from_mode(0o755)).expect("chmod");
    eventually("mode change", || mode(&d.dst.join("tool")) == 0o755).await;
    handle.stop().await;
}

#[tokio::test]
async fn initial_copy_reuses_matching_files_and_prunes_stale_ones() {
    let d = dirs();
    write(&d.src.join("same.txt"), "same");
    write(&d.src.join("changed.txt"), "new");
    write(&d.dst.join("same.txt"), "same");
    write(&d.dst.join("changed.txt"), "old");
    write(&d.dst.join("stale.txt"), "stale");
    write(&d.dst.join("local.map"), "user file under an ignored name");
    let inode = fs::metadata(d.dst.join("same.txt")).expect("meta").ino();

    let status = syncer(&d, &["*.map"]).sync_once().await.expect("sync");
    assert_eq!((status.written, status.removed, status.files), (1, 1, 2));
    assert_eq!(
        fs::metadata(d.dst.join("same.txt")).expect("meta").ino(),
        inode,
        "identical file not rewritten"
    );
    assert_eq!(read(&d.dst.join("changed.txt")).as_deref(), Some("new"));
    assert!(!d.dst.join("stale.txt").exists());
    assert!(
        d.dst.join("local.map").exists(),
        "ignored target files are left alone"
    );
}

#[tokio::test]
async fn offered_held_files_replace_the_target_manifest() {
    let d = dirs();
    write(&d.src.join("same.txt"), "same");
    write(&d.src.join("dir/changed.txt"), "new");
    write(&d.src.join("added.txt"), "added");
    write(&d.dst.join("same.txt"), "same");
    write(&d.dst.join("dir/changed.txt"), "old");
    write(&d.dst.join("stale.txt"), "stale");
    write(&d.dst.join("dir/also-stale/x.txt"), "stale");
    write(&d.dst.join("local.map"), "user file under an ignored name");
    let inode = fs::metadata(d.dst.join("same.txt")).expect("meta").ino();
    // The source mtimes must be settled, or the pass rehashes (correctly, but
    // then this test would not show that matches are skipped without reading).
    let settled = std::time::SystemTime::now() - Duration::from_secs(60);
    for name in ["same.txt", "dir/changed.txt", "added.txt"] {
        fs::File::options()
            .write(true)
            .open(d.src.join(name))
            .and_then(|file| file.set_modified(settled))
            .expect("mtime");
    }

    // A remote client walks its destination in small batches.
    let mut syncer = syncer(&d, &["*.map"]);
    let mut walk = LocalTarget::new(&d.dst).held(Default::default(), 2);
    let mut batches = 0;
    while let Some(batch) = walk.next().await {
        assert!(batch.len() <= 2);
        batches += 1;
        syncer.offer_held(batch).await.expect("offer");
    }
    assert_eq!(batches, 3);
    let status = syncer.sync_once().await.expect("sync");
    assert_eq!((status.written, status.removed), (2, 2), "{status:?}");
    assert_eq!(
        fs::metadata(d.dst.join("same.txt")).expect("meta").ino(),
        inode,
        "identical file not rewritten"
    );
    assert_eq!(read(&d.dst.join("dir/changed.txt")).as_deref(), Some("new"));
    assert_eq!(read(&d.dst.join("added.txt")).as_deref(), Some("added"));
    assert!(!d.dst.join("stale.txt").exists());
    assert!(!d.dst.join("dir/also-stale").exists());
    assert!(
        d.dst.join("local.map").exists(),
        "ignored target files are left alone"
    );
}

#[tokio::test]
async fn waits_for_a_late_source_and_follows_replacement() {
    let temp = tempfile::tempdir().expect("tempdir");
    let src = temp.path().join("out/chrome-mv3");
    let dst = temp.path().join("dst");
    let handle = Syncer::new(&src, Arc::new(LocalTarget::new(&dst)), options(&[]))
        .expect("syncer")
        .start()
        .await
        .expect("start");
    assert_eq!(handle.status().state, SyncState::WaitingForSource);

    write(&src.join("manifest.json"), "{}");
    eventually("late source", || dst.join("manifest.json").exists()).await;

    // Dev servers often wipe and recreate their output folder.
    fs::remove_dir_all(&src).expect("wipe");
    write(&src.join("background.js"), "bg");
    eventually("recreated source", || {
        dst.join("background.js").exists() && !dst.join("manifest.json").exists()
    })
    .await;
    write(&src.join("later.js"), "later");
    eventually("events after rewatch", || dst.join("later.js").exists()).await;
    handle.stop().await;
}

#[tokio::test]
async fn refuses_overlapping_directories_and_missing_one_shot_source() {
    let d = dirs();
    let inside = Syncer::new(
        &d.src,
        Arc::new(LocalTarget::new(d.src.join("mirror"))),
        options(&[]),
    )
    .expect("syncer");
    assert!(matches!(
        inside.start().await,
        Err(SyncError::Overlap { .. })
    ));
    let around = Syncer::new(
        d.src.join("sub"),
        Arc::new(LocalTarget::new(&d.src)),
        options(&[]),
    )
    .expect("syncer");
    fs::create_dir_all(d.src.join("sub")).expect("sub");
    assert!(matches!(
        around.sync_once().await,
        Err(SyncError::Overlap { .. })
    ));

    let missing = Syncer::new(
        d.src.join("nope"),
        Arc::new(LocalTarget::new(&d.dst)),
        options(&[]),
    )
    .expect("syncer");
    assert!(matches!(
        missing.sync_once().await,
        Err(SyncError::SourceMissing(_))
    ));
    assert!(matches!(
        Syncer::new(
            &d.src,
            Arc::new(LocalTarget::new(&d.dst)),
            options(&["a[b"])
        ),
        Err(SyncError::InvalidIgnore { .. })
    ));
}

/// Local target that fails writes to one path.
struct FlakyTarget {
    inner: LocalTarget,
    broken: RelPath,
    attempts: Mutex<u32>,
}

#[async_trait]
impl SyncTarget for FlakyTarget {
    async fn manifest(&self) -> Result<Manifest, SyncError> {
        self.inner.manifest().await
    }

    async fn write_file(
        &self,
        path: &RelPath,
        contents: Bytes,
        mode: u32,
    ) -> Result<(), SyncError> {
        if *path == self.broken {
            *self.attempts.lock().expect("lock") += 1;
            return Err(SyncError::Target("disk full".into()));
        }
        self.inner.write_file(path, contents, mode).await
    }

    async fn remove(&self, path: &RelPath) -> Result<(), SyncError> {
        self.inner.remove(path).await
    }
}

#[tokio::test]
async fn status_reports_errors_and_retries_on_the_next_change() {
    let d = dirs();
    write(&d.src.join("ok.txt"), "ok");
    write(&d.src.join("bad.txt"), "bad");
    let target = Arc::new(FlakyTarget {
        inner: LocalTarget::new(&d.dst),
        broken: RelPath::new("bad.txt").expect("path"),
        attempts: Mutex::new(0),
    });
    let handle = Syncer::new(&d.src, target.clone(), options(&[]))
        .expect("syncer")
        .start()
        .await
        .expect("start");
    let status = handle.status();
    assert_eq!(status.files, 1);
    assert_eq!(status.error_count, 1);
    assert_eq!(status.errors[0].path.as_deref(), Some("bad.txt"));
    assert!(status.errors[0].message.contains("disk full"));

    write(&d.src.join("bad.txt"), "still bad");
    eventually("retry", || *target.attempts.lock().expect("lock") >= 2).await;
    let mut updates = handle.subscribe();
    tokio::time::timeout(WAIT, updates.wait_for(|status| status.error_count >= 2))
        .await
        .expect("status update in time")
        .expect("status channel open");
    handle.stop().await;
}

#[tokio::test]
async fn stop_ends_the_task_and_reports_stopped() {
    let d = dirs();
    let handle = syncer(&d, &[]).start().await.expect("start");
    let updates = handle.subscribe();
    tokio::time::timeout(WAIT, handle.stop())
        .await
        .expect("stops in time");
    assert_eq!(updates.borrow().state, SyncState::Stopped);
}

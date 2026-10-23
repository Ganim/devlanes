# devlanes-sync

One-way folder sync for `services.<name>.sync`. The daemon starts a `Syncer` into a `LocalTarget` for each entry once a service unit has spawned, and stops it when that run ends. It copies the source folder to a `SyncTarget`, then follows filesystem events. `devlanes-tunnel` uses the same crate to stream a box's entries to a connected client.

## Usage

```rust
use std::path::Path;
use std::sync::Arc;

use devlanes_sync::{LocalTarget, SyncError, SyncHandle, SyncOptions, Syncer};

async fn sync_extension(worktree: &Path, to: &Path) -> Result<SyncHandle, SyncError> {
    let options = SyncOptions {
        ignore: vec!["*.map".into()],
        ..SyncOptions::default()
    };
    let source = worktree.join("apps/extension/.output/chrome-mv3");
    let handle = Syncer::new(source, Arc::new(LocalTarget::new(to)), options)?
        .start()
        .await?;
    println!("{} files mirrored", handle.status().files);
    Ok(handle)
}
```

## API

- `Syncer`: `start` finishes the first copy before it returns when the source exists, then watches in a background task. `sync_once` runs a single pass.
- `SyncHandle`: `status`, `subscribe` and `stop`. Dropping it stops the sync.
- `SyncStatus`: state, last sync time, file and byte counts, and recent errors, for `devlanes remote sync`.
- `SyncTarget`: `manifest`, `write_file` and `remove`. `LocalTarget` implements it here; `devlanes-tunnel` implements it over the SSH mux for connected boxes.
- `Syncer::offer_held`: for a remote target, takes the target's file list one batch at a time before `start`, in place of `manifest`.
- `LocalTarget::held`: walks a destination and yields `HeldFile` batches, reusing a `HashCache` while size, mode and mtime match.
- `LocalTarget`: writes a temp file and renames it into place. It never writes or deletes through a symlinked directory.
- `IgnoreRules`: a pattern without `/` matches at any depth, and a pattern with `/` is anchored at the source root.

## Config example

```yaml
services:
  extension:
    run_script:
      file: apps/extension/package.json
      name: dev
    # Folders copied to a stable path, locally or on a machine connected with devlanes remote connect.
    sync:
      - from: apps/extension/.output/chrome-mv3
        # Destination; defaults to ~/devlanes/sync/<project>/<lane>/<folder>.
        # to: ~/extensions/acme
        ignore: ["*.map"]
```

While the service runs, the build output is mirrored to a path that stays the same across rebuilds, so a browser can keep it loaded unpacked. A relative `to` is resolved against the lane's worktree.

## Behaviour

The first pass deletes destination files that are missing from the source. Files matching `ignore` are left alone on both sides.

With `offer_held`, each batch is compared with the source when it arrives: files whose size, mode and hash match become entries of the syncer's index, which covers the whole source after the first pass anyway, and files the source lacks stay in it until the first pass deletes them. Nothing else of the target's list is kept, and the first pass then reads no file it already knows matches.

Changes are debounced and detected by size and mtime, then confirmed by content hash. Permission bits are copied. Symlinks, special files and empty directories are skipped.

A source that does not exist yet is polled until it appears. If the source folder is deleted and recreated, the sync mirrors the new folder and watches it.

## License

Apache-2.0

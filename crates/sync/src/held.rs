//! What a remote destination already holds, walked in batches so neither side
//! keeps the whole list: the client produces [`HeldFile`] batches while walking
//! its destination, and the box compares each batch with its source through
//! [`crate::Syncer::offer_held`].

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::target::{CachedHash, MODE_MASK, OWNER_MARKER, TEMP_PREFIX};
use crate::{ContentHash, HashCache, LocalTarget, RelPath};

/// Estimated bytes per entry besides its path, for the batch byte budget.
const ENTRY_OVERHEAD: usize = 160;

/// Bytes of paths and metadata per batch, so a batch of long paths stays far
/// below the transport's 8 MiB frame limit.
const BATCH_BYTES: usize = 1024 * 1024;

/// Batches the walker may finish before the reader takes them.
const AHEAD: usize = 2;

/// One file a destination holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeldFile {
    /// Path below the destination.
    pub path: RelPath,
    /// Size in bytes.
    pub size: u64,
    /// Permission bits.
    pub mode: u32,
    /// Content hash; `None` when the file could not be read.
    pub hash: Option<ContentHash>,
    /// Modification time on the destination.
    pub mtime: Option<SystemTime>,
}

/// A destination walk in progress; see [`LocalTarget::held`].
pub struct HeldWalk {
    batches: mpsc::Receiver<Vec<HeldFile>>,
    done: JoinHandle<HashCache>,
}

impl HeldWalk {
    /// The next batch; `None` once every file was listed.
    pub async fn next(&mut self) -> Option<Vec<HeldFile>> {
        self.batches.recv().await
    }

    /// Stops the walk (if it is not done) and returns the updated hash cache.
    ///
    /// Files the walk did not reach keep their old cache entries.
    pub async fn finish(self) -> HashCache {
        drop(self.batches);
        self.done.await.unwrap_or_default()
    }
}

impl LocalTarget {
    /// Lists the files held, in batches of at most `batch` entries (and about
    /// 1 MiB). Hashes come from `cache` while size, mode and mtime match, and are
    /// computed otherwise; the walk runs at most a couple of batches ahead of the
    /// reader, so memory beyond the cache stays bounded.
    pub fn held(&self, cache: HashCache, batch: usize) -> HeldWalk {
        let (sender, batches) = mpsc::channel(AHEAD);
        let root = self.root().to_path_buf();
        let batch = batch.max(1);
        let done = tokio::task::spawn_blocking(move || walk_held(&root, cache, batch, &sender));
        HeldWalk { batches, done }
    }
}

/// Blocking walk behind [`LocalTarget::held`]; returns the new cache.
fn walk_held(
    root: &Path,
    mut old: HashCache,
    batch: usize,
    sender: &mpsc::Sender<Vec<HeldFile>>,
) -> HashCache {
    let mut next = HashCache::default();
    if !root.exists() {
        return next;
    }
    let marker = root.join(OWNER_MARKER);
    let mut pending: Vec<HeldFile> = Vec::new();
    let mut bytes = 0;
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        // An unreadable entry is left out; the box then resends or deletes it.
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file()
            || entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
            || entry.path() == marker
        {
            continue;
        }
        let Some(rel) = entry
            .path()
            .strip_prefix(root)
            .ok()
            .and_then(|relative| RelPath::from_path(relative).ok())
        else {
            continue;
        };
        let Ok(meta) = entry.metadata() else { continue };
        let mtime = meta.modified().ok();
        let mode = meta.permissions().mode() & MODE_MASK;
        let cached = old.entries.remove(&rel).filter(|known| {
            known.size == meta.len() && known.mode == mode && mtime == Some(known.mtime)
        });
        let hash = match cached {
            Some(known) => Some(known.hash),
            None => fs::File::open(entry.path())
                .and_then(|mut file| ContentHash::of_reader(&mut file))
                .ok(),
        };
        if let Some(hash) = hash {
            next.entries.insert(
                rel.clone(),
                CachedHash {
                    size: meta.len(),
                    mode,
                    mtime: mtime.unwrap_or(SystemTime::UNIX_EPOCH),
                    hash,
                },
            );
        }
        bytes += rel.as_str().len() + ENTRY_OVERHEAD;
        pending.push(HeldFile {
            path: rel,
            size: meta.len(),
            mode,
            hash,
            mtime,
        });
        if pending.len() >= batch || bytes >= BATCH_BYTES {
            bytes = 0;
            if sender.blocking_send(std::mem::take(&mut pending)).is_err() {
                // The reader stopped; unvisited files keep their cached hashes.
                next.entries.append(&mut old.entries);
                return next;
            }
        }
    }
    if !pending.is_empty() {
        let _ = sender.blocking_send(pending);
    }
    next
}

/// How a held file compares with the source.
pub(crate) enum Probe {
    /// Same contents and mode; the settled mtime is known when not racy.
    Matches {
        size: u64,
        mtime: Option<SystemTime>,
        hash: ContentHash,
    },
    /// The source has a different regular file there (or it could not be read).
    Differs,
    /// The source has no regular file there.
    Absent,
}

/// Compares `held` with the source file at the same path below `root`, reading
/// at most `max_file` bytes. Blocking.
pub(crate) fn probe_source(
    root: &Path,
    held: &HeldFile,
    max_file: Option<u64>,
    settled: impl Fn(&fs::Metadata) -> Option<SystemTime>,
) -> Probe {
    // A symlinked or missing parent means the path is not in the source tree.
    let mut dir: PathBuf = root.to_path_buf();
    for name in held.path.parent_names() {
        dir.push(name);
        if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
            return Probe::Absent;
        }
    }
    let path = held.path.to_path(root);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Probe::Absent,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Probe::Absent,
        Err(_) => return Probe::Differs,
    }
    let Some(expected) = held.hash else {
        return Probe::Differs;
    };
    let Ok(mut file) = fs::File::open(&path) else {
        return Probe::Differs;
    };
    let Ok(meta) = file.metadata() else {
        return Probe::Differs;
    };
    let mode = meta.permissions().mode() & MODE_MASK;
    if meta.len() != held.size || mode != held.mode || max_file.is_some_and(|max| meta.len() > max)
    {
        return Probe::Differs;
    }
    let mut capped = std::io::Read::take(&mut file, meta.len().saturating_add(1));
    match ContentHash::of_reader(&mut capped) {
        Ok(hash) if hash == expected => Probe::Matches {
            size: meta.len(),
            mtime: settled(&meta),
            hash,
        },
        _ => Probe::Differs,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn walks_in_bounded_batches_and_reuses_the_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = LocalTarget::owned(dir.path().join("out"))
            .await
            .expect("claim");
        for i in 0..25 {
            let path = dir.path().join(format!("out/d{}/f{i}.txt", i % 3));
            fs::create_dir_all(path.parent().expect("parent")).expect("dirs");
            fs::write(path, format!("file {i}")).expect("write");
        }
        let mut walk = target.held(HashCache::default(), 10);
        let mut sizes = Vec::new();
        while let Some(batch) = walk.next().await {
            assert!(batch.iter().all(|file| file.hash.is_some()));
            sizes.push(batch.len());
        }
        assert_eq!(sizes, [10, 10, 5]);
        let cache = walk.finish().await;
        assert_eq!(cache.manifest().len(), 25, "marker and temp files left out");

        // Stopping after one batch keeps the cache entries of the unvisited files.
        let mut walk = target.held(cache, 10);
        assert_eq!(walk.next().await.map(|batch| batch.len()), Some(10));
        let cache = walk.finish().await;
        assert_eq!(cache.manifest().len(), 25);
    }
}

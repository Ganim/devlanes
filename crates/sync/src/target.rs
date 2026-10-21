use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{RelPath, SyncError};

/// Prefix of in-flight temp files; manifests skip them so a crash never syncs one.
pub const TEMP_PREFIX: &str = ".devlanes-sync-";

/// Only permission bits travel; setuid, setgid and sticky are dropped on purpose.
pub const MODE_MASK: u32 = 0o777;

/// SHA-256 of file contents.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// Hashes `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }

    /// Hashes a reader without buffering it whole.
    pub fn of_reader(reader: &mut impl io::Read) -> io::Result<Self> {
        let mut hasher = Sha256::new();
        io::copy(reader, &mut hasher)?;
        Ok(Self(hasher.finalize().into()))
    }

    /// Lowercase hex, as sent in remote sync manifests.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    /// Parses lowercase or uppercase hex; `None` unless it is exactly 32 bytes.
    pub fn from_hex(text: &str) -> Option<Self> {
        let bytes = hex::decode(text).ok()?;
        Some(Self(bytes.try_into().ok()?))
    }

    /// A value no file hashes to in practice, for contents nobody could read.
    pub(crate) fn unknown() -> Self {
        Self([0; 32])
    }
}

impl fmt::Debug for ContentHash {
    /// Lowercase hex.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

/// What a target holds at one path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// Size in bytes.
    pub size: u64,
    /// Permission bits ([`MODE_MASK`]).
    pub mode: u32,
    /// Content hash.
    pub hash: ContentHash,
}

/// Every regular file a target holds, by relative path.
pub type Manifest = BTreeMap<RelPath, ManifestEntry>;

/// Where a [`crate::Syncer`] writes.
///
/// Implemented by [`LocalTarget`] here and by the remote mux target later; the
/// syncer only ever calls these methods, in order, from one task.
#[async_trait]
pub trait SyncTarget: Send + Sync {
    /// Lists current files with hashes; read once before the first full copy so
    /// unchanged files are not rewritten.
    async fn manifest(&self) -> Result<Manifest, SyncError>;

    /// Replaces `path` atomically with `contents` and permission bits `mode`,
    /// creating parent directories.
    async fn write_file(&self, path: &RelPath, contents: Bytes, mode: u32)
    -> Result<(), SyncError>;

    /// Removes the file at `path` and any parent directories left empty; absent is success.
    async fn remove(&self, path: &RelPath) -> Result<(), SyncError>;

    /// The destination directory when it is on this machine's filesystem.
    ///
    /// The syncer uses it to refuse a destination inside the source, which would
    /// copy its own output forever.
    fn local_root(&self) -> Option<&Path> {
        None
    }
}

/// File that marks a directory as created by devlanes sync. Only such a directory
/// may lose whole subtrees (a source directory that became a file) or be pruned.
pub const OWNER_MARKER: &str = ".devlanes-sync";

/// A directory on the local filesystem.
#[derive(Debug, Clone)]
pub struct LocalTarget {
    root: PathBuf,
    owned: bool,
}

impl LocalTarget {
    /// Writes below `root`, created on first write. Never removes a non-empty
    /// directory that is in the way of a file; use [`LocalTarget::owned`] for that.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            owned: false,
        }
    }

    /// A destination devlanes owns: created with an [`OWNER_MARKER`] when absent,
    /// adopted when empty, refused when it holds files but no marker.
    pub async fn owned(root: impl Into<PathBuf>) -> Result<Self, SyncError> {
        let root = root.into();
        let claimed = root.clone();
        blocking(format!("claiming {}", root.display()), move || {
            claim(&claimed)
        })
        .await?;
        Ok(Self { root, owned: true })
    }

    /// The destination directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Paths of the files held, without reading them.
    pub async fn files(&self) -> Result<Vec<RelPath>, SyncError> {
        let root = self.root.clone();
        blocking(format!("reading {}", self.root.display()), move || {
            let mut files = Vec::new();
            walk(&root, |rel, _| {
                files.push(rel);
                Ok(())
            })?;
            Ok(files)
        })
        .await
    }

    /// Like [`SyncTarget::manifest`], but files whose size, mtime and mode match
    /// `cache` keep their hash instead of being read again.
    pub async fn manifest_cached(&self, cache: HashCache) -> Result<HashCache, SyncError> {
        let root = self.root.clone();
        blocking(format!("reading {}", self.root.display()), move || {
            let mut next = HashCache::default();
            walk(&root, |rel, path| {
                let meta = fs::symlink_metadata(path)?;
                let mtime = meta.modified().ok();
                let mode = meta.permissions().mode() & MODE_MASK;
                let known = cache.entries.get(&rel).filter(|known| {
                    known.size == meta.len() && known.mode == mode && mtime == Some(known.mtime)
                });
                let entry = match known {
                    Some(known) => known.clone(),
                    None => CachedHash {
                        size: meta.len(),
                        mode,
                        mtime: mtime.unwrap_or(SystemTime::UNIX_EPOCH),
                        hash: ContentHash::of_reader(&mut fs::File::open(path)?)?,
                    },
                };
                next.entries.insert(rel, entry);
                Ok(())
            })?;
            Ok(next)
        })
        .await
    }

    /// Starts writing `path` in pieces; nothing is visible until [`PartialFile::commit`].
    pub async fn begin_file(&self, path: &RelPath, mode: u32) -> Result<PartialFile, SyncError> {
        let root = self.root.clone();
        let rel = path.clone();
        let dest = path.to_path(&self.root);
        let temp = blocking(format!("writing {}", dest.display()), move || {
            let parent = prepare_parents(&root, &rel)?;
            tempfile::Builder::new()
                .prefix(TEMP_PREFIX)
                .suffix(".tmp")
                .tempfile_in(parent)
        })
        .await?;
        Ok(PartialFile {
            temp: Some(temp),
            dest,
            mode,
            owned: self.owned,
            written: 0,
        })
    }
}

/// Hashes of a destination's files with the metadata they were computed for.
#[derive(Debug, Clone, Default)]
pub struct HashCache {
    pub(crate) entries: BTreeMap<RelPath, CachedHash>,
}

/// One cached hash.
#[derive(Debug, Clone)]
pub(crate) struct CachedHash {
    pub(crate) size: u64,
    pub(crate) mode: u32,
    pub(crate) mtime: SystemTime,
    pub(crate) hash: ContentHash,
}

impl HashCache {
    /// The files as a manifest.
    pub fn manifest(&self) -> Manifest {
        self.entries
            .iter()
            .map(|(rel, cached)| {
                (
                    rel.clone(),
                    ManifestEntry {
                        size: cached.size,
                        mode: cached.mode,
                        hash: cached.hash,
                    },
                )
            })
            .collect()
    }

    /// Modification time recorded for `path`.
    pub fn mtime(&self, path: &RelPath) -> Option<SystemTime> {
        self.entries.get(path).map(|cached| cached.mtime)
    }
}

/// A file being received piece by piece into a temp file next to its destination.
///
/// Dropping it without [`PartialFile::commit`] deletes the temp file.
pub struct PartialFile {
    temp: Option<tempfile::NamedTempFile>,
    dest: PathBuf,
    mode: u32,
    owned: bool,
    written: u64,
}

impl PartialFile {
    /// Bytes appended so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Appends `data`.
    pub async fn append(&mut self, data: Bytes) -> Result<(), SyncError> {
        let mut temp = self.temp.take().ok_or_else(|| SyncError::Io {
            context: format!("writing {}", self.dest.display()),
            source: io::Error::other("file already failed"),
        })?;
        let length = data.len() as u64;
        let (temp, result) = tokio::task::spawn_blocking(move || {
            let result = temp.write_all(&data);
            (temp, result)
        })
        .await
        .map_err(|join| SyncError::Io {
            context: format!("writing {}", self.dest.display()),
            source: io::Error::other(join),
        })?;
        result.map_err(|source| SyncError::Io {
            context: format!("writing {}", self.dest.display()),
            source,
        })?;
        self.temp = Some(temp);
        self.written += length;
        Ok(())
    }

    /// Sets the mode and renames the temp file over the destination.
    pub async fn commit(mut self) -> Result<(), SyncError> {
        let temp = self.temp.take().ok_or_else(|| SyncError::Io {
            context: format!("writing {}", self.dest.display()),
            source: io::Error::other("file already failed"),
        })?;
        let (dest, mode, owned) = (self.dest.clone(), self.mode, self.owned);
        blocking(format!("writing {}", self.dest.display()), move || {
            clear_directory(&dest, owned)?;
            temp.as_file()
                .set_permissions(Permissions::from_mode(mode & MODE_MASK))?;
            temp.persist(&dest).map_err(|error| error.error)?;
            Ok(())
        })
        .await
    }
}

/// Makes `root` a devlanes-owned destination, or explains why it cannot be one.
fn claim(root: &Path) -> io::Result<()> {
    match fs::symlink_metadata(root) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir_all(root)?;
            fs::write(root.join(OWNER_MARKER), b"")
        }
        Err(error) => Err(error),
        Ok(meta) if !meta.is_dir() => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("{} exists and is not a directory", root.display()),
        )),
        Ok(_) => {
            if fs::symlink_metadata(root.join(OWNER_MARKER)).is_ok_and(|meta| meta.is_file()) {
                return Ok(());
            }
            if fs::read_dir(root)?.next().is_none() {
                return fs::write(root.join(OWNER_MARKER), b"");
            }
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} holds files that devlanes sync did not create; move them away",
                    root.display()
                ),
            ))
        }
    }
}

/// Visits every regular file below `root` without following symlinks, skipping
/// in-flight temp files and the owner marker. A missing root has no files.
fn walk(root: &Path, mut visit: impl FnMut(RelPath, &Path) -> io::Result<()>) -> io::Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let marker = root.join(OWNER_MARKER);
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file()
            || entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
            || entry.path() == marker
        {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        // Names the source could never send (non-UTF-8) are left alone.
        let Ok(rel) = RelPath::from_path(relative) else {
            continue;
        };
        visit(rel, entry.path())?;
    }
    Ok(())
}

/// Removes a directory standing where a file must go: its whole tree only inside
/// an owned destination, else only when empty.
fn clear_directory(dest: &Path, owned: bool) -> io::Result<()> {
    if !fs::symlink_metadata(dest).is_ok_and(|meta| meta.is_dir()) {
        return Ok(());
    }
    if owned {
        fs::remove_dir_all(dest)
    } else {
        fs::remove_dir(dest)
    }
}

/// Runs blocking filesystem work off the async executor.
async fn blocking<T: Send + 'static>(
    context: String,
    work: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> Result<T, SyncError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result.map_err(|source| SyncError::Io { context, source }),
        Err(join) => Err(SyncError::Io {
            context,
            source: io::Error::other(join),
        }),
    }
}

#[async_trait]
impl SyncTarget for LocalTarget {
    /// Walks the root without following symlinks; a missing root is empty.
    async fn manifest(&self) -> Result<Manifest, SyncError> {
        let root = self.root.clone();
        blocking(format!("reading {}", self.root.display()), move || {
            local_manifest(&root)
        })
        .await
    }

    /// Writes a sibling temp file, sets its mode, then renames it over the target.
    async fn write_file(
        &self,
        path: &RelPath,
        contents: Bytes,
        mode: u32,
    ) -> Result<(), SyncError> {
        let root = self.root.clone();
        let rel = path.clone();
        let owned = self.owned;
        blocking(
            format!("writing {}", path.to_path(&self.root).display()),
            move || write_local(&root, &rel, &contents, mode, owned),
        )
        .await
    }

    /// Deletes the file and prunes empty parents up to the root.
    async fn remove(&self, path: &RelPath) -> Result<(), SyncError> {
        let root = self.root.clone();
        let rel = path.clone();
        blocking(
            format!("removing {}", path.to_path(&self.root).display()),
            move || remove_local(&root, &rel),
        )
        .await
    }

    /// Always the root.
    fn local_root(&self) -> Option<&Path> {
        Some(&self.root)
    }
}

/// Blocking manifest walk.
fn local_manifest(root: &Path) -> io::Result<Manifest> {
    let mut manifest = Manifest::new();
    walk(root, |rel, path| {
        let meta = fs::symlink_metadata(path)?;
        let hash = ContentHash::of_reader(&mut fs::File::open(path)?)?;
        manifest.insert(
            rel,
            ManifestEntry {
                size: meta.len(),
                mode: meta.permissions().mode() & MODE_MASK,
                hash,
            },
        );
        Ok(())
    })?;
    Ok(manifest)
}

/// Makes every directory from `root` to `rel`'s parent a real directory.
///
/// A file or symlink in the way is replaced: the source now has a directory there,
/// and following a symlink could write outside the root.
fn prepare_parents(root: &Path, rel: &RelPath) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    let mut dir = root.to_path_buf();
    for name in rel.parent_names() {
        dir.push(name);
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => {
                fs::remove_file(&dir)?;
                fs::create_dir(&dir)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&dir)?,
            Err(error) => return Err(error),
        }
    }
    Ok(dir)
}

/// Blocking atomic write.
fn write_local(
    root: &Path,
    rel: &RelPath,
    contents: &[u8],
    mode: u32,
    owned: bool,
) -> io::Result<()> {
    let parent = prepare_parents(root, rel)?;
    let dest = rel.to_path(root);
    // The source turned this directory into a file.
    clear_directory(&dest, owned)?;
    let mut temp = tempfile::Builder::new()
        .prefix(TEMP_PREFIX)
        .suffix(".tmp")
        .tempfile_in(&parent)?;
    temp.write_all(contents)?;
    temp.as_file()
        .set_permissions(Permissions::from_mode(mode & MODE_MASK))?;
    temp.persist(&dest).map_err(|error| error.error)?;
    Ok(())
}

/// Blocking removal with empty-parent pruning.
fn remove_local(root: &Path, rel: &RelPath) -> io::Result<()> {
    let mut dir = root.to_path_buf();
    let mut parents = Vec::new();
    for name in rel.parent_names() {
        dir.push(name);
        // Never remove through a symlinked directory: it would delete outside the root.
        match fs::symlink_metadata(&dir) {
            Ok(meta) if meta.is_dir() => parents.push(dir.clone()),
            _ => return Ok(()),
        }
    }
    let dest = rel.to_path(root);
    match fs::symlink_metadata(&dest) {
        Ok(meta) if !meta.is_dir() => match fs::remove_file(&dest) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        },
        // A directory here was not created by the syncer for this path; leave it.
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    for parent in parents.iter().rev() {
        // Non-empty (or already gone) ends pruning; both are fine.
        if fs::remove_dir(parent).is_err() {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> RelPath {
        RelPath::new(path).expect("path")
    }

    #[tokio::test]
    async fn writes_atomically_with_mode_and_prunes_on_remove() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = LocalTarget::new(dir.path().join("out"));
        target
            .write_file(
                &rel("a/b/run.sh"),
                Bytes::from_static(b"#!/bin/sh\n"),
                0o4755,
            )
            .await
            .expect("write");
        let path = dir.path().join("out/a/b/run.sh");
        assert_eq!(fs::read(&path).expect("read"), b"#!/bin/sh\n");
        assert_eq!(
            fs::metadata(&path).expect("meta").permissions().mode() & 0o7777,
            0o755,
            "setuid dropped"
        );
        let manifest = target.manifest().await.expect("manifest");
        assert_eq!(manifest.len(), 1);
        assert_eq!(
            manifest[&rel("a/b/run.sh")].hash,
            ContentHash::of(b"#!/bin/sh\n")
        );

        target.remove(&rel("a/b/run.sh")).await.expect("remove");
        assert!(!dir.path().join("out/a").exists(), "empty parents pruned");
        assert!(dir.path().join("out").exists(), "root kept");
        target
            .remove(&rel("a/b/run.sh"))
            .await
            .expect("absent is fine");
    }

    #[tokio::test]
    async fn never_follows_symlinked_directories() {
        let dir = tempfile::tempdir().expect("tempdir");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("keep.txt"), b"keep").expect("keep");
        let root = dir.path().join("out");
        fs::create_dir_all(&root).expect("root");
        std::os::unix::fs::symlink(&outside, root.join("link")).expect("symlink");
        let target = LocalTarget::new(&root);

        target.remove(&rel("link/keep.txt")).await.expect("remove");
        assert!(outside.join("keep.txt").exists(), "remove did not escape");
        assert!(target.manifest().await.expect("manifest").is_empty());

        target
            .write_file(&rel("link/new.txt"), Bytes::from_static(b"x"), 0o644)
            .await
            .expect("write");
        assert!(!outside.join("new.txt").exists(), "write did not escape");
        assert!(root.join("link").is_dir() && !root.join("link").is_symlink());
    }

    #[tokio::test]
    async fn only_owned_destinations_lose_directory_trees() {
        let dir = tempfile::tempdir().expect("tempdir");
        let foreign = dir.path().join("foreign");
        fs::create_dir_all(foreign.join("a")).expect("dir");
        fs::write(foreign.join("a/keep.txt"), b"keep").expect("keep");
        let claimed = LocalTarget::owned(&foreign).await;
        assert!(
            claimed.is_err(),
            "a non-empty directory without marker is refused"
        );
        let plain = LocalTarget::new(&foreign);
        let replaced = plain
            .write_file(&rel("a"), Bytes::from_static(b"file"), 0o644)
            .await;
        assert!(
            replaced.is_err(),
            "unowned targets never remove a directory tree"
        );
        assert!(foreign.join("a/keep.txt").exists());

        let owned = LocalTarget::owned(dir.path().join("mine"))
            .await
            .expect("claim");
        assert!(dir.path().join("mine").join(OWNER_MARKER).is_file());
        assert!(
            owned.manifest().await.expect("manifest").is_empty(),
            "marker is not a file of the sync"
        );
        // A second claim of the same destination succeeds.
        LocalTarget::owned(dir.path().join("mine"))
            .await
            .expect("reclaim");
    }

    #[tokio::test]
    async fn streamed_files_appear_only_on_commit() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = LocalTarget::owned(dir.path().join("out"))
            .await
            .expect("claim");
        let mut file = target
            .begin_file(&rel("x/big.bin"), 0o600)
            .await
            .expect("begin");
        file.append(Bytes::from_static(b"one "))
            .await
            .expect("append");
        assert!(!dir.path().join("out/x/big.bin").exists());
        file.append(Bytes::from_static(b"two"))
            .await
            .expect("append");
        assert_eq!(file.written(), 7);
        file.commit().await.expect("commit");
        assert_eq!(
            fs::read(dir.path().join("out/x/big.bin")).expect("read"),
            b"one two"
        );
        let abandoned = target
            .begin_file(&rel("x/gone.bin"), 0o600)
            .await
            .expect("begin");
        drop(abandoned);
        let files = target.files().await.expect("files");
        assert_eq!(files, vec![rel("x/big.bin")], "no temp file is left behind");
    }

    #[tokio::test]
    async fn cached_manifests_match_fresh_ones() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = LocalTarget::owned(dir.path().join("out"))
            .await
            .expect("claim");
        target
            .write_file(&rel("a.txt"), Bytes::from_static(b"a"), 0o644)
            .await
            .expect("write");
        let cache = target
            .manifest_cached(HashCache::default())
            .await
            .expect("cache");
        let again = target.manifest_cached(cache).await.expect("cache");
        assert_eq!(again.manifest(), target.manifest().await.expect("manifest"));
        assert!(again.mtime(&rel("a.txt")).is_some());
        assert_eq!(
            ContentHash::from_hex(&again.manifest()[&rel("a.txt")].hash.to_hex()),
            Some(ContentHash::of(b"a"))
        );
    }

    #[tokio::test]
    async fn replaces_files_and_directories_in_the_way() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = LocalTarget::owned(dir.path()).await.expect("claim");
        target
            .write_file(&rel("a"), Bytes::from_static(b"file"), 0o644)
            .await
            .expect("file");
        target
            .write_file(&rel("a/b"), Bytes::from_static(b"nested"), 0o644)
            .await
            .expect("dir");
        assert_eq!(fs::read(dir.path().join("a/b")).expect("read"), b"nested");
        target
            .write_file(&rel("a"), Bytes::from_static(b"file"), 0o600)
            .await
            .expect("back");
        assert_eq!(fs::read(dir.path().join("a")).expect("read"), b"file");
    }
}

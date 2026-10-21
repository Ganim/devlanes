use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, Permissions};
use std::io::{self, Write as _};
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

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

/// A directory on the local filesystem.
#[derive(Debug, Clone)]
pub struct LocalTarget {
    root: PathBuf,
}

impl LocalTarget {
    /// Writes below `root`, created on first write.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The destination directory.
    pub fn root(&self) -> &Path {
        &self.root
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
        blocking(
            format!("writing {}", path.to_path(&self.root).display()),
            move || write_local(&root, &rel, &contents, mode),
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
    if !root.exists() {
        return Ok(manifest);
    }
    for entry in walkdir::WalkDir::new(root).min_depth(1).follow_links(false) {
        let entry = entry.map_err(io::Error::other)?;
        if !entry.file_type().is_file()
            || entry.file_name().to_string_lossy().starts_with(TEMP_PREFIX)
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
        let meta = entry.metadata().map_err(io::Error::other)?;
        let hash = ContentHash::of_reader(&mut fs::File::open(entry.path())?)?;
        manifest.insert(
            rel,
            ManifestEntry {
                size: meta.len(),
                mode: meta.permissions().mode() & MODE_MASK,
                hash,
            },
        );
    }
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
fn write_local(root: &Path, rel: &RelPath, contents: &[u8], mode: u32) -> io::Result<()> {
    let parent = prepare_parents(root, rel)?;
    let dest = rel.to_path(root);
    if fs::symlink_metadata(&dest).is_ok_and(|meta| meta.is_dir()) {
        // The source turned this directory into a file.
        fs::remove_dir_all(&dest)?;
    }
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
    async fn replaces_files_and_directories_in_the_way() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = LocalTarget::new(dir.path());
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

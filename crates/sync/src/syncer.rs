use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use bytes::Bytes;
use devlanes_sys::FileIdentity;
use notify::event::{AccessKind, AccessMode, ModifyKind};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher as _};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::held::{Probe, probe_source};
use crate::mode::mode_of;
use crate::{ContentHash, HeldFile, IgnoreRules, Manifest, RelPath, SyncError, SyncTarget};

/// Default quiet period before a burst of events is processed.
const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(100);

/// Upper bound on batching, so a file written continuously still syncs every second.
const MAX_BATCH_DELAY: Duration = Duration::from_secs(1);

/// How often a missing source is checked; dev servers create output folders late.
const SOURCE_POLL: Duration = Duration::from_millis(500);

/// Events buffered between the watcher thread and the sync task; overflow forces
/// a full rescan instead of growing memory during event storms.
const EVENT_BUFFER: usize = 4096;

/// Recent errors kept in [`SyncStatus::errors`].
const MAX_ISSUES: usize = 32;

/// Files modified this recently may change again within the same mtime tick, so
/// their mtime is not trusted for skipping (the same "racy clean" rule git uses).
const RACY_WINDOW: Duration = Duration::from_secs(2);

/// Settings for one sync entry.
#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// `ignore` globs; see [`IgnoreRules`].
    pub ignore: Vec<String>,
    /// Quiet period before processing a burst of events.
    pub debounce: Duration,
    /// Files larger than this are reported and skipped instead of read into memory.
    pub max_file: Option<u64>,
    /// The target keeps only a read-only attribute, not permission bits (a
    /// Windows client, see [`crate::READ_ONLY_MODES`]): a held file's mode then
    /// matches the source's when both or neither have a write bit, so `0o755`
    /// sources are not resent to a target that reports `0o644`.
    pub read_only_modes: bool,
}

impl Default for SyncOptions {
    /// No ignores, 100 ms debounce, no size limit, exact modes.
    fn default() -> Self {
        Self {
            ignore: Vec::new(),
            debounce: DEFAULT_DEBOUNCE,
            max_file: None,
            read_only_modes: false,
        }
    }
}

/// What a sync is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncState {
    /// The source directory does not exist yet; it is polled.
    WaitingForSource,
    /// Copying or removing files.
    Syncing,
    /// Up to date and following events.
    Watching,
    /// Stopped by [`SyncHandle::stop`] or drop.
    Stopped,
}

/// One recorded failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncIssue {
    /// Relative path involved, when there is one.
    pub path: Option<String>,
    /// Error text.
    pub message: String,
    /// When it happened.
    pub at: SystemTime,
}

/// Snapshot for `devlanes remote sync`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    /// Source directory as configured.
    pub source: PathBuf,
    /// Current activity.
    pub state: SyncState,
    /// End of the last completed pass.
    pub last_sync: Option<SystemTime>,
    /// Files currently mirrored.
    pub files: usize,
    /// Total size of mirrored files.
    pub bytes: u64,
    /// Files written since start.
    pub written: u64,
    /// Files removed since start.
    pub removed: u64,
    /// Completed passes (initial copy plus one per event batch).
    pub passes: u64,
    /// Most recent errors, oldest first, at most 32.
    pub errors: Vec<SyncIssue>,
    /// Errors since start, including ones no longer in `errors`.
    pub error_count: u64,
}

impl SyncStatus {
    /// An empty status for `source`.
    fn new(source: PathBuf, state: SyncState) -> Self {
        Self {
            source,
            state,
            last_sync: None,
            files: 0,
            bytes: 0,
            written: 0,
            removed: 0,
            passes: 0,
            errors: Vec::new(),
            error_count: 0,
        }
    }
}

/// Copies a source directory to a target and keeps it updated.
pub struct Syncer {
    source: PathBuf,
    target: Arc<dyn SyncTarget>,
    rules: Arc<IgnoreRules>,
    debounce: Duration,
    max_file: Option<u64>,
    /// See [`SyncOptions::read_only_modes`].
    read_only_modes: bool,
    /// What the target said it holds, as index entries; see [`Syncer::offer_held`].
    seed: Option<BTreeMap<RelPath, Tracked>>,
    /// Source root resolved for [`Syncer::offer_held`]; `Some(None)` when absent.
    held_root: Option<Option<PathBuf>>,
}

impl Syncer {
    /// Prepares a sync; nothing touches the filesystem until [`Syncer::start`] or
    /// [`Syncer::sync_once`].
    pub fn new(
        source: impl Into<PathBuf>,
        target: Arc<dyn SyncTarget>,
        options: SyncOptions,
    ) -> Result<Self, SyncError> {
        Ok(Self {
            source: source.into(),
            target,
            rules: Arc::new(IgnoreRules::new(&options.ignore)?),
            debounce: options.debounce,
            max_file: options.max_file,
            read_only_modes: options.read_only_modes,
            seed: None,
            held_root: None,
        })
    }

    /// Takes one batch of what a remote target already holds, before
    /// [`Syncer::start`]. Call it for every batch of the target's manifest; the
    /// first pass then sends only files the target lacks or holds differently, and
    /// deletes held files the source does not have (ignored paths excepted).
    ///
    /// Each batch is compared with the source right away (hashing source files
    /// whose size and mode match), and only its outcome is kept: matching files
    /// become entries of the index the syncer keeps for the source anyway, and
    /// files the source lacks stay in it until the first pass deletes them. The
    /// target's manifest itself is never held whole.
    pub async fn offer_held(&mut self, batch: Vec<HeldFile>) -> Result<(), SyncError> {
        let root = match &self.held_root {
            Some(root) => root.clone(),
            None => {
                let root = canonical_source(&self.source).await?;
                self.held_root = Some(root.clone());
                root
            }
        };
        let rules = self.rules.clone();
        let max_file = self.max_file;
        let read_only_modes = self.read_only_modes;
        let entries = tokio::task::spawn_blocking(move || {
            batch
                .into_iter()
                .filter_map(|held| {
                    held_entry(root.as_deref(), &rules, max_file, read_only_modes, held)
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|error| SyncError::Io {
            context: "comparing the target's files".into(),
            source: io::Error::other(error),
        })?;
        self.seed.get_or_insert_with(BTreeMap::new).extend(entries);
        Ok(())
    }

    /// Runs one full pass without watching and returns the resulting status.
    pub async fn sync_once(&self) -> Result<SyncStatus, SyncError> {
        let root = canonical_source(&self.source)
            .await?
            .ok_or_else(|| SyncError::SourceMissing(self.source.clone()))?;
        check_overlap(&root, self.target.as_ref()).await?;
        let (status, _receiver) =
            watch::channel(SyncStatus::new(self.source.clone(), SyncState::Syncing));
        let mut engine = self.engine(root, status, self.seed.clone());
        engine.pass(None).await;
        Ok(engine.status.borrow().clone())
    }

    /// Starts syncing in a background task.
    ///
    /// When the source exists, the initial copy finishes before this returns, so a
    /// service can rely on its files. Otherwise the task waits for the source to
    /// appear and this returns at once in [`SyncState::WaitingForSource`].
    pub async fn start(mut self) -> Result<SyncHandle, SyncError> {
        let root = canonical_source(&self.source).await?;
        if let Some(root) = &root {
            check_overlap(root, self.target.as_ref()).await?;
        }
        let state = if root.is_some() {
            SyncState::Syncing
        } else {
            SyncState::WaitingForSource
        };
        let (status, receiver) = watch::channel(SyncStatus::new(self.source.clone(), state));
        let debounce = self.debounce;
        let cancel = CancellationToken::new();
        let seed = self.seed.take();
        let mut engine = self.engine(root.clone().unwrap_or_default(), status, seed);
        let watch = match root {
            Some(root) => {
                // Watch before copying so edits made during the copy are not missed.
                let watch = SourceWatch::new(&root).await?;
                engine.pass(None).await;
                Some(watch)
            }
            None => None,
        };
        let task = tokio::spawn(run(engine, watch, cancel.clone(), debounce));
        Ok(SyncHandle {
            status: receiver,
            cancel,
            task: Some(task),
        })
    }

    /// Builds the engine for a canonical root. A seed from [`Syncer::offer_held`]
    /// replaces reading the target's manifest.
    fn engine(
        &self,
        root: PathBuf,
        status: watch::Sender<SyncStatus>,
        seed: Option<BTreeMap<RelPath, Tracked>>,
    ) -> Engine {
        Engine {
            source: self.source.clone(),
            root,
            target: self.target.clone(),
            rules: self.rules.clone(),
            primed: seed.is_some(),
            index: seed.unwrap_or_default(),
            status,
            max_file: self.max_file,
        }
    }
}

/// A running sync. Dropping it stops the sync without waiting.
pub struct SyncHandle {
    status: watch::Receiver<SyncStatus>,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

impl SyncHandle {
    /// Current status.
    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }

    /// A receiver that wakes on every status change.
    pub fn subscribe(&self) -> watch::Receiver<SyncStatus> {
        self.status.clone()
    }

    /// Stops watching and waits for the task to finish its current file.
    pub async fn stop(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SyncHandle {
    /// Cancels the background task.
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// What the target is known to hold for one path.
#[derive(Debug, Clone)]
struct Tracked {
    size: u64,
    /// `None` for racily clean files, which must be rehashed on the next look.
    mtime: Option<SystemTime>,
    mode: u32,
    hash: ContentHash,
}

impl Tracked {
    /// Whether a scan result matches without reading the file.
    fn unchanged(&self, meta: &FileMeta) -> bool {
        self.mtime.is_some()
            && self.mtime == meta.mtime
            && self.size == meta.size
            && self.mode == meta.mode
    }
}

/// Source metadata from a scan.
#[derive(Debug, Clone)]
struct FileMeta {
    size: u64,
    mtime: Option<SystemTime>,
    mode: u32,
}

impl From<&fs::Metadata> for FileMeta {
    /// Extracts size, mtime and permission bits.
    fn from(meta: &fs::Metadata) -> Self {
        Self {
            size: meta.len(),
            mtime: meta.modified().ok(),
            mode: mode_of(meta),
        }
    }
}

/// Result of scanning part of the source.
#[derive(Debug, Default)]
struct Scan {
    files: BTreeMap<RelPath, FileMeta>,
    issues: Vec<(Option<String>, String)>,
    /// Directories that could not be read; tracked files under them are kept.
    unreadable: Vec<RelPath>,
    /// The scan root itself failed; no deletions may follow from it.
    aborted: bool,
}

/// Owns the index of what the target holds; runs on one task.
struct Engine {
    source: PathBuf,
    root: PathBuf,
    target: Arc<dyn SyncTarget>,
    rules: Arc<IgnoreRules>,
    index: BTreeMap<RelPath, Tracked>,
    /// Whether the target manifest has been read; until then deletions and skips
    /// cannot know what the target already holds.
    primed: bool,
    status: watch::Sender<SyncStatus>,
    max_file: Option<u64>,
}

impl Engine {
    /// Reconciles the whole source (`None`) or the given paths.
    async fn pass(&mut self, scopes: Option<Vec<RelPath>>) {
        self.status
            .send_modify(|status| status.state = SyncState::Syncing);
        match scopes {
            None => self.apply(None).await,
            Some(scopes) => {
                for scope in minimal_scopes(scopes) {
                    self.apply(Some(scope)).await;
                }
            }
        }
        let files = self.index.len();
        let bytes = self.index.values().map(|tracked| tracked.size).sum();
        self.status.send_modify(|status| {
            status.state = SyncState::Watching;
            status.last_sync = Some(SystemTime::now());
            status.files = files;
            status.bytes = bytes;
            status.passes += 1;
        });
    }

    /// Records an error in the status.
    fn issue(&self, path: Option<String>, message: String) {
        tracing::debug!(source = %self.source.display(), ?path, %message, "sync issue");
        self.status.send_modify(|status| {
            if status.errors.len() == MAX_ISSUES {
                status.errors.remove(0);
            }
            status.errors.push(SyncIssue {
                path,
                message,
                at: SystemTime::now(),
            });
            status.error_count += 1;
        });
    }

    /// Removes stale files, then writes changed ones, within `scope`.
    ///
    /// Removals go first so a file that became a directory (or the reverse) is
    /// cleared before its replacement is written.
    async fn apply(&mut self, scope: Option<RelPath>) {
        let manifest = if scope.is_none() && !self.primed {
            match self.target.manifest().await {
                Ok(manifest) => Some(manifest),
                Err(error) => {
                    self.issue(None, error.to_string());
                    None
                }
            }
        } else {
            None
        };
        let root = self.root.clone();
        let rules = self.rules.clone();
        let scan_scope = scope.clone();
        let scan =
            match tokio::task::spawn_blocking(move || scan(&root, scan_scope.as_ref(), &rules))
                .await
            {
                Ok(scan) => scan,
                Err(error) => {
                    self.issue(None, format!("scan failed: {error}"));
                    return;
                }
            };
        for (path, message) in &scan.issues {
            self.issue(path.clone(), message.clone());
        }
        if !scan.aborted {
            self.remove_stale(&scan, scope.as_ref(), manifest.as_ref())
                .await;
        }
        self.write_changed(scan.files, manifest.as_ref()).await;
        if manifest.is_some() {
            self.primed = true;
        }
    }

    /// Deletes target files whose source is gone.
    async fn remove_stale(
        &mut self,
        scan: &Scan,
        scope: Option<&RelPath>,
        manifest: Option<&Manifest>,
    ) {
        let kept = |rel: &RelPath| {
            scan.files.contains_key(rel) || scan.unreadable.iter().any(|dir| rel.is_within(dir))
        };
        let mut stale: BTreeSet<RelPath> = self
            .index
            .keys()
            .filter(|rel| scope.is_none_or(|scope| rel.is_within(scope)) && !kept(rel))
            .cloned()
            .collect();
        if let Some(manifest) = manifest {
            // Ignored paths on the target belong to someone else; leave them.
            stale.extend(
                manifest
                    .keys()
                    .filter(|rel| !kept(rel) && !self.rules.is_ignored(rel))
                    .cloned(),
            );
        }
        for rel in stale {
            match self.target.remove(&rel).await {
                Ok(()) => {
                    self.index.remove(&rel);
                    self.status.send_modify(|status| status.removed += 1);
                }
                Err(error) => self.issue(Some(rel.to_string()), error.to_string()),
            }
        }
    }

    /// Writes files whose content or mode differs from what the target holds.
    async fn write_changed(
        &mut self,
        files: BTreeMap<RelPath, FileMeta>,
        manifest: Option<&Manifest>,
    ) {
        for (rel, meta) in files {
            if self
                .index
                .get(&rel)
                .is_some_and(|tracked| tracked.unchanged(&meta))
            {
                continue;
            }
            let root = self.root.clone();
            let read_rel = rel.clone();
            let max_file = self.max_file;
            let read =
                tokio::task::spawn_blocking(move || read_source(&root, &read_rel, max_file)).await;
            let (contents, tracked) = match read {
                Ok(Ok(Some(read))) => read,
                // Gone or replaced by a symlink since the scan; its own event follows.
                Ok(Ok(None)) => continue,
                Ok(Err(error)) => {
                    self.issue(Some(rel.to_string()), error.to_string());
                    continue;
                }
                Err(error) => {
                    self.issue(Some(rel.to_string()), format!("read failed: {error}"));
                    continue;
                }
            };
            let held = self
                .index
                .get(&rel)
                .map(|known| (known.hash, known.mode))
                .or_else(|| {
                    manifest
                        .and_then(|manifest| manifest.get(&rel))
                        .map(|entry| (entry.hash, entry.mode))
                });
            if held == Some((tracked.hash, tracked.mode)) {
                self.index.insert(rel, tracked);
                continue;
            }
            match self.target.write_file(&rel, contents, tracked.mode).await {
                Ok(()) => {
                    self.index.insert(rel, tracked);
                    self.status.send_modify(|status| status.written += 1);
                }
                Err(error) => self.issue(Some(rel.to_string()), error.to_string()),
            }
        }
    }
}

/// Drops scopes covered by another scope in the list.
fn minimal_scopes(scopes: Vec<RelPath>) -> Vec<RelPath> {
    let sorted: BTreeSet<RelPath> = scopes.into_iter().collect();
    let mut kept: Vec<RelPath> = Vec::new();
    for scope in sorted {
        if !kept.iter().any(|outer| scope.is_within(outer)) {
            kept.push(scope);
        }
    }
    kept
}

/// Background loop: wait for the source, watch it, process batches; rewatch when
/// the source directory is replaced.
async fn run(
    mut engine: Engine,
    mut watch: Option<SourceWatch>,
    cancel: CancellationToken,
    debounce: Duration,
) {
    loop {
        let mut current = match watch.take() {
            Some(current) => current,
            None => {
                engine
                    .status
                    .send_modify(|status| status.state = SyncState::WaitingForSource);
                let Some(root) = wait_for_source(&engine.source, &cancel).await else {
                    break;
                };
                if let Err(error) = check_overlap(&root, engine.target.as_ref()).await {
                    // Overlap cannot fix itself; stay waiting rather than copy into the source.
                    engine.issue(None, error.to_string());
                    cancel.cancelled().await;
                    break;
                }
                engine.root = root;
                match SourceWatch::new(&engine.root).await {
                    Ok(current) => {
                        engine.pass(None).await;
                        current
                    }
                    Err(error) => {
                        engine.issue(None, error.to_string());
                        tokio::select! {
                            () = cancel.cancelled() => break,
                            () = tokio::time::sleep(SOURCE_POLL) => continue,
                        }
                    }
                }
            }
        };
        match follow(&mut engine, &mut current, &cancel, debounce).await {
            Follow::Cancelled => break,
            Follow::Rewatch => {}
        }
    }
    engine
        .status
        .send_modify(|status| status.state = SyncState::Stopped);
}

/// Why [`follow`] returned.
enum Follow {
    Cancelled,
    /// The source directory vanished or was replaced; its watch is stale.
    Rewatch,
}

/// Processes event batches until cancelled or the source root changes identity.
async fn follow(
    engine: &mut Engine,
    watch: &mut SourceWatch,
    cancel: &CancellationToken,
    debounce: Duration,
) -> Follow {
    loop {
        let first = tokio::select! {
            () = cancel.cancelled() => return Follow::Cancelled,
            event = watch.events.recv() => event,
        };
        let Some(first) = first else {
            return Follow::Rewatch;
        };
        let mut batch = Batch::default();
        batch.add(engine, first);
        let deadline = Instant::now() + MAX_BATCH_DELAY;
        loop {
            let wait = debounce.min(deadline.saturating_duration_since(Instant::now()));
            if wait.is_zero() {
                break;
            }
            tokio::select! {
                () = cancel.cancelled() => return Follow::Cancelled,
                next = tokio::time::timeout(wait, watch.events.recv()) => match next {
                    Ok(Some(event)) => batch.add(engine, event),
                    Ok(None) | Err(_) => break,
                },
            }
        }
        if watch.overflow.swap(false, Ordering::Relaxed) {
            batch.full = true;
        }
        // The removal event matters on its own: a directory recreated at once can
        // get the freed inode back (ext4 does), and the old watch is dead either way.
        if batch.root_gone || root_identity(&engine.root).await != watch.identity {
            // Mirror whatever is there now (nothing, if it was deleted), then rewatch.
            engine.pass(None).await;
            return Follow::Rewatch;
        }
        if batch.full {
            engine.pass(None).await;
        } else if !batch.paths.is_empty() {
            engine.pass(Some(batch.paths.into_iter().collect())).await;
        }
    }
}

/// Paths touched by one debounced burst.
#[derive(Default)]
struct Batch {
    paths: BTreeSet<RelPath>,
    full: bool,
    /// The root itself was removed or renamed.
    root_gone: bool,
}

impl Batch {
    /// Adds an event's paths, or marks a full rescan.
    fn add(&mut self, engine: &Engine, event: notify::Result<Event>) {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                engine.issue(None, format!("watcher: {error}"));
                self.full = true;
                return;
            }
        };
        if event.need_rescan() {
            self.full = true;
            return;
        }
        // Reads (including the syncer's own) are noise; a closed writer is a real change.
        if let EventKind::Access(kind) = event.kind
            && kind != AccessKind::Close(AccessMode::Write)
        {
            return;
        }
        for path in &event.paths {
            let relative = path
                .strip_prefix(&engine.root)
                .or_else(|_| path.strip_prefix(&engine.source));
            let Ok(relative) = relative else {
                continue;
            };
            if relative.as_os_str().is_empty() {
                self.full = true;
                if matches!(
                    event.kind,
                    EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(_))
                ) {
                    self.root_gone = true;
                }
                continue;
            }
            match RelPath::from_path(relative) {
                Ok(rel) if engine.rules.is_ignored(&rel) => {}
                Ok(rel) => {
                    self.paths.insert(rel);
                }
                Err(error) => engine.issue(Some(path.display().to_string()), error.to_string()),
            }
        }
    }
}

/// A recursive watch on the source root.
struct SourceWatch {
    // Kept alive for its event thread; dropping it stops events.
    _watcher: RecommendedWatcher,
    events: mpsc::Receiver<notify::Result<Event>>,
    overflow: Arc<AtomicBool>,
    /// File identity of the root when the watch started.
    identity: Option<FileIdentity>,
}

impl SourceWatch {
    /// Starts watching `root` recursively.
    async fn new(root: &Path) -> Result<Self, SyncError> {
        let (sender, events) = mpsc::channel(EVENT_BUFFER);
        let overflow = Arc::new(AtomicBool::new(false));
        let flag = overflow.clone();
        let owned = root.to_owned();
        let watcher = tokio::task::spawn_blocking(move || {
            let mut watcher = notify::recommended_watcher(move |event| {
                // Runs on the watcher's thread; never block it.
                if sender.try_send(event).is_err() {
                    flag.store(true, Ordering::Relaxed);
                }
            })?;
            watcher.watch(&owned, RecursiveMode::Recursive)?;
            Ok::<_, notify::Error>(watcher)
        })
        .await
        .map_err(|error| SyncError::Io {
            context: format!("watching {}", root.display()),
            source: io::Error::other(error),
        })?
        .map_err(|source| SyncError::Watch {
            path: root.to_owned(),
            source,
        })?;
        Ok(Self {
            _watcher: watcher,
            events,
            overflow,
            identity: root_identity(root).await,
        })
    }
}

/// File identity of `root`, or `None` when it is missing or not a directory.
async fn root_identity(root: &Path) -> Option<FileIdentity> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        fs::symlink_metadata(&root)
            .ok()
            .filter(fs::Metadata::is_dir)?;
        devlanes_sys::path_identity(&root).ok()
    })
    .await
    .ok()
    .flatten()
}

/// Polls until `source` is a directory; `None` when cancelled first.
async fn wait_for_source(source: &Path, cancel: &CancellationToken) -> Option<PathBuf> {
    loop {
        if let Ok(Some(root)) = canonical_source(source).await {
            return Some(root);
        }
        tokio::select! {
            () = cancel.cancelled() => return None,
            () = tokio::time::sleep(SOURCE_POLL) => {}
        }
    }
}

/// Canonical source directory, `None` when absent or not a directory.
///
/// Canonical because FSEvents reports `/private/var/...` for `/var/...` paths.
async fn canonical_source(source: &Path) -> Result<Option<PathBuf>, SyncError> {
    let owned = source.to_owned();
    let result = tokio::task::spawn_blocking(move || fs::canonicalize(owned))
        .await
        .map_err(|error| SyncError::Io {
            context: format!("resolving {}", source.display()),
            source: io::Error::other(error),
        })?;
    match result {
        Ok(path) if path.is_dir() => Ok(Some(path)),
        Ok(_) => Ok(None),
        Err(error) if is_absent(&error) => Ok(None),
        Err(source_error) => Err(SyncError::Io {
            context: format!("resolving {}", source.display()),
            source: source_error,
        }),
    }
}

/// Refuses a local destination that contains or is inside the source.
async fn check_overlap(root: &Path, target: &dyn SyncTarget) -> Result<(), SyncError> {
    let Some(destination) = target.local_root() else {
        return Ok(());
    };
    let owned = destination.to_owned();
    let resolved = tokio::task::spawn_blocking(move || resolve_lexically(&owned))
        .await
        .map_err(|error| SyncError::Io {
            context: format!("resolving {}", destination.display()),
            source: io::Error::other(error),
        })?;
    if resolved.starts_with(root) || root.starts_with(&resolved) {
        return Err(SyncError::Overlap {
            source_dir: root.to_owned(),
            destination: destination.to_owned(),
        });
    }
    Ok(())
}

/// Canonicalizes the longest existing prefix of `path` and appends the rest.
fn resolve_lexically(path: &Path) -> PathBuf {
    let mut existing = path.to_path_buf();
    let mut tail = Vec::new();
    while !existing.exists() {
        match (existing.file_name(), existing.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_owned());
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    let mut resolved = fs::canonicalize(&existing).unwrap_or(existing);
    resolved.extend(tail.into_iter().rev());
    resolved
}

/// Missing, or a path component that is not a directory.
fn is_absent(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
    )
}

/// Lists regular files under `scope` (or the whole root) without following symlinks.
fn scan(root: &Path, scope: Option<&RelPath>, rules: &IgnoreRules) -> Scan {
    let mut out = Scan::default();
    let start = match scope {
        None => root.to_path_buf(),
        Some(rel) => {
            if rules.is_ignored(rel) {
                return out;
            }
            // A symlinked or vanished parent means the path is no longer in the tree.
            let mut dir = root.to_path_buf();
            for name in rel.parent_names() {
                dir.push(name);
                if !fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
                    return out;
                }
            }
            rel.to_path(root)
        }
    };
    let meta = match fs::symlink_metadata(&start) {
        Ok(meta) => meta,
        Err(error) if is_absent(&error) => return out,
        Err(error) => {
            out.issues
                .push((scope.map(ToString::to_string), error.to_string()));
            out.aborted = true;
            return out;
        }
    };
    if meta.is_file() {
        if let Some(rel) = scope {
            out.files.insert(rel.clone(), FileMeta::from(&meta));
        }
        return out;
    }
    if !meta.is_dir() {
        return out;
    }
    let walker = walkdir::WalkDir::new(&start)
        .min_depth(1)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            relative(root, entry.path()).is_none_or(|rel| !rules.is_ignored(&rel))
        });
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                let rel = error.path().and_then(|path| relative(root, path));
                out.issues
                    .push((rel.as_ref().map(ToString::to_string), error.to_string()));
                match rel {
                    Some(rel) => out.unreadable.push(rel),
                    None => out.aborted = true,
                }
                continue;
            }
        };
        // Symlinks, sockets and FIFOs are skipped; directories are only containers.
        if !entry.file_type().is_file() {
            continue;
        }
        let Some(rel) = relative(root, entry.path()) else {
            out.issues.push((
                Some(entry.path().display().to_string()),
                "skipped: the name is not valid UTF-8".into(),
            ));
            continue;
        };
        match entry.metadata() {
            Ok(meta) => {
                out.files.insert(rel, FileMeta::from(&meta));
            }
            Err(error) => {
                out.issues.push((Some(rel.to_string()), error.to_string()));
                out.unreadable.push(rel);
            }
        }
    }
    out
}

/// `path` relative to `root` as a [`RelPath`].
fn relative(root: &Path, path: &Path) -> Option<RelPath> {
    RelPath::from_path(path.strip_prefix(root).ok()?).ok()
}

/// The error for a file over [`SyncOptions::max_file`].
fn too_large(size: u64, limit: u64) -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        format!("{size} bytes is over the sync limit of {limit} bytes; not synced"),
    )
}

/// Reads a source file; `None` when it is gone or no longer a regular file.
fn read_source(
    root: &Path,
    rel: &RelPath,
    max_file: Option<u64>,
) -> io::Result<Option<(Bytes, Tracked)>> {
    let path = rel.to_path(root);
    match fs::symlink_metadata(&path) {
        Ok(meta) if meta.is_file() => {}
        Ok(_) => return Ok(None),
        Err(error) if is_absent(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(error) if is_absent(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    // Metadata from the open handle describes the bytes being read, not a later file.
    let meta = file.metadata()?;
    // Whole files are held in memory, so the size is checked before reading, and
    // the read is capped in case the file grows meanwhile.
    if let Some(limit) = max_file
        && meta.len() > limit
    {
        return Err(too_large(meta.len(), limit));
    }
    let cap = max_file.unwrap_or(u64::MAX);
    let mut contents = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(0));
    (&mut file)
        .take(cap.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 > cap {
        return Err(too_large(contents.len() as u64, cap));
    }
    let tracked = Tracked {
        size: contents.len() as u64,
        mtime: settled_mtime(&meta),
        mode: mode_of(&meta),
        hash: ContentHash::of(&contents),
    };
    Ok(Some((Bytes::from(contents), tracked)))
}

/// The mtime to trust for skipping, or `None` for racily clean files.
fn settled_mtime(meta: &fs::Metadata) -> Option<SystemTime> {
    let mtime = meta.modified().ok()?;
    let racy = SystemTime::now()
        .duration_since(mtime)
        .map_or(true, |age| age < RACY_WINDOW);
    (!racy).then_some(mtime)
}

/// The index entry a held file becomes, if any. Blocking.
///
/// A match is indexed with the source's own settled mtime and mode, so the first
/// pass skips it without reading; a held file the source lacks is indexed so the
/// first pass deletes it; a different file is left out, so the pass sends it.
fn held_entry(
    root: Option<&Path>,
    rules: &IgnoreRules,
    max_file: Option<u64>,
    read_only_modes: bool,
    held: HeldFile,
) -> Option<(RelPath, Tracked)> {
    // Ignored paths on the target belong to someone else; never compared or deleted.
    if rules.is_ignored(&held.path) {
        return None;
    }
    let probe = match root {
        Some(root) => probe_source(root, &held, max_file, read_only_modes, settled_mtime),
        None => Probe::Absent,
    };
    match probe {
        Probe::Matches {
            size,
            mtime,
            mode,
            hash,
        } => Some((
            held.path,
            Tracked {
                size,
                mtime,
                mode,
                hash,
            },
        )),
        Probe::Differs => None,
        // The held hash stays: should the file appear in the source before the
        // first pass, identical contents are still not resent.
        Probe::Absent => Some((
            held.path,
            Tracked {
                size: held.size,
                mtime: None,
                mode: held.mode,
                hash: held.hash.unwrap_or_else(ContentHash::unknown),
            },
        )),
    }
}

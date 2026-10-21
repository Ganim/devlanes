//! One-way folder sync for `services.<name>.sync`.
//!
//! A [`Syncer`] copies a source directory to a [`SyncTarget`] once, then follows
//! filesystem events. Changes are debounced and detected by size and mtime, then
//! confirmed by content hash, so touching a file does not rewrite it. Deletions
//! are mirrored, `ignore` globs are honored on both sides, symlinks and special
//! files are skipped, and only permission bits travel. [`LocalTarget`] writes a
//! temp file and renames it into place, so readers never see a partial file.
//!
//! Empty directories are not synced; directories exist on the target only to
//! hold files.

mod held;
mod ignore;
mod path;
mod syncer;
mod target;

pub use held::{HeldFile, HeldWalk};
pub use ignore::IgnoreRules;
pub use path::RelPath;
pub use syncer::{SyncHandle, SyncIssue, SyncOptions, SyncState, SyncStatus, Syncer};
pub use target::{
    ContentHash, HashCache, LocalTarget, MODE_MASK, Manifest, ManifestEntry, OWNER_MARKER,
    PartialFile, SyncTarget, TEMP_PREFIX,
};

use std::path::PathBuf;

/// Failure to configure or run a sync.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    /// A relative path that could escape the sync root.
    #[error("invalid sync path `{path}`: {reason}")]
    InvalidPath {
        /// The rejected path.
        path: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// An `ignore` entry that is not a valid glob.
    #[error("invalid ignore pattern `{pattern}`: {source}")]
    InvalidIgnore {
        /// The pattern as written.
        pattern: String,
        /// Parser error.
        #[source]
        source: globset::Error,
    },
    /// Source and destination contain each other.
    #[error("sync destination {destination} and source {source_dir} overlap")]
    Overlap {
        /// Source directory.
        source_dir: PathBuf,
        /// Destination directory.
        destination: PathBuf,
    },
    /// A one-shot sync of a source that does not exist.
    #[error("sync source {0} does not exist")]
    SourceMissing(PathBuf),
    /// The filesystem watcher could not start.
    #[error("cannot watch {path}: {source}")]
    Watch {
        /// Watched directory.
        path: PathBuf,
        /// Watcher error.
        #[source]
        source: notify::Error,
    },
    /// Local filesystem work failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// Underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A remote target reported a failure.
    #[error("sync target failed: {0}")]
    Target(String),
}

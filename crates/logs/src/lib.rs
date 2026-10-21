//! Synchronous, single-writer per-run log storage.
//!
//! A daemon must own each [`UnitLog`] on a writer thread or call it through
//! `spawn_blocking`; ingestion, queries, compression and retention perform disk I/O.
//! Use one stream identifier per stdout/stderr/PTY byte stream. The daemon persists
//! [`SegmentMetadata`] and the next sequence number across runs; this crate has no
//! database dependency. Cross-run retention is the daemon's responsibility.
//!
//! ```
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use devlanes_logs::{LogOptions, UnitLog};
//! use devlanes_proto::LogQuery;
//! let run = tempfile::tempdir()?;
//! let mut logs = UnitLog::open(run.path(), "api", LogOptions::default())?;
//! logs.ingest(b"\x1b[31mERROR unavailable\x1b[0m\n", 1_000)?;
//! let page = logs.query(&LogQuery { tail: Some(100), max_bytes: Some(16_384),
//!     ..LogQuery::default() })?;
//! assert_eq!(page.items[0].level.as_deref(), Some("error"));
//! logs.finish(1_001)?;
//! # Ok(()) }
//! ```

mod framing;
mod level;
mod query;
mod storage;

pub use framing::{Frame, Framer, MAX_LINE_BYTES, PARTIAL_FLUSH_MS, RawRing};
pub use level::extract_level;
pub use storage::{IndexEntry, LogOptions, SegmentMetadata, UnitLog};

/// Expected failures are suitable for conversion into daemon RPC errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("log I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("log metadata: {0}")]
    Metadata(#[from] serde_json::Error),
    #[error("invalid log query: {0}")]
    Query(String),
    #[error("invalid log options: {0}")]
    Options(&'static str),
    #[error("corrupt log record: {0}")]
    Corrupt(String),
    #[error("log sequence exhausted")]
    SequenceExhausted,
    #[error("log writer failed previously; reopen the run to recover")]
    WriterFailed,
}

/// Library operations never hide filesystem or query validation failures.
pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests;

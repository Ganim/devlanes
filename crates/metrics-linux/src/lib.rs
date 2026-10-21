//! Linux process collector for `devlanes-metrics`, built on procfs.
//!
//! CPU and tree data come from `/proc/<pid>/stat`, RSS from `statm`, and the
//! memory number from `smaps_rollup` PSS, which divides shared pages among their
//! sharers so a tree's sum is meaningful. PSS walks page tables, so it is
//! refreshed at a lower rate (default every 10 s) and RSS stands in until the
//! first read or when the file is unavailable.
//!
//! The [`procfs`] parsers are platform-independent; the collector is Linux-only.

pub mod procfs;

#[cfg(target_os = "linux")]
mod collector;

#[cfg(target_os = "linux")]
pub use collector::{DEFAULT_PSS_INTERVAL, LinuxSource, process_identity};

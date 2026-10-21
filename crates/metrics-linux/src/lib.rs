//! Linux process collector for `devlanes-metrics`, built on procfs.
//!
//! CPU and tree data come from `/proc/<pid>/stat`, RSS from `statm`, and the
//! memory number from `smaps_rollup` PSS, which divides shared pages among their
//! sharers so a tree's sum is meaningful. PSS walks page tables, so it is
//! refreshed at a lower rate (default every 10 s) and RSS stands in until the
//! first read or when the file is unavailable.
//!
//! [`pressure`] reads system CPU, PSI and available memory for the start gate
//! (`LinuxPressure`, a `PressureSource`).
//!
//! The [`procfs`] and [`pressure`] parsers are platform-independent; the
//! collector and the pressure source are Linux-only.

pub mod pressure;
pub mod procfs;

#[cfg(target_os = "linux")]
mod collector;

#[cfg(target_os = "linux")]
pub use collector::{DEFAULT_PSS_INTERVAL, LinuxSource, process_identity};
#[cfg(target_os = "linux")]
pub use pressure::LinuxPressure;

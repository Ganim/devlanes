//! macOS process collector for `devlanes-metrics`, built on libproc.
//!
//! Memory is `phys_footprint` from `proc_pid_rusage(RUSAGE_INFO_V6)`, the number
//! Activity Monitor shows; RSS collapses once idle pages are compressed. On other
//! platforms this crate is empty.
//!
//! Unsafe code is confined to the FFI module; every block states its invariants.

#![cfg_attr(target_os = "macos", allow(unsafe_code))]
#![deny(clippy::undocumented_unsafe_blocks)]

#[cfg(target_os = "macos")]
mod collector;
#[cfg(target_os = "macos")]
mod ffi;

#[cfg(target_os = "macos")]
pub use collector::{MacosSource, process_identity};

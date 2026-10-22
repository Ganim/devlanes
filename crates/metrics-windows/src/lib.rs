//! Windows process collector for `devlanes-metrics`.
//!
//! CPU is the change in a process's kernel plus user time between samples. Memory
//! is the private working set (resident pages no other process shares, Task
//! Manager's Memory column) as the footprint, and the working set as RSS.
//!
//! `WindowsPressure` is the start gate's `PressureSource`: system CPU from
//! `GetSystemTimes`, the memory level from `GlobalMemoryStatusEx`'s memory load.
//!
//! All system calls go through the safe wrappers in `devlanes_sys::windows::usage`.
//! On other platforms this crate is empty.

#![forbid(unsafe_code)]

#[cfg(windows)]
mod collector;
#[cfg(windows)]
mod pressure;

#[cfg(windows)]
pub use collector::{WindowsSource, process_identity};
#[cfg(windows)]
pub use pressure::WindowsPressure;

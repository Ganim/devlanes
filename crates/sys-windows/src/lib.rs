//! Safe wrappers over the Win32 calls devlanes needs on Windows: security
//! identifiers and descriptors, private directories, named pipes, the process
//! table, job objects, process creation, consoles, windows, the registry, the
//! shell's `ShellExecuteExW`, file identity, certificate stores and socket options.
//!
//! The wrappers hold no policy and produce no user-facing text; the crates that
//! call them decide what a result means. Every call blocks, so async callers use
//! `spawn_blocking` except where a function says otherwise. On other platforms
//! the crate is empty.
//!
//! ```
//! # #[cfg(windows)]
//! # fn main() -> std::io::Result<()> {
//! let me = devlanes_sys_windows::token::current_user()?;
//! assert!(me.to_string().starts_with("S-1-"));
//! # Ok(())
//! # }
//! # #[cfg(not(windows))]
//! # fn main() {}
//! ```

#![cfg(windows)]
// Unsafe code is limited to the FFI modules below; each block documents its invariants.
#![deny(unsafe_code)]
#![warn(clippy::undocumented_unsafe_blocks)]

#[allow(unsafe_code)]
pub mod cert;
#[allow(unsafe_code)]
pub mod conpty;
#[allow(unsafe_code)]
pub mod console;
#[allow(unsafe_code)]
pub mod file;
#[allow(unsafe_code)]
pub mod job;
#[allow(unsafe_code)]
pub mod net;
#[allow(unsafe_code)]
pub mod pipe;
#[allow(unsafe_code)]
pub mod proc;
#[allow(unsafe_code)]
pub mod registry;
#[allow(unsafe_code)]
pub mod security;
#[allow(unsafe_code)]
pub mod shell_execute;
#[allow(unsafe_code)]
pub mod spawn;
#[allow(unsafe_code)]
pub mod token;
#[allow(unsafe_code)]
pub mod usage;
#[allow(unsafe_code)]
mod util;
#[allow(unsafe_code)]
pub mod window;

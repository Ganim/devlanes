//! `devlanes-hosts-service`, the Windows service that keeps devlanes hostnames in
//! the hosts file for one user's daemon.
//!
//! It is the Windows counterpart of `devlanes-hosts` (the sudo helper): a
//! LocalSystem service, installed by an elevated `devlanes certs trust --hosts`,
//! that answers the installing user's daemon over `\\.\pipe\devlanes-hosts` with
//! the helper's commands, limits and exit codes
//! ([`devlanes_hosts_helper::protocol`]). Validation and block editing come from
//! `devlanes-hosts-helper`; Win32 calls go through `devlanes-sys`, and the
//! service plumbing through the `windows-service` crate, so this crate has no
//! `unsafe`. See `README.md` for the threat model.
//!
//! - [`handler`] answers one request against a hosts file path; tests drive it
//!   (and [`server`]) against temp files and temp pipe names.
//! - [`server`] owns the pipe: its access list, the caller check, framing.
//! - [`service`] is what the binary runs: install checks, configuration from the
//!   registry (never from the caller), the service control handler.
//! - [`install`] is the elevated install and removal that `devlanes.exe` runs.
//!
//! Everything is Windows only; elsewhere the crate is empty and the binary
//! refuses to run.

#[cfg(windows)]
pub mod checks;
#[cfg(windows)]
pub mod config;
#[cfg(windows)]
pub mod flush;
#[cfg(windows)]
pub mod handler;
#[cfg(windows)]
pub mod install;
#[cfg(windows)]
pub mod server;
#[cfg(windows)]
pub mod service;
#[cfg(windows)]
pub mod write;

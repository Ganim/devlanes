//! `devlanes-hosts`, the root helper that keeps devlanes hostnames in `/etc/hosts`.
//!
//! The binary runs as root through a NOPASSWD sudo rule that names only its
//! installed path, so this crate is the whole privileged surface: std only, no
//! unsafe, no async runtime. It accepts lowercase hostnames under `.localhost` (and
//! optionally one reserved TLD), maps each to `127.0.0.1` and `::1`, and rewrites only
//! the lines between `# devlanes begin` and `# devlanes end`. See `README.md` for the
//! reasoning and the security model.
//!
//! The library half holds the pure logic ([`names`], [`block`]) and the file
//! replacement ([`write`](mod@write)), so the daemon side (`devlanes-resolver-hosts`) can share
//! the same validation and constants.

pub mod block;
// The helper edits `/etc/hosts` through sudo; Windows needs no hosts file entries.
#[cfg(unix)]
pub mod cli;
#[cfg(unix)]
pub mod flush;
pub mod names;
#[cfg(unix)]
pub mod write;

/// The file the helper edits.
pub const HOSTS_PATH: &str = "/etc/hosts";

/// Where `devlanes certs trust --hosts` installs the helper.
pub const INSTALL_PATH: &str = "/usr/local/libexec/devlanes-hosts";

/// Where `devlanes certs trust --hosts` installs the sudo rule.
pub const SUDOERS_PATH: &str = "/etc/sudoers.d/devlanes";

/// Version of the command-line contract between the daemon and the helper.
///
/// Bump it when arguments, input or output change incompatibly; the daemon compares
/// it with `devlanes-hosts version` and asks for a reinstall on mismatch.
pub const PROTOCOL: u32 = 1;

/// Build profile reported by `version`; the installer refuses `debug` builds because
/// they accept `--hosts-file`.
pub const BUILD: &str = if cfg!(debug_assertions) {
    "debug"
} else {
    "release"
};

/// Exit codes. 1 is never used, so under `sudo -n` it always means sudo itself refused.
pub mod exit {
    /// Success.
    pub const OK: u8 = 0;
    /// Bad arguments.
    pub const USAGE: u8 = 2;
    /// A name or the TLD failed validation, or there were too many names.
    pub const INVALID_NAMES: u8 = 3;
    /// The hosts file is a symlink, not a regular file, has malformed markers, or kept
    /// changing during the update.
    pub const REFUSED_FILE: u8 = 4;
    /// Reading or writing failed.
    pub const IO: u8 = 5;
}

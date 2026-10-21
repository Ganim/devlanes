//! The `hosts_file` hostname fallback: deciding when devlanes writes exact names to
//! `/etc/hosts`, sending them to the root helper, and installing that helper.
//!
//! - [`policy`] turns the user's `hostnames.hosts_file`, each project's
//!   `lanes.hosts_file` hint and a probe of the system resolver into the set of
//!   names to write.
//! - [`HostsClient`] runs `sudo -n /usr/local/libexec/devlanes-hosts apply` with
//!   those names on stdin; [`HostsSync`] debounces route changes in the daemon.
//! - [`install`] builds and runs the commands of `devlanes trust --hosts` and
//!   `devlanes untrust --hosts` in the user's terminal.
//!
//! Validation rules and paths come from `devlanes-hosts-helper`, so the daemon
//! filters names exactly the way the helper checks them. The helper's README
//! explains why the fallback exists and its security model.

pub mod client;
pub mod exec;
#[cfg(any(test, feature = "fake"))]
pub mod fake;
pub mod install;
pub mod policy;
pub mod sync;

pub use client::{Applied, HelperVersion, HostsClient, HostsError};
pub use devlanes_hosts_helper::names::Suffixes;
pub use devlanes_hosts_helper::{INSTALL_PATH, PROTOCOL, SUDOERS_PATH};
pub use exec::{
    CommandOutput, CommandSpec, Executor, FileInfo, FileKind, FsProbe, SystemExecutor, SystemFs,
};
pub use install::{InstallError, InstallInputs, InstallPlan, Platform, Step};
pub use policy::{
    DesiredHosts, HostsFileMode, HostsFilePolicy, LocalhostResolution, SkipReason, Skipped,
    filter_names, probe_localhost,
};
pub use sync::{HostsSync, SyncConfig, SyncState};

//! Resolver cache flushes after the hosts file changes.
//!
//! macOS caches `/etc/hosts` answers in mDNSResponder and the Directory Service
//! cache. On Linux, glibc rereads the file on every lookup and systemd-resolved
//! notices the change itself, but flushing resolved drops answers it cached from
//! other sources for the same names.

use std::path::Path;
use std::process::{Command, Stdio};

/// Operating system family, for picking flush commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// macOS.
    Macos,
    /// Linux.
    Linux,
    /// Anything else; nothing is flushed.
    Other,
}

impl Os {
    /// The OS this binary was built for.
    pub const CURRENT: Self = if cfg!(target_os = "macos") {
        Self::Macos
    } else if cfg!(target_os = "linux") {
        Self::Linux
    } else {
        Self::Other
    };
}

/// Where `resolvectl` lives on merged-/usr and older layouts.
const RESOLVECTL: &[&str] = &["/usr/bin/resolvectl", "/bin/resolvectl"];

/// Flush commands for `os`, as absolute argv; `exists` decides optional tools.
///
/// Absolute paths matter because the helper runs as root and must not depend on
/// the caller's `PATH`.
pub fn commands(os: Os, exists: impl Fn(&Path) -> bool) -> Vec<Vec<&'static str>> {
    match os {
        Os::Macos => vec![
            vec!["/usr/bin/dscacheutil", "-flushcache"],
            vec!["/usr/bin/killall", "-HUP", "mDNSResponder"],
        ],
        Os::Linux => RESOLVECTL
            .iter()
            .find(|path| exists(Path::new(path)))
            .map(|path| vec![vec![*path, "flush-caches"]])
            .unwrap_or_default(),
        Os::Other => Vec::new(),
    }
}

/// Runs the flush commands for this OS with an empty environment.
///
/// Returns one warning per failed command. Failures do not fail the update: the
/// file is already written and caches expire on their own.
pub fn flush() -> Vec<String> {
    let mut warnings = Vec::new();
    for argv in commands(Os::CURRENT, Path::exists) {
        let Some((program, args)) = argv.split_first() else {
            continue;
        };
        let status = Command::new(program)
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        match status {
            Ok(status) if status.success() => {}
            Ok(status) => warnings.push(format!("`{}` exited with {status}", argv.join(" "))),
            Err(error) => warnings.push(format!("`{}` failed: {error}", argv.join(" "))),
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn macos_flushes_both_caches() {
        assert_eq!(
            commands(Os::Macos, |_| false),
            [
                vec!["/usr/bin/dscacheutil", "-flushcache"],
                vec!["/usr/bin/killall", "-HUP", "mDNSResponder"],
            ]
        );
    }

    #[test]
    fn linux_flushes_resolved_only_when_present() {
        assert!(commands(Os::Linux, |_| false).is_empty());
        assert_eq!(
            commands(Os::Linux, |path| path == Path::new("/bin/resolvectl")),
            [vec!["/bin/resolvectl", "flush-caches"]]
        );
        assert_eq!(
            commands(Os::Linux, |_| true),
            [vec!["/usr/bin/resolvectl", "flush-caches"]]
        );
        assert!(commands(Os::Other, |_| true).is_empty());
    }
}

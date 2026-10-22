//! Lexical path comparison that follows the host file system's case rules.
//!
//! Identity paths (project roots, Git common directories, worktrees, request
//! cwds) come from Git and from canonicalization, which can disagree on case on
//! Windows. Compare them through these helpers; never fold case for display or
//! storage. No filesystem access happens here.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub use os::{HOME_VAR, path_starts_with, same_path, strip_path_prefix};

/// The user's home directory from a captured client environment, used for `~`
/// in lane references and config paths (`$HOME` on Unix, `%USERPROFILE%` on
/// Windows, where `HOME` is usually unset or set by Git Bash to a POSIX path).
pub fn client_home(env: &BTreeMap<String, String>) -> Option<PathBuf> {
    crate::env::get(env, HOME_VAR).map(PathBuf::from)
}

#[cfg(unix)]
mod os {
    use std::path::{Path, PathBuf};

    /// The client environment variable that holds the user's home directory.
    pub const HOME_VAR: &str = "HOME";

    /// Whether `a` and `b` name the same path (exact on Unix).
    pub fn same_path(a: &Path, b: &Path) -> bool {
        a == b
    }

    /// Whether `path` is `base` or lies below it, by whole components.
    pub fn path_starts_with(path: &Path, base: &Path) -> bool {
        path.starts_with(base)
    }

    /// The rest of `path` below `base`, or `None` when it is not below it.
    pub fn strip_path_prefix(path: &Path, base: &Path) -> Option<PathBuf> {
        path.strip_prefix(base).ok().map(Path::to_path_buf)
    }
}

#[cfg(windows)]
mod os {
    use std::path::{Component, Path, PathBuf};

    /// The client environment variable that holds the user's home directory.
    pub const HOME_VAR: &str = "USERPROFILE";

    /// Whether two components match; NTFS names are case-insensitive, and only
    /// ASCII folding is needed for drive letters and the usual path spellings.
    fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
        a.as_os_str().eq_ignore_ascii_case(b.as_os_str())
    }

    /// Whether `a` and `b` name the same path, ignoring ASCII case and
    /// accepting either separator.
    pub fn same_path(a: &Path, b: &Path) -> bool {
        let (mut a, mut b) = (a.components(), b.components());
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) if same_component(x, y) => {}
                _ => return false,
            }
        }
    }

    /// Whether `path` is `base` or lies below it, by whole components and
    /// ignoring ASCII case.
    pub fn path_starts_with(path: &Path, base: &Path) -> bool {
        strip_path_prefix(path, base).is_some()
    }

    /// The rest of `path` below `base` (ignoring ASCII case), or `None` when it
    /// is not below it.
    pub fn strip_path_prefix(path: &Path, base: &Path) -> Option<PathBuf> {
        let mut rest = path.components();
        for wanted in base.components() {
            match rest.next() {
                Some(found) if same_component(found, wanted) => {}
                _ => return None,
            }
        }
        Some(rest.as_path().to_path_buf())
    }
}

/// Joins `path`'s normal components with `/`, the separator Git and devlanes
/// use in identities such as `worktrees/<name>`, whatever the host separator.
pub fn slash_joined(path: &Path) -> String {
    let parts: Vec<_> = path
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect();
    parts.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Prefixes match by whole components on every OS.
    fn prefixes_match_whole_components() {
        let base = Path::new("work").join("acme");
        assert!(path_starts_with(&base.join("web"), &base));
        assert!(path_starts_with(&base, &base));
        assert!(!path_starts_with(&Path::new("work").join("acme-2"), &base));
        assert_eq!(
            strip_path_prefix(&base.join("apps").join("web"), &base),
            Some(Path::new("apps").join("web"))
        );
        assert_eq!(strip_path_prefix(&base, &base.join("web")), None);
        assert_eq!(
            slash_joined(&Path::new("worktrees").join("lane")),
            "worktrees/lane"
        );
    }

    #[test]
    /// The home comes from the OS's own variable only.
    fn client_home_reads_the_platform_variable() {
        let home = Path::new("acme").join("home");
        let env = BTreeMap::from([(HOME_VAR.to_owned(), home.display().to_string())]);
        assert_eq!(client_home(&env), Some(home));
        assert_eq!(client_home(&BTreeMap::new()), None);
    }

    #[cfg(unix)]
    #[test]
    /// Unix paths compare exactly.
    fn unix_comparison_is_exact() {
        assert!(!same_path(Path::new("/Work/acme"), Path::new("/work/acme")));
        assert!(!path_starts_with(
            Path::new("/Work/acme/web"),
            Path::new("/work/acme")
        ));
    }

    #[cfg(windows)]
    #[test]
    /// Windows paths compare ignoring ASCII case and separator spelling, and the
    /// home ignores a Git Bash `HOME`.
    fn windows_comparison_ignores_case_and_separators() {
        let env = BTreeMap::from([
            ("HOME".to_owned(), "/c/Users/acme".to_owned()),
            ("UserProfile".to_owned(), r"C:\Users\acme".to_owned()),
        ]);
        assert_eq!(client_home(&env), Some(PathBuf::from(r"C:\Users\acme")));
        assert!(same_path(
            Path::new(r"C:\Work\Acme"),
            Path::new("c:/work/acme")
        ));
        assert!(!same_path(
            Path::new(r"C:\work\acme"),
            Path::new(r"D:\work\acme")
        ));
        assert!(path_starts_with(
            Path::new(r"c:\WORK\acme\web"),
            Path::new("C:/work/acme")
        ));
        assert_eq!(
            strip_path_prefix(
                Path::new(r"C:\Work\Acme\apps\web"),
                Path::new(r"c:\work\acme")
            ),
            Some(PathBuf::from(r"apps\web"))
        );
    }
}

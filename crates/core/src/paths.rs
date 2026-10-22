//! Lexical path comparison that follows the host file system's case rules, and
//! the file name devlanes stores a config name under.
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
    use std::path::{Component, Path, PathBuf, Prefix};

    /// The client environment variable that holds the user's home directory.
    pub const HOME_VAR: &str = "USERPROFILE";

    /// Whether two components match; NTFS names are case-insensitive, and only
    /// ASCII folding is needed for drive letters and the usual path spellings.
    /// Prefixes compare by what they name, so the verbatim `\\?\C:` equals `C:`
    /// and `\\?\UNC\server\share` equals `\\server\share`.
    fn same_component(a: Component<'_>, b: Component<'_>) -> bool {
        match (a, b) {
            (Component::Prefix(a), Component::Prefix(b)) => same_prefix(a.kind(), b.kind()),
            (a, b) => a.as_os_str().eq_ignore_ascii_case(b.as_os_str()),
        }
    }

    /// Whether two prefixes name the same drive or share, ignoring ASCII case
    /// and the verbatim form. Other prefixes (`\\.\pipe`, `\\?\Volume{..}`)
    /// match only their own kind with the same text.
    fn same_prefix(a: Prefix<'_>, b: Prefix<'_>) -> bool {
        match (a, b) {
            (
                Prefix::Disk(a) | Prefix::VerbatimDisk(a),
                Prefix::Disk(b) | Prefix::VerbatimDisk(b),
            ) => a.eq_ignore_ascii_case(&b),
            (
                Prefix::UNC(a_server, a_share) | Prefix::VerbatimUNC(a_server, a_share),
                Prefix::UNC(b_server, b_share) | Prefix::VerbatimUNC(b_server, b_share),
            ) => a_server.eq_ignore_ascii_case(b_server) && a_share.eq_ignore_ascii_case(b_share),
            (Prefix::Verbatim(a), Prefix::Verbatim(b))
            | (Prefix::DeviceNS(a), Prefix::DeviceNS(b)) => a.eq_ignore_ascii_case(b),
            _ => false,
        }
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

/// The file or directory name a project, service, task or unit name is stored
/// under (logs, infra state, sync folders), the same on every OS so one config
/// lays out the same tree everywhere. Characters Windows refuses in file names
/// (`< > : " | ? * \ /` and control characters) become `_`, as does a trailing
/// `.` or space, which Windows drops; a reserved device name (`CON`, `aux.log`,
/// `COM1`, `LPT¹`) gets `_` after its stem. Any other name is returned as is, so
/// existing names keep their directories. Two names can map to one result
/// (`db:migrate` and `db_migrate`); config validation rejects such pairs.
///
/// ```
/// use devlanes_core::paths::disk_name;
/// assert_eq!(disk_name("web"), "web");
/// assert_eq!(disk_name("db:migrate"), "db_migrate");
/// assert_eq!(disk_name("apps/web"), "apps_web");
/// assert_eq!(disk_name("aux.2"), "aux_.2");
/// assert_eq!(disk_name("api."), "api_");
/// ```
pub fn disk_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\' | '/') {
                '_'
            } else {
                c
            }
        })
        .collect();
    if out.ends_with(['.', ' ']) {
        out.pop();
        out.push('_');
    }
    // Windows reads the part before the first dot, without trailing spaces, as
    // the device name: `nul.txt` and `con .log` are devices too.
    let stem = out.find('.').unwrap_or(out.len());
    if is_device_name(out[..stem].trim_end_matches(' ')) {
        out.insert(stem, '_');
    }
    out
}

/// Whether `stem` is a DOS device name, ignoring ASCII case: `CON`, `PRN`,
/// `AUX`, `NUL`, or `COM`/`LPT` followed by one digit or a superscript `¹²³`.
fn is_device_name(stem: &str) -> bool {
    let upper = stem.to_ascii_uppercase();
    if matches!(upper.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let Some(number) = upper
        .strip_prefix("COM")
        .or_else(|| upper.strip_prefix("LPT"))
    else {
        return false;
    };
    let mut chars = number.chars();
    matches!(
        (chars.next(), chars.next()),
        (Some('0'..='9' | '¹' | '²' | '³'), None)
    )
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

    #[test]
    /// Ordinary names keep their spelling, so existing directories stay put;
    /// `/` maps to `_` as unit log directories always did.
    fn disk_names_keep_ordinary_names() {
        for name in [
            "web",
            "Web",
            "api.2",
            "db_migrate",
            "a-b",
            "café",
            "console",
            "auxiliary",
            "com10",
            "lpt",
            "nul_",
            ".hidden",
        ] {
            assert_eq!(disk_name(name), name);
        }
        assert_eq!(disk_name("apps/web"), "apps_web");
    }

    #[test]
    /// Characters Windows refuses, control characters and a trailing dot or
    /// space become `_`.
    fn disk_names_replace_what_windows_refuses() {
        for (name, expected) in [
            ("db:migrate", "db_migrate"),
            ("build:css", "build_css"),
            ("a<b>c", "a_b_c"),
            ("say\"hi\"", "say_hi_"),
            ("a|b?c*", "a_b_c_"),
            (r"db\seed", "db_seed"),
            ("a\tb\u{1}", "a_b_"),
            ("a\u{7f}", "a_"),
            ("api.", "api_"),
            ("api ", "api_"),
            ("api..", "api._"),
            (".", "_"),
            ("..", "._"),
            ("", ""),
        ] {
            assert_eq!(disk_name(name), expected, "{name:?}");
        }
    }

    #[test]
    /// Device names get `_` after the stem, in any case, with or without an
    /// extension, including `COM0`/`LPT0` and the superscript digits.
    fn disk_names_escape_device_names() {
        for (name, expected) in [
            ("aux", "aux_"),
            ("AUX", "AUX_"),
            ("Con", "Con_"),
            ("prn", "prn_"),
            ("nul.txt", "nul_.txt"),
            ("aux.2", "aux_.2"),
            ("con.tar.gz", "con_.tar.gz"),
            ("con .log", "con _.log"),
            ("com0", "com0_"),
            ("COM9.log", "COM9_.log"),
            ("lpt1", "lpt1_"),
            ("LPT¹", "LPT¹_"),
            ("com²", "com²_"),
            ("lpt³.x", "lpt³_.x"),
            ("aux.", "aux_"),
            ("nul:", "nul_"),
        ] {
            assert_eq!(disk_name(name), expected, "{name:?}");
        }
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

    #[cfg(windows)]
    #[test]
    /// Verbatim and plain prefixes naming the same drive or share are equal,
    /// ignoring case and separator spelling; other drives and shares are not.
    fn windows_prefixes_compare_by_drive_and_share() {
        for (a, b) in [
            (r"\\?\C:\Work\acme", r"c:\work\ACME"),
            (r"\\?\c:\work", "C:/work"),
            (r"\\Server\Share\acme", r"\\server\share\acme"),
            (r"\\server\share/acme\web", "//SERVER/share/acme/web"),
            (r"\\?\UNC\server\share\acme", r"\\SERVER\Share\acme"),
        ] {
            assert!(same_path(Path::new(a), Path::new(b)), "{a} vs {b}");
        }
        for (a, b) in [
            (r"\\?\C:\work", r"D:\work"),
            (r"\\server\share\acme", r"\\server\other\acme"),
            (r"\\server\share\acme", r"\\other\share\acme"),
            (r"\\server\share\acme", r"C:\share\acme"),
        ] {
            assert!(!same_path(Path::new(a), Path::new(b)), "{a} vs {b}");
        }
        assert_eq!(
            strip_path_prefix(
                Path::new(r"\\?\C:\Work\acme\apps\web"),
                Path::new(r"c:\work\acme")
            ),
            Some(PathBuf::from(r"apps\web"))
        );
        assert!(path_starts_with(
            Path::new(r"//server/share/acme/web"),
            Path::new(r"\\?\UNC\Server\Share\acme")
        ));
    }
}

//! Paths under the user's profile directory shown as `~\…` on Windows. Unix
//! shortens with plain prefix checks against `HOME` where the paths are
//! drawn; Windows ignores `HOME` (Git Bash sets it to `/c/Users/acme`, which
//! never matches the native paths the daemon reports) and compares paths
//! ignoring case and separator spelling.

use std::path::{Path, PathBuf};

use crate::Options;

/// The profile directory: `USERPROFILE` from [`Options::env`] when tests pass
/// their own environment, else the one Windows reports.
pub fn user_home(options: &Options) -> Option<PathBuf> {
    match &options.env {
        Some(env) => env.get("USERPROFILE").map(PathBuf::from),
        None => devlanes_client::user_home(),
    }
}

/// `path` as `~\rest` when it lies under `home`, else `None`.
pub fn shorten(home: &Path, path: &Path) -> Option<String> {
    if home.as_os_str().is_empty() {
        return None;
    }
    let rest = devlanes_core::paths::strip_path_prefix(path, home)?;
    Some(if rest.as_os_str().is_empty() {
        "~".to_owned()
    } else {
        format!("~\\{}", rest.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_under_the_profile_directory_start_with_a_tilde() {
        let home = Path::new(r"C:\Users\Acme");
        for (path, shown) in [
            (
                r"C:\Users\Acme\.devlanes\state.db",
                Some(r"~\.devlanes\state.db"),
            ),
            (r"c:/users/acme/.devlanes", Some(r"~\.devlanes")),
            (r"\\?\C:\Users\ACME\.devlanes", Some(r"~\.devlanes")),
            (r"C:\Users\Acme", Some("~")),
            (r"C:\Users\Acme-2\.devlanes", None),
            (r"D:\Users\Acme\.devlanes", None),
            (r"\\.\pipe\devlanes-0123", None),
        ] {
            assert_eq!(shorten(home, Path::new(path)).as_deref(), shown, "{path}");
        }
        assert_eq!(shorten(Path::new(""), Path::new(r"C:\x")), None);
    }

    #[test]
    fn tests_choose_the_profile_directory() {
        let options = Options {
            env: Some([("USERPROFILE".to_owned(), r"C:\Users\Acme".to_owned())].into()),
            ..Options::default()
        };
        assert_eq!(user_home(&options), Some(PathBuf::from(r"C:\Users\Acme")));
        let hermetic = Options {
            env: Some(std::collections::BTreeMap::new()),
            ..Options::default()
        };
        assert_eq!(user_home(&hermetic), None);
    }
}

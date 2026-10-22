//! Paths a [`LocalTarget`](crate::LocalTarget) on this OS cannot hold. A box
//! sends Unix names; on Windows some of them are not valid file names, and
//! names that differ only in letter case would land in one file there.

pub(crate) use os::unusable_name;

/// Tracks the spelling of every file a destination holds, so that a second file
/// whose path differs only in letter case is refused instead of overwriting the
/// first. Windows file systems ignore case; on Unix such paths are distinct
/// files and every check passes.
///
/// Feed it the destination's files with [`CaseCollisions::hold`], ask
/// [`CaseCollisions::admit`] before writing a file and
/// [`CaseCollisions::release`] before removing one.
#[derive(Debug, Default)]
pub struct CaseCollisions {
    /// Held files by their case-folded path, with the spelling that holds the slot.
    #[cfg(windows)]
    held: std::collections::HashMap<String, crate::RelPath>,
}

#[cfg(unix)]
mod os {
    use super::CaseCollisions;
    use crate::RelPath;

    /// Every relative path is a valid name on Unix.
    pub(crate) fn unusable_name(_path: &RelPath) -> bool {
        false
    }

    impl CaseCollisions {
        /// Tracks nothing: Unix keeps paths that differ in case apart.
        pub fn new() -> Self {
            Self::default()
        }

        /// Nothing to record.
        pub fn hold(&mut self, _path: &RelPath) {}

        /// Always admits.
        pub fn admit(&mut self, _path: &RelPath) -> Result<(), RelPath> {
            Ok(())
        }

        /// Every removal applies to its own file.
        pub fn release(&mut self, _path: &RelPath) -> bool {
            true
        }
    }
}

#[cfg(windows)]
mod os {
    use devlanes_core::paths::is_portable_name;

    use super::CaseCollisions;
    use crate::RelPath;

    /// Whether a component of `path` is not a valid Windows name: a device name
    /// (`aux.js`), a trailing dot or space (which Windows drops, so `a.` would
    /// write `a`), or a character such as `:` (which would write an alternate
    /// data stream) or `\` (which would nest directories).
    pub(crate) fn unusable_name(path: &RelPath) -> bool {
        !path.as_str().split('/').all(is_portable_name)
    }

    /// `path` as the file system compares it: each character upper-cased when
    /// that gives one character, like NTFS's case table (so `ß` stays `ß`).
    fn fold(path: &RelPath) -> String {
        path.as_str()
            .chars()
            .map(|c| {
                let mut upper = c.to_uppercase();
                match (upper.next(), upper.next()) {
                    (Some(single), None) => single,
                    _ => c,
                }
            })
            .collect()
    }

    impl CaseCollisions {
        /// An empty destination.
        pub fn new() -> Self {
            Self::default()
        }

        /// Records a file the destination holds under this spelling.
        pub fn hold(&mut self, path: &RelPath) {
            self.held.insert(fold(path), path.clone());
        }

        /// Admits a file to write and records it, or returns the held path it
        /// would overwrite because the two differ only in letter case.
        pub fn admit(&mut self, path: &RelPath) -> Result<(), RelPath> {
            match self.held.get(&fold(path)) {
                Some(held) if held != path => Err(held.clone()),
                Some(_) => Ok(()),
                None => {
                    self.hold(path);
                    Ok(())
                }
            }
        }

        /// Whether removing `path` touches its own file, forgetting it if so.
        /// `false` when another spelling holds the slot: `path` was refused
        /// earlier, and removing it would delete the other file.
        pub fn release(&mut self, path: &RelPath) -> bool {
            let key = fold(path);
            match self.held.get(&key) {
                Some(held) if held != path => false,
                _ => {
                    self.held.remove(&key);
                    true
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RelPath;

    fn rel(path: &str) -> RelPath {
        RelPath::new(path).expect("path")
    }

    /// Paths that differ only in case are one file on Windows: the second is
    /// refused, its removal leaves the first alone, and once the first is gone
    /// the other spelling is admitted. On Unix they are distinct files.
    #[test]
    fn case_collisions_follow_the_file_system() {
        let mut names = CaseCollisions::new();
        names.hold(&rel("dist/README.md"));
        assert_eq!(names.admit(&rel("dist/README.md")), Ok(()));
        let other = names.admit(&rel("dist/readme.md"));
        let removal = names.release(&rel("dist/readme.md"));
        #[cfg(windows)]
        {
            assert_eq!(other, Err(rel("dist/README.md")));
            assert!(
                !removal,
                "the refused spelling must not delete the held file"
            );
            assert!(names.release(&rel("dist/README.md")));
            assert_eq!(names.admit(&rel("dist/readme.md")), Ok(()));
            // NTFS does not fold `ß` into `SS`.
            assert_eq!(names.admit(&rel("STRASSE")), Ok(()));
            assert_eq!(names.admit(&rel("straße")), Ok(()));
            assert_eq!(names.admit(&rel("Straße")), Err(rel("straße")));
        }
        #[cfg(unix)]
        {
            assert_eq!(other, Ok(()));
            assert!(removal);
        }
    }

    /// Device names, trailing dots and spaces, and characters Windows refuses
    /// are unusable there, in any component; Unix takes every name.
    #[test]
    fn unusable_names_follow_the_os() {
        let bad = [
            "aux.js",
            "dist/CON",
            "dist/com1.txt/x",
            "notes.",
            "a /b",
            "a:b",
            "q?",
            r"a\b",
            "tab\there",
        ];
        for path in bad {
            #[cfg(windows)]
            assert!(unusable_name(&rel(path)), "{path:?}");
            #[cfg(unix)]
            assert!(!unusable_name(&rel(path)), "{path:?}");
        }
        for good in ["index.html", "a/b.c", ".hidden", "auxiliary.js", "con_.x"] {
            assert!(!unusable_name(&rel(good)), "{good:?}");
        }
    }
}

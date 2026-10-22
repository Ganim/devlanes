//! Paths a [`LocalTarget`](crate::LocalTarget) on this OS cannot hold. A box
//! sends Unix names; on Windows some of them are not valid file names, and
//! names that differ only in letter case would land in one file there.

pub(crate) use os::{stored_elsewhere, unusable_name};

/// `name` as Windows file systems compare names: each character upper-cased
/// when that gives one character, like NTFS's case table (so `ß` stays `ß`,
/// where a full upper-casing would give `SS`). Used on every OS so that tests
/// and callers agree on one folding.
pub fn fold_case(name: &str) -> String {
    name.chars()
        .map(|c| {
            let mut upper = c.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => single,
                _ => c,
            }
        })
        .collect()
}

/// Tracks the spelling of every file and folder a destination holds, so that a
/// box path that differs from a held one only in letter case is refused
/// instead of overwriting it. Windows file systems ignore case; on Unix such
/// paths are distinct and every check passes.
///
/// Feed it the destination's files with [`CaseCollisions::hold`] and call
/// [`CaseCollisions::end_listing`] once all of them went through. Ask
/// [`CaseCollisions::admit`] before writing a file, [`CaseCollisions::release`]
/// before removing one, and release a newly admitted path again when its write
/// fails. Keep one value per destination across connections and call
/// [`CaseCollisions::forget_files`] before each new listing: the spellings the
/// box used are remembered, so a file stored as `Dist/b.js` is reported back
/// to a box that calls it `dist/b.js` under the box's name and is not deleted
/// and sent again.
#[derive(Debug, Default)]
pub struct CaseCollisions {
    /// Held files by their case-folded path, with the spelling that holds the slot.
    #[cfg(windows)]
    held: std::collections::HashMap<String, crate::RelPath>,
    /// Folders of held files by case-folded path, with the number of held
    /// files below them per spelling of the folder.
    #[cfg(windows)]
    dirs: std::collections::HashMap<String, std::collections::BTreeMap<String, usize>>,
    /// The box's spelling of every file it sent, by case-folded path; kept by
    /// [`CaseCollisions::forget_files`].
    #[cfg(windows)]
    spellings: std::collections::HashMap<String, crate::RelPath>,
}

#[cfg(unix)]
mod os {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;

    use super::CaseCollisions;
    use crate::RelPath;

    /// Every relative path is a valid name on Unix.
    pub(crate) fn unusable_name(_path: &RelPath) -> bool {
        false
    }

    /// A Unix path reaches only the entry of that exact name.
    pub(crate) fn stored_elsewhere(_dest: &Path) -> io::Result<Option<OsString>> {
        Ok(None)
    }

    impl CaseCollisions {
        /// Tracks nothing: Unix keeps paths that differ in case apart.
        pub fn new() -> Self {
            Self::default()
        }

        /// Nothing to record; the path is reported as it is.
        pub fn hold(&mut self, path: &RelPath) -> RelPath {
            path.clone()
        }

        /// Always admits, as a new path.
        pub fn admit(&mut self, _path: &RelPath) -> Result<bool, RelPath> {
            Ok(true)
        }

        /// Every removal applies to its own file.
        pub fn release(&mut self, _path: &RelPath) -> bool {
            true
        }

        /// Nothing is held.
        pub fn forget_files(&mut self) {}

        /// No spellings are kept.
        pub fn end_listing(&mut self) {}
    }
}

#[cfg(windows)]
mod os {
    use std::ffi::OsString;
    use std::io;
    use std::path::Path;

    use devlanes_core::paths::is_portable_name;

    use super::{CaseCollisions, fold_case};
    use crate::RelPath;

    /// Whether a component of `path` is not a valid Windows name: a device name
    /// (`aux.js`), a trailing dot or space (which Windows drops, so `a.` would
    /// write `a`), or a character such as `:` (which would write an alternate
    /// data stream) or `\` (which would nest directories).
    pub(crate) fn unusable_name(path: &RelPath) -> bool {
        !path.as_str().split('/').all(is_portable_name)
    }

    /// The stored name of the entry `dest` reaches when it differs from
    /// `dest`'s last component: an 8.3 short name (`README~1.MD` reaching
    /// `readme-long.md`) or another case. Writing or removing `dest` would
    /// then act on that other file. `None` when nothing is there or the names
    /// match.
    pub(crate) fn stored_elsewhere(dest: &Path) -> io::Result<Option<OsString>> {
        let stored = devlanes_sys::windows::names::stored_name(dest)?;
        Ok(stored.filter(|stored| Some(stored.as_os_str()) != dest.file_name()))
    }

    impl CaseCollisions {
        /// An empty destination.
        pub fn new() -> Self {
            Self::default()
        }

        /// Records a file the destination holds and returns the spelling to
        /// report for it: the box's own when it sent this file before, else
        /// the stored one.
        pub fn hold(&mut self, path: &RelPath) -> RelPath {
            let key = fold_case(path.as_str());
            let spelled = self
                .spellings
                .get(&key)
                .cloned()
                .unwrap_or_else(|| path.clone());
            self.insert(key, spelled.clone());
            spelled
        }

        /// Admits a file to write and records it, or returns the held path it
        /// would overwrite: a file whose path differs only in letter case, or a
        /// file standing where this path needs a folder (or the reverse) under
        /// another spelling. `Ok(true)` when the path was not held before, so
        /// a failed write has to [`Self::release`] it again.
        pub fn admit(&mut self, path: &RelPath) -> Result<bool, RelPath> {
            let key = fold_case(path.as_str());
            let new = match self.held.get(&key) {
                Some(held) if held != path => return Err(held.clone()),
                Some(_) => false,
                None => {
                    // The file replaces the folder at its path with everything
                    // below it, which box folders in other cases share.
                    if let Some(spellings) = self.dirs.get(&key)
                        && let Some(other) = spellings.keys().find(|&other| other != path.as_str())
                    {
                        return Err(RelPath::new(other.clone()).unwrap_or_else(|_| path.clone()));
                    }
                    for folder in path.ancestors() {
                        if let Some(held) = self.held.get(&fold_case(folder))
                            && held.as_str() != folder
                        {
                            return Err(held.clone());
                        }
                    }
                    self.insert(key.clone(), path.clone());
                    true
                }
            };
            self.spellings.insert(key, path.clone());
            Ok(new)
        }

        /// Whether removing `path` touches its own file, forgetting it if so.
        /// `false` when another spelling holds the slot: `path` was refused
        /// earlier, and removing it would delete the other file.
        pub fn release(&mut self, path: &RelPath) -> bool {
            let key = fold_case(path.as_str());
            match self.held.get(&key) {
                Some(held) if held != path => false,
                Some(_) => {
                    self.held.remove(&key);
                    for folder in path.ancestors() {
                        let folded = fold_case(folder);
                        let Some(spellings) = self.dirs.get_mut(&folded) else {
                            continue;
                        };
                        if let Some(count) = spellings.get_mut(folder) {
                            *count -= 1;
                            if *count == 0 {
                                spellings.remove(folder);
                            }
                        }
                        if spellings.is_empty() {
                            self.dirs.remove(&folded);
                        }
                    }
                    self.spellings.remove(&key);
                    true
                }
                None => {
                    self.spellings.remove(&key);
                    true
                }
            }
        }

        /// Forgets the held files and folders before the destination is
        /// listed again, keeping the box's spellings for [`Self::hold`].
        pub fn forget_files(&mut self) {
            self.held.clear();
            self.dirs.clear();
        }

        /// Drops the spellings of files the listing just fed to [`Self::hold`]
        /// did not report: they are gone from the destination. Call it only
        /// after a complete listing.
        pub fn end_listing(&mut self) {
            let held = &self.held;
            self.spellings.retain(|key, _| held.contains_key(key));
        }

        /// Holds `path` under `key` with its folders. A slot held already
        /// stays with its holder; a second file of one key is only listed from
        /// a case-sensitive folder.
        fn insert(&mut self, key: String, path: RelPath) {
            if self.held.contains_key(&key) {
                return;
            }
            self.held.insert(key, path.clone());
            for folder in path.ancestors() {
                *self
                    .dirs
                    .entry(fold_case(folder))
                    .or_default()
                    .entry(folder.to_owned())
                    .or_default() += 1;
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
        assert_eq!(names.hold(&rel("dist/README.md")), rel("dist/README.md"));
        let again = names.admit(&rel("dist/README.md"));
        let other = names.admit(&rel("dist/readme.md"));
        let removal = names.release(&rel("dist/readme.md"));
        #[cfg(windows)]
        {
            assert_eq!(again, Ok(false), "a held file is not new");
            assert_eq!(other, Err(rel("dist/README.md")));
            assert!(
                !removal,
                "the refused spelling must not delete the held file"
            );
            assert!(names.release(&rel("dist/README.md")));
            assert_eq!(names.admit(&rel("dist/readme.md")), Ok(true));
            // NTFS does not fold `ß` into `SS`.
            assert_eq!(names.admit(&rel("STRASSE")), Ok(true));
            assert_eq!(names.admit(&rel("straße")), Ok(true));
            assert_eq!(names.admit(&rel("Straße")), Err(rel("straße")));
        }
        #[cfg(unix)]
        {
            assert_eq!(again, Ok(true));
            assert_eq!(other, Ok(true));
            assert!(removal);
        }
    }

    /// A box file `Dist` next to a box folder `dist/` would be one entry on
    /// Windows: whichever comes second is refused, in either order. The same
    /// spelling is a file turning into a folder and passes. Unix keeps both.
    #[test]
    fn files_and_folders_that_differ_in_case_collide() {
        let mut names = CaseCollisions::new();
        names.hold(&rel("Dist"));
        let folder = names.admit(&rel("dist/x.js"));
        let mut reverse = CaseCollisions::new();
        assert_eq!(reverse.admit(&rel("Dist/x.js")), Ok(true));
        let file = reverse.admit(&rel("dist"));
        let mut same = CaseCollisions::new();
        same.hold(&rel("a"));
        assert_eq!(same.admit(&rel("a/b")), Ok(true));
        #[cfg(windows)]
        {
            assert_eq!(folder, Err(rel("Dist")));
            assert_eq!(file, Err(rel("Dist")));
            // Once the box removes its file, the folder is free.
            assert!(names.release(&rel("Dist")));
            assert_eq!(names.admit(&rel("dist/x.js")), Ok(true));
            // And once the folder's last file goes, so is the file name.
            assert!(reverse.release(&rel("Dist/x.js")));
            assert_eq!(reverse.admit(&rel("dist")), Ok(true));
        }
        #[cfg(unix)]
        {
            assert_eq!(folder, Ok(true));
            assert_eq!(file, Ok(true));
        }
    }

    /// Box folders `Dist/` and `dist/` share one local folder, so a box file
    /// `Dist` would replace the files of both: it is refused while `dist/`
    /// holds one, and admitted once every file below uses its spelling.
    #[test]
    fn a_file_replaces_a_shared_folder_only_when_it_is_all_its_own() {
        let mut names = CaseCollisions::new();
        assert_eq!(names.admit(&rel("Dist/a.js")), Ok(true));
        assert_eq!(names.admit(&rel("dist/b.js")), Ok(true));
        let merged = names.admit(&rel("Dist"));
        #[cfg(windows)]
        {
            assert_eq!(merged, Err(rel("dist")));
            assert!(names.release(&rel("dist/b.js")));
            assert_eq!(names.admit(&rel("Dist")), Ok(true));
        }
        #[cfg(unix)]
        assert_eq!(merged, Ok(true));
    }

    /// Box folders `Dist/` and `dist/` share one local folder. After the box
    /// sent `dist/b.js`, the next listing reports the stored `Dist/b.js` under
    /// the box's spelling, so the box neither deletes nor resends it.
    #[test]
    fn listings_report_files_as_the_box_spelled_them() {
        let mut names = CaseCollisions::new();
        assert_eq!(names.admit(&rel("Dist/a.js")), Ok(true));
        assert_eq!(names.admit(&rel("dist/b.js")), Ok(true));
        names.forget_files();
        assert_eq!(names.hold(&rel("Dist/a.js")), rel("Dist/a.js"));
        let reported = names.hold(&rel("Dist/b.js"));
        names.end_listing();
        let again = names.admit(&rel("dist/b.js"));
        #[cfg(windows)]
        {
            assert_eq!(reported, rel("dist/b.js"));
            assert_eq!(again, Ok(false));
        }
        #[cfg(unix)]
        {
            assert_eq!(reported, rel("Dist/b.js"));
            assert_eq!(again, Ok(true));
        }
        // A file deleted on the box loses its spelling with it.
        assert!(names.release(&rel("dist/b.js")));
        names.forget_files();
        assert_eq!(names.hold(&rel("Dist/b.js")), rel("Dist/b.js"));
    }

    /// A file that a complete listing no longer reports (deleted here) loses
    /// the box's spelling, so a later file at that path keeps its own.
    #[test]
    fn listings_drop_spellings_of_files_that_are_gone() {
        let mut names = CaseCollisions::new();
        assert_eq!(names.admit(&rel("dist/b.js")), Ok(true));
        names.forget_files();
        names.end_listing();
        names.forget_files();
        assert_eq!(names.hold(&rel("Dist/b.js")), rel("Dist/b.js"));
    }

    /// One-to-one upper-casing, as NTFS compares names.
    #[test]
    fn folding_keeps_one_character_per_character() {
        assert_eq!(fold_case("dist/Straße.js"), "DIST/STRAßE.JS");
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

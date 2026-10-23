//! Destinations whose file system ignores case (macOS volumes by default). A
//! source on a case-sensitive file system can hold names that differ only by
//! case, such as `README.md` and `Readme.md`, which map to one path there.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt as _;

use crate::target::{TEMP_PREFIX, blocking};
use crate::{LocalTarget, RelPath, SyncError};

impl LocalTarget {
    /// Whether the destination's file system treats names that differ only by
    /// case as one. It is probed with a temp file because the OS does not decide
    /// it: macOS can have case-sensitive volumes and Linux case-folding
    /// directories. Creates the root when it is missing.
    pub async fn case_insensitive(&self) -> Result<bool, SyncError> {
        let root = self.root().to_path_buf();
        blocking(format!("probing {}", root.display()), move || {
            fs::create_dir_all(&root)?;
            let probe = tempfile::Builder::new()
                .prefix(&format!("{TEMP_PREFIX}case-"))
                .suffix(".tmp")
                .tempfile_in(&root)?;
            let name = probe
                .path()
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default();
            let swapped: String = name
                .chars()
                .map(|c| {
                    if c.is_ascii_lowercase() {
                        c.to_ascii_uppercase()
                    } else {
                        c.to_ascii_lowercase()
                    }
                })
                .collect();
            let held = probe.as_file().metadata()?;
            // The same inode, so an unrelated file with that spelling cannot pass.
            Ok(fs::symlink_metadata(root.join(swapped))
                .is_ok_and(|meta| meta.dev() == held.dev() && meta.ino() == held.ino()))
        })
        .await
    }
}

/// The spellings a case-insensitive destination holds, so a second spelling of
/// a held name is skipped instead of overwriting the first, and removing it
/// never deletes the first. The first spelling seen wins.
///
/// Names are compared by their lowercase form, which matches APFS for the
/// names build tools produce; Unicode normalization differences are not folded.
#[derive(Debug, Default)]
pub struct CaseNames {
    files: HashSet<String>,
    /// Lowercase file or parent directory path to its held spelling and the
    /// number of held files at or below it.
    names: HashMap<String, (String, usize)>,
}

impl CaseNames {
    /// The held spelling that `path`, or one of its parent directories, differs
    /// from only by case; `None` when `path` can be written or removed.
    pub fn conflict(&self, path: &RelPath) -> Option<&str> {
        prefixes(path).find_map(|prefix| {
            self.names
                .get(&prefix.to_lowercase())
                .map(|(spelling, _)| spelling.as_str())
                .filter(|spelling| *spelling != prefix)
        })
    }

    /// Records that the destination holds the file `path`. Does nothing when it
    /// is already recorded or conflicts with a held spelling.
    pub fn insert(&mut self, path: &RelPath) {
        if self.conflict(path).is_some() || !self.files.insert(path.as_str().to_owned()) {
            return;
        }
        for prefix in prefixes(path) {
            self.names
                .entry(prefix.to_lowercase())
                .or_insert_with(|| (prefix.to_owned(), 0))
                .1 += 1;
        }
    }

    /// Forgets the file `path` after the destination removed it.
    pub fn remove(&mut self, path: &RelPath) {
        if !self.files.remove(path.as_str()) {
            return;
        }
        for prefix in prefixes(path) {
            let key = prefix.to_lowercase();
            if let Some((_, count)) = self.names.get_mut(&key) {
                *count -= 1;
                if *count == 0 {
                    self.names.remove(&key);
                }
            }
        }
    }
}

/// Parent directories of `path`, shortest first, then `path` itself.
fn prefixes(path: &RelPath) -> impl Iterator<Item = &str> {
    path.ancestors().chain(std::iter::once(path.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(path: &str) -> RelPath {
        RelPath::new(path).expect("path")
    }

    #[test]
    fn first_spelling_wins_for_files_and_directories() {
        let mut names = CaseNames::default();
        names.insert(&rel("README.md"));
        names.insert(&rel("build"));
        names.insert(&rel("src/App.ts"));
        assert_eq!(names.conflict(&rel("README.md")), None);
        assert_eq!(names.conflict(&rel("Readme.md")), Some("README.md"));
        // A folder `Build` would replace the file `build`.
        assert_eq!(names.conflict(&rel("Build/index.js")), Some("build"));
        assert_eq!(names.conflict(&rel("Src/main.ts")), Some("src"));
        assert_eq!(names.conflict(&rel("src/main.ts")), None);
        // A skipped spelling is never recorded.
        names.insert(&rel("Readme.md"));
        names.remove(&rel("Readme.md"));
        assert_eq!(names.conflict(&rel("Readme.md")), Some("README.md"));
    }

    #[test]
    fn removing_the_held_spelling_frees_the_name() {
        let mut names = CaseNames::default();
        names.insert(&rel("src/a.ts"));
        names.insert(&rel("src/b.ts"));
        names.insert(&rel("src/a.ts"));
        names.remove(&rel("src/a.ts"));
        assert_eq!(names.conflict(&rel("Src/c.ts")), Some("src"));
        names.remove(&rel("src/b.ts"));
        assert_eq!(names.conflict(&rel("Src/c.ts")), None);
    }

    /// The probe agrees with what the temp directory's file system does, and
    /// leaves nothing behind. On a default macOS volume it reports true.
    #[tokio::test]
    async fn probe_matches_the_file_system() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("probe-a"), b"").expect("write");
        let expected = dir.path().join("PROBE-A").exists();
        fs::remove_file(dir.path().join("probe-a")).expect("remove");
        let target = LocalTarget::new(dir.path().join("out"));
        assert_eq!(target.case_insensitive().await.expect("probe"), expected);
        assert!(target.files().await.expect("files").is_empty());
        assert_eq!(
            fs::read_dir(dir.path().join("out")).expect("read").count(),
            0
        );
        #[cfg(target_os = "linux")]
        assert!(!expected, "Linux temp directories are case-sensitive");
    }
}

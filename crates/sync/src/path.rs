use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SyncError;

/// A validated relative path inside a sync root, `/`-separated.
///
/// Paths from a remote source are untrusted, so every component must be a plain
/// name: no empty, `.` or `..` components, no leading `/`, no NUL. Joining a
/// `RelPath` to a root therefore never leaves the root lexically; targets still
/// guard against symlinked directories.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    /// Validates a `/`-separated relative path.
    pub fn new(path: impl Into<String>) -> Result<Self, SyncError> {
        let path = path.into();
        let invalid = |reason| SyncError::InvalidPath {
            path: path.clone(),
            reason,
        };
        if path.is_empty() {
            return Err(invalid("it is empty"));
        }
        if path.contains('\0') {
            return Err(invalid("it contains NUL"));
        }
        if path.starts_with('/') {
            return Err(invalid("it is absolute"));
        }
        if path
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        {
            return Err(invalid("it has an empty, `.` or `..` component"));
        }
        Ok(Self(path))
    }

    /// Converts a filesystem path relative to a root; only plain UTF-8 components pass.
    pub fn from_path(path: &Path) -> Result<Self, SyncError> {
        let mut parts = Vec::new();
        for component in path.components() {
            let Component::Normal(part) = component else {
                return Err(SyncError::InvalidPath {
                    path: path.display().to_string(),
                    reason: "it is not a plain relative path",
                });
            };
            let part = part.to_str().ok_or_else(|| SyncError::InvalidPath {
                path: path.display().to_string(),
                reason: "it is not valid UTF-8",
            })?;
            parts.push(part);
        }
        Self::new(parts.join("/"))
    }

    /// The path as a `/`-separated string.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Joins the path to `root`.
    pub fn to_path(&self, root: &Path) -> PathBuf {
        let mut path = root.to_path_buf();
        path.extend(self.0.split('/'));
        path
    }

    /// Whether `self` is `dir` or lies below it.
    pub fn is_within(&self, dir: &Self) -> bool {
        self.0 == dir.0
            || (self.0.len() > dir.0.len()
                && self.0.starts_with(&dir.0)
                && self.0.as_bytes()[dir.0.len()] == b'/')
    }

    /// Names of the parent directories, outermost first: `a`, `b` for `a/b/c`.
    pub fn parent_names(&self) -> impl Iterator<Item = &str> {
        let mut parts: Vec<&str> = self.0.split('/').collect();
        parts.pop();
        parts.into_iter()
    }

    /// Strict ancestors, shortest first: `a`, `a/b` for `a/b/c`.
    pub fn ancestors(&self) -> impl Iterator<Item = &str> {
        self.0.match_indices('/').map(|(index, _)| &self.0[..index])
    }
}

impl fmt::Debug for RelPath {
    /// Debug-quotes the inner string.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for RelPath {
    /// Writes the `/`-separated path.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for RelPath {
    type Error = SyncError;

    /// Validates like [`RelPath::new`].
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<RelPath> for String {
    /// Unwraps the inner string.
    fn from(value: RelPath) -> Self {
        value.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_escaping_paths() {
        for bad in [
            "",
            "/etc/passwd",
            "a/../b",
            "..",
            "./a",
            "a//b",
            "a/",
            "a\0b",
        ] {
            assert!(RelPath::new(bad).is_err(), "{bad:?} must be rejected");
        }
        assert!(RelPath::from_path(Path::new("../a")).is_err());
        assert!(RelPath::from_path(Path::new("/a")).is_err());
        assert_eq!(
            RelPath::from_path(Path::new("a/b.txt"))
                .expect("valid")
                .as_str(),
            "a/b.txt"
        );
        // Deserialization goes through `TryFrom<String>`, so it validates too.
        assert!(RelPath::try_from("../x".to_owned()).is_err());
    }

    #[test]
    fn prefix_and_ancestors() {
        let path = RelPath::new("a/b/c").expect("valid");
        let ab = RelPath::new("a/b").expect("valid");
        assert!(path.is_within(&ab));
        assert!(ab.is_within(&ab));
        assert!(!RelPath::new("a/bc").expect("valid").is_within(&ab));
        assert_eq!(path.ancestors().collect::<Vec<_>>(), ["a", "a/b"]);
        assert_eq!(
            path.to_path(Path::new("/root")),
            PathBuf::from("/root/a/b/c")
        );
    }
}

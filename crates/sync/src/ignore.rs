use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use crate::{RelPath, SyncError};

/// The `ignore` globs of one sync entry.
///
/// A pattern without `/` matches a name at any depth (`*.map`, `node_modules`); a
/// pattern containing `/`, including a leading one, is anchored at the source root
/// (`/cache`, `assets/cache/**`), as in `.gitignore`. A trailing `/` is dropped, so
/// it also matches files. `*` never crosses `/`; `**` does. A path is ignored
/// when it or any ancestor matches, so ignoring a directory ignores its contents.
#[derive(Debug, Clone)]
pub struct IgnoreRules {
    set: GlobSet,
}

impl IgnoreRules {
    /// Compiles `patterns`.
    pub fn new<I, S>(patterns: I) -> Result<Self, SyncError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut builder = GlobSetBuilder::new();
        for raw in patterns {
            let raw = raw.as_ref();
            let pattern = raw.trim_start_matches('/').trim_end_matches('/');
            if pattern.is_empty() {
                continue;
            }
            let anchored = raw.trim_end_matches('/').contains('/');
            let full = if anchored {
                pattern.to_owned()
            } else {
                format!("**/{pattern}")
            };
            let glob = GlobBuilder::new(&full)
                .literal_separator(true)
                .build()
                .map_err(|source| SyncError::InvalidIgnore {
                    pattern: raw.to_owned(),
                    source,
                })?;
            builder.add(glob);
        }
        let set = builder.build().map_err(|source| SyncError::InvalidIgnore {
            pattern: String::new(),
            source,
        })?;
        Ok(Self { set })
    }

    /// Rules that ignore nothing.
    pub fn none() -> Self {
        Self {
            set: GlobSet::empty(),
        }
    }

    /// Whether `path` or one of its ancestors matches.
    pub fn is_ignored(&self, path: &RelPath) -> bool {
        !self.set.is_empty()
            && (path.ancestors().any(|ancestor| self.set.is_match(ancestor))
                || self.set.is_match(path.as_str()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignored(rules: &IgnoreRules, path: &str) -> bool {
        rules.is_ignored(&RelPath::new(path).expect("path"))
    }

    #[test]
    fn basename_patterns_match_at_any_depth() {
        let rules = IgnoreRules::new(["*.map", "node_modules/"]).expect("rules");
        assert!(ignored(&rules, "app.js.map"));
        assert!(ignored(&rules, "assets/js/app.js.map"));
        assert!(!ignored(&rules, "app.js"));
        assert!(ignored(&rules, "node_modules"));
        assert!(ignored(&rules, "a/node_modules/x/index.js"));
    }

    #[test]
    fn slash_patterns_are_anchored() {
        let rules = IgnoreRules::new(["/cache", "assets/tmp", "logs/**/*.log"]).expect("rules");
        assert!(ignored(&rules, "cache/x"));
        assert!(!ignored(&rules, "x/cache"));
        assert!(ignored(&rules, "assets/tmp/a.png"));
        assert!(!ignored(&rules, "other/assets/tmp/a.png"));
        assert!(ignored(&rules, "logs/a/b/c.log"));
        assert!(!ignored(&rules, "logs/a/b/c.txt"));
        assert!(!ignored(&rules, "assets/tmpfile"));
    }

    #[test]
    fn invalid_and_empty_patterns() {
        assert!(matches!(
            IgnoreRules::new(["a[b"]),
            Err(SyncError::InvalidIgnore { pattern, .. }) if pattern == "a[b"
        ));
        let rules = IgnoreRules::new(["", "/"]).expect("rules");
        assert!(!ignored(&rules, "anything"));
        assert!(!ignored(&IgnoreRules::none(), "x"));
    }
}

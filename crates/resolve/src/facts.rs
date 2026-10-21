use std::path::PathBuf;

/// What the daemon knows about one lane before resolving its config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneFacts {
    /// `project` from the config.
    pub project: String,
    /// Worktree of lane 0 (`project.primary_path`).
    pub primary_path: PathBuf,
    pub index: u32,
    /// `main` for lane 0, else `lane<N>` (see [`lane_name`]).
    pub name: String,
    /// Branch slug for display, or the lane name on a detached HEAD.
    pub label: String,
    /// The lane's worktree directory.
    pub path: PathBuf,
    /// Checked-out branch; `None` on a detached HEAD or outside git.
    pub branch: Option<String>,
}

impl LaneFacts {
    /// Derives `name` and `label` from the index and branch.
    pub fn new(
        project: impl Into<String>,
        primary_path: impl Into<PathBuf>,
        index: u32,
        path: impl Into<PathBuf>,
        branch: Option<String>,
    ) -> Self {
        let name = lane_name(index);
        let label = branch.as_deref().map_or_else(|| name.clone(), branch_label);
        Self {
            project: project.into(),
            primary_path: primary_path.into(),
            index,
            name,
            label,
            path: path.into(),
            branch,
        }
    }
}

/// Returns `main` for lane 0 and `lane<N>` otherwise.
pub fn lane_name(index: u32) -> String {
    if index == 0 {
        "main".to_owned()
    } else {
        format!("lane{index}")
    }
}

/// Turns a branch name into a display label: `/` becomes `-`.
pub fn branch_label(branch: &str) -> String {
    branch.replace('/', "-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Names follow the index; labels follow the branch with slashes replaced.
    fn names_and_labels() {
        let main = LaneFacts::new("acme", "/w", 0, "/w", Some("main".into()));
        assert_eq!((main.name.as_str(), main.label.as_str()), ("main", "main"));
        let lane = LaneFacts::new("acme", "/w", 3, "/x", Some("feat/a/b".into()));
        assert_eq!(
            (lane.name.as_str(), lane.label.as_str()),
            ("lane3", "feat-a-b")
        );
        let detached = LaneFacts::new("acme", "/w", 2, "/y", None);
        assert_eq!(detached.label, "lane2");
    }
}

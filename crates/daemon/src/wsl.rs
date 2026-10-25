//! Worktrees on Windows drives inside WSL (`/mnt/c/...`). File access there
//! goes through the 9P file server, and inotify does not see edits Windows
//! programs make, so the Git watcher and `sync` miss them. Starts warn with
//! `windows_drive` and `doctor` lists the worktrees. Outside WSL
//! [`devlanes_sys::windows_drives`] has no drives, so nothing changes there.

use std::path::{Path, PathBuf};

use devlanes_proto::LaneWarning;
use serde_json::{Value, json};

use crate::engine::Inner;

/// The `windows_drive` warning for `lane` in `path` on the drive `root`
/// (`C:\`): `{lane, path, drive}`.
fn warning(lane: &str, path: &Path, root: &str) -> LaneWarning {
    LaneWarning {
        code: "windows_drive".into(),
        data: json!({"lane": lane, "path": path, "drive": root}),
    }
}

/// The warnings of the `lanes` whose worktree is on a Windows drive; `None`
/// outside WSL. Runs off the async threads: detection and the mount table are
/// file reads.
async fn on_windows_drives(lanes: Vec<(String, PathBuf)>) -> Option<Vec<LaneWarning>> {
    tokio::task::spawn_blocking(move || {
        devlanes_sys::wsl()?;
        let drives = devlanes_sys::windows_drives();
        Some(
            lanes
                .into_iter()
                .filter_map(|(lane, path)| {
                    let drive = drives.drive_of(&path)?;
                    Some(warning(&lane, &path, &drive.root))
                })
                .collect(),
        )
    })
    .await
    .ok()
    .flatten()
}

impl Inner {
    /// The `windows_drive` warning for a start of `reference` in `path`.
    pub(crate) async fn windows_drive_warning(
        &self,
        reference: &str,
        path: &Path,
    ) -> Option<LaneWarning> {
        on_windows_drives(vec![(reference.to_owned(), path.to_owned())])
            .await?
            .pop()
    }

    /// `doctor`'s `windows_drive_worktrees`: `[{lane, path, drive}]` for every
    /// listed lane whose worktree is on a Windows drive; `None` outside WSL,
    /// where doctor leaves the key out.
    pub(crate) async fn windows_drive_doctor(&self) -> Option<Value> {
        let lanes = self
            .all_lanes()
            .iter()
            .filter_map(|(project, lane)| {
                let st = lane.st();
                st.listed()
                    .then(|| (st.reference(&project.name()), st.path.clone()))
            })
            .collect();
        let warnings = on_windows_drives(lanes).await?;
        Some(Value::Array(
            warnings.into_iter().map(|warning| warning.data).collect(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_names_the_lane_path_and_drive() {
        let warning = warning("acme@2", Path::new("/mnt/c/src/acme"), r"C:\");
        assert_eq!(warning.code, "windows_drive");
        assert_eq!(
            warning.data,
            json!({"lane": "acme@2", "path": "/mnt/c/src/acme", "drive": "C:\\"})
        );
    }
}

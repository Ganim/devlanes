//! Worktrees on Windows drives inside WSL (`/mnt/c/...`). File access there goes
//! through the 9P file server, and inotify does not see edits Windows programs
//! make, so the git watcher and `sync` miss them. Starts warn with
//! `windows_drive`, and `doctor` lists the worktrees. Nothing happens outside
//! WSL, which [`devlanes_sys::windows_drives`] reports as no drives.

use std::path::{Path, PathBuf};

use devlanes_proto::LaneWarning;
use serde_json::{Value, json};

use crate::engine::Inner;

/// The `windows_drive` warning for `lane` when `path` is on `drive`:
/// `{lane, path, drive}`, with `drive` the Windows root (`C:\`).
fn warning(lane: &str, path: &Path, drive: &devlanes_sys::DriveMount) -> LaneWarning {
    LaneWarning {
        code: "windows_drive".into(),
        data: json!({
            "lane": lane,
            "path": path,
            "drive": drive.windows_root,
        }),
    }
}

/// Which of `paths` are on Windows drives, with each one's warning. Blocking
/// (reads the mount table once).
fn on_windows_drives(lanes: Vec<(String, PathBuf)>) -> Vec<LaneWarning> {
    let drives = devlanes_sys::windows_drives();
    lanes
        .into_iter()
        .filter_map(|(lane, path)| {
            drives
                .drive_of(&path)
                .map(|drive| warning(&lane, &path, &drive))
        })
        .collect()
}

/// Runs [`on_windows_drives`] off the async threads; nothing when the task fails.
async fn check(lanes: Vec<(String, PathBuf)>) -> Vec<LaneWarning> {
    if devlanes_sys::wsl().is_none() || lanes.is_empty() {
        return Vec::new();
    }
    tokio::task::spawn_blocking(move || on_windows_drives(lanes))
        .await
        .unwrap_or_default()
}

impl Inner {
    /// The `windows_drive` warning for a lane whose worktree is on a Windows
    /// drive, for its start result.
    pub(crate) async fn windows_drive_warning(
        &self,
        reference: &str,
        path: &Path,
    ) -> Option<LaneWarning> {
        check(vec![(reference.to_owned(), path.to_owned())])
            .await
            .into_iter()
            .next()
    }

    /// `doctor`'s `windows_drive_worktrees`: `[{lane, path, drive}]` for every
    /// known worktree on a Windows drive; `null` outside WSL.
    pub(crate) async fn windows_drive_doctor(&self) -> Value {
        if devlanes_sys::wsl().is_none() {
            return Value::Null;
        }
        let lanes = self
            .all_lanes()
            .iter()
            .map(|(project, lane)| {
                let st = lane.st();
                (st.reference(&project.name()), st.path.clone())
            })
            .collect();
        Value::Array(
            check(lanes)
                .await
                .into_iter()
                .map(|warning| warning.data)
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warning_names_the_lane_path_and_drive() {
        let drive = devlanes_sys::DriveMount {
            mount_point: "/mnt/c".into(),
            windows_root: r"C:\".into(),
        };
        let warning = warning("acme@2", Path::new("/mnt/c/src/acme"), &drive);
        assert_eq!(warning.code, "windows_drive");
        assert_eq!(
            warning.data,
            json!({"lane": "acme@2", "path": "/mnt/c/src/acme", "drive": "C:\\"})
        );
    }
}

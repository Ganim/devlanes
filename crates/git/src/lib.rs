//! Git worktree discovery preserves administration identities across directory moves.

use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::process::Command;

/// Git failures retain stderr for diagnostics without exposing a request environment.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("git I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("git command failed: {0}")]
    Git(String),
    #[error("git output is not valid UTF-8")]
    Encoding,
    #[error("malformed worktree output: {0}")]
    Porcelain(String),
    #[error("watch setup failed: {0}")]
    Watch(#[from] notify::Error),
}

/// The administration name is stable when `git worktree move` changes its path.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Worktree {
    pub identity: String,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head: String,
    pub present: bool,
    pub prunable: bool,
}

/// The common Git directory distinguishes independent clones with the same project name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub common_dir: PathBuf,
    pub root: PathBuf,
    pub primary_branch: String,
    pub worktrees: Vec<Worktree>,
}

/// Where a search for `devlanes.yaml` ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSearch {
    Found(PathBuf),
    /// No config; `searched_to` is the last directory looked in (the repository
    /// root inside Git, else the filesystem root).
    Missing {
        searched_to: PathBuf,
    },
}

/// Searches `cwd` and its ancestors so monorepo configs can live below the Git
/// root, stopping at the repository root (the first directory holding `.git`):
/// a config above it belongs to some other project.
pub async fn search_config(cwd: &Path) -> Result<ConfigSearch, Error> {
    let mut searched_to = cwd.to_path_buf();
    for parent in cwd.ancestors() {
        searched_to = parent.to_path_buf();
        let path = parent.join("devlanes.yaml");
        if tokio::fs::try_exists(&path).await? {
            return Ok(ConfigSearch::Found(path));
        }
        if tokio::fs::try_exists(parent.join(".git")).await? {
            break;
        }
    }
    Ok(ConfigSearch::Missing { searched_to })
}

/// [`search_config`] without the place the search ended.
pub async fn find_config(cwd: &Path) -> Result<Option<PathBuf>, Error> {
    Ok(match search_config(cwd).await? {
        ConfigSearch::Found(path) => Some(path),
        ConfigSearch::Missing { .. } => None,
    })
}

/// Returns `None` for an ordinary non-Git directory; other failures remain errors.
pub async fn discover(cwd: &Path, primary: Option<&str>) -> Result<Option<Repository>, Error> {
    let probe = Command::new("git")
        .args(["-C"])
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"])
        .stdin(Stdio::null())
        .output()
        .await?;
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr);
        if stderr.contains("not a git repository") {
            return Ok(None);
        }
        return Err(Error::Git(stderr.trim().into()));
    }
    let root = PathBuf::from(text(&probe.stdout)?.trim_end_matches('\n'));
    let common_dir = PathBuf::from(
        text(
            &git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .await?,
        )?
        .trim_end_matches('\n'),
    );
    let primary_branch = match primary {
        Some(branch) => branch.to_owned(),
        None => default_branch(&root).await?,
    };
    let worktrees = list(&root, &common_dir).await?;
    Ok(Some(Repository {
        common_dir,
        root,
        primary_branch,
        worktrees,
    }))
}

/// Re-listing is authoritative; filesystem notifications only trigger this operation.
pub async fn list(root: &Path, common_dir: &Path) -> Result<Vec<Worktree>, Error> {
    let output = git(root, &["worktree", "list", "--porcelain", "-z"]).await?;
    let mut worktrees = parse_porcelain(&output)?;
    for worktree in &mut worktrees {
        worktree.present = tokio::fs::try_exists(&worktree.path).await? && !worktree.prunable;
        if worktree.present {
            let admin = git(
                &worktree.path,
                &["rev-parse", "--path-format=absolute", "--git-dir"],
            )
            .await?;
            let admin = PathBuf::from(text(&admin)?.trim_end_matches('\n'));
            worktree.identity = if admin == common_dir {
                "main".into()
            } else {
                admin
                    .strip_prefix(common_dir)
                    .map_err(|_| {
                        Error::Porcelain("worktree admin directory outside common directory".into())
                    })?
                    .to_string_lossy()
                    .into_owned()
            };
        }
    }
    // Missing entries still have administration records until Git prunes them.
    let admins = common_dir.join("worktrees");
    if let Ok(mut entries) = tokio::fs::read_dir(&admins).await {
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path().join("gitdir");
            if let Ok(content) = tokio::fs::read_to_string(path).await {
                let gitfile = PathBuf::from(content.trim_end_matches('\n'));
                if let Some(worktree) = worktrees
                    .iter_mut()
                    .find(|tree| tree.path == gitfile.parent().unwrap_or(Path::new("")))
                {
                    worktree.identity =
                        format!("worktrees/{}", entry.file_name().to_string_lossy());
                }
            }
        }
    }
    Ok(worktrees)
}

/// NUL records preserve spaces and newlines in worktree paths.
pub fn parse_porcelain(bytes: &[u8]) -> Result<Vec<Worktree>, Error> {
    let mut result = Vec::new();
    let mut current: Option<Worktree> = None;
    let mut bare = false;
    for field in bytes.split(|byte| *byte == 0) {
        let field = text(field)?;
        if field.is_empty() {
            if let Some(worktree) = current.take()
                && !bare
            {
                result.push(worktree);
            }
            bare = false;
        } else if let Some(path) = field.strip_prefix("worktree ") {
            current = Some(Worktree {
                identity: String::new(),
                path: path.into(),
                branch: None,
                head: String::new(),
                present: false,
                prunable: false,
            });
        } else if field == "bare" {
            bare = true;
        } else if let Some(worktree) = current.as_mut() {
            if let Some(head) = field.strip_prefix("HEAD ") {
                worktree.head = head.into();
            }
            if let Some(branch) = field.strip_prefix("branch refs/heads/") {
                worktree.branch = Some(branch.into());
            }
            if field == "prunable" || field.starts_with("prunable ") {
                worktree.prunable = true;
            }
        } else {
            return Err(Error::Porcelain("field before worktree path".into()));
        }
    }
    if let Some(worktree) = current
        && !bare
    {
        result.push(worktree);
    }
    Ok(result)
}

/// Branch names are passed after an option boundary and never interpreted by a shell.
pub async fn add(root: &Path, destination: &Path, branch: &str) -> Result<(), Error> {
    git(root, &["check-ref-format", "--branch", branch]).await?;
    let exists = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ])
        .stdin(Stdio::null())
        .status()
        .await?
        .success();
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args(["worktree", "add"]);
    if !exists {
        command.args(["-b", branch]);
    }
    command.arg("--").arg(destination);
    if exists {
        command.arg(branch);
    }
    let output = command.stdin(Stdio::null()).output().await?;
    if !output.status.success() {
        return Err(Error::Git(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ));
    }
    Ok(())
}

/// Periodic reconciliation covers dropped notifications and newly created admin dirs.
pub fn watch(
    repository: Repository,
) -> Result<tokio::sync::mpsc::Receiver<Result<Vec<Worktree>, Error>>, Error> {
    use notify::Watcher;
    let (events, mut changes) = tokio::sync::mpsc::channel(1);
    let mut watcher =
        notify::recommended_watcher(move |event: Result<notify::Event, notify::Error>| {
            if event.is_ok() {
                let _ = events.try_send(());
            }
        })?;
    watcher.watch(&repository.common_dir, notify::RecursiveMode::Recursive)?;
    for worktree in &repository.worktrees {
        if worktree.present {
            watcher.watch(&worktree.path, notify::RecursiveMode::NonRecursive)?;
        }
    }
    let (updates, receiver) = tokio::sync::mpsc::channel(2);
    tokio::spawn(async move {
        let _watcher = watcher;
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        loop {
            tokio::select! {
                _ = updates.closed() => break,
                _ = tick.tick() => {},
                Some(()) = changes.recv() => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    while changes.try_recv().is_ok() {}
                }
            }
            if updates
                .send(list(&repository.root, &repository.common_dir).await)
                .await
                .is_err()
            {
                break;
            }
        }
    });
    Ok(receiver)
}

/// Uses origin's symbolic default before falling back to conventional local branches.
async fn default_branch(root: &Path) -> Result<String, Error> {
    if let Ok(output) = git(
        root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await
        && let Some(branch) = text(&output)?.trim().strip_prefix("refs/remotes/origin/")
    {
        return Ok(branch.into());
    }
    for branch in ["main", "master"] {
        if git(
            root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .await
        .is_ok()
        {
            return Ok(branch.into());
        }
    }
    Ok("main".into())
}

/// Helper subprocesses use Tokio's own reaping independently of managed units.
async fn git(cwd: &Path, args: &[&str]) -> Result<Vec<u8>, Error> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(Error::Git(
            String::from_utf8_lossy(&output.stderr).trim().into(),
        ))
    }
}

/// Rejects undecodable Git metadata rather than silently changing an identity.
fn text(bytes: &[u8]) -> Result<&str, Error> {
    std::str::from_utf8(bytes).map_err(|_| Error::Encoding)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    /// The search finds a config below the repository root but never one above it.
    async fn config_search_stops_at_the_repository_root() -> Result<(), Box<dyn std::error::Error>>
    {
        let dir = tempfile::tempdir()?;
        let outer = dir.path();
        std::fs::write(outer.join("devlanes.yaml"), "project: outer\n")?;
        let repo = outer.join("repo");
        let nested = repo.join("apps/web");
        std::fs::create_dir_all(&nested)?;
        std::fs::create_dir(repo.join(".git"))?;
        assert_eq!(
            search_config(&nested).await?,
            ConfigSearch::Missing {
                searched_to: repo.clone()
            }
        );
        std::fs::write(repo.join("apps/devlanes.yaml"), "project: web\n")?;
        assert_eq!(
            find_config(&nested).await?,
            Some(repo.join("apps/devlanes.yaml"))
        );
        Ok(())
    }

    #[test]
    fn nul_paths_and_detached_heads_survive() -> Result<(), Error> {
        let trees = parse_porcelain(b"worktree /repo with\nnewline\0HEAD abc\0branch refs/heads/feat/checkout\0\0worktree /detached\0HEAD def\0detached\0\0")?;
        assert_eq!(trees.len(), 2);
        assert_eq!(trees[0].path, Path::new("/repo with\nnewline"));
        assert_eq!(trees[0].branch.as_deref(), Some("feat/checkout"));
        assert!(trees[1].branch.is_none());
        Ok(())
    }

    #[test]
    fn bare_entries_do_not_become_lanes() -> Result<(), Error> {
        assert!(parse_porcelain(b"worktree /bare.git\0bare\0\0")?.is_empty());
        Ok(())
    }

    /// Keeps the test body readable while using a canonicalized directory.
    struct TempPath(PathBuf);

    impl TempPath {
        /// Mirrors `TempDir::path` for the canonical location.
        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[tokio::test]
    async fn moved_worktree_keeps_its_identity() -> Result<(), Box<dyn std::error::Error>> {
        tokio::time::timeout(Duration::from_secs(15), async {
            let dir = tempfile::tempdir()?;
            // Git reports canonical paths, while macOS temp paths include a /var symlink.
            let temp = TempPath(dir.path().canonicalize()?);
            git(temp.path(), &["init", "-b", "main"]).await?;
            git(
                temp.path(),
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "--allow-empty",
                    "-m",
                    "initial",
                ],
            )
            .await?;
            let lane = temp.path().join("lane");
            add(temp.path(), &lane, "feat/checkout").await?;
            let before = discover(temp.path(), None)
                .await?
                .ok_or("missing repository")?;
            let linked = before
                .worktrees
                .iter()
                .find(|tree| tree.branch.as_deref() == Some("feat/checkout"))
                .ok_or("missing lane")?;
            let moved = temp.path().join("moved");
            let output = Command::new("git")
                .arg("-C")
                .arg(temp.path())
                .args(["worktree", "move"])
                .arg(&lane)
                .arg(&moved)
                .output()
                .await?;
            assert!(output.status.success());
            let after = discover(temp.path(), None)
                .await?
                .ok_or("missing repository")?;
            let moved = after
                .worktrees
                .iter()
                .find(|tree| tree.path == moved)
                .ok_or("missing moved lane")?;
            assert_eq!(linked.identity, moved.identity);
            Ok::<_, Box<dyn std::error::Error>>(())
        })
        .await??;
        Ok(())
    }
}

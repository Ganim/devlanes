# devlanes-git

Git worktree discovery for devlanes. The daemon uses it to find a project's `devlanes.yaml`, list the worktrees that become lanes, create new ones for `devlanes lane new`, and watch for worktrees being added or removed; it runs the `git` CLI.

## Usage

```rust
use std::path::Path;
use devlanes_git::{Error, discover, find_config, watch};

async fn lanes(cwd: &Path) -> Result<(), Error> {
    let Some(config) = find_config(cwd).await? else { return Ok(()) };
    let Some(repository) = discover(cwd, None).await? else { return Ok(()) };
    println!("{} on {}", config.display(), repository.primary_branch);
    for worktree in &repository.worktrees {
        println!("{} {:?} {}", worktree.identity, worktree.branch, worktree.path.display());
    }
    let mut changes = watch(repository)?;
    while let Some(worktrees) = changes.recv().await {
        println!("{} worktrees", worktrees?.len());
    }
    Ok(())
}
```

## API

- `find_config`, `search_config`: the nearest `devlanes.yaml` in `cwd` or its ancestors, never above the repository root; `search_config` also says where a fruitless search stopped.
- `discover`: the repository's common directory, root, primary branch and worktrees; `None` outside Git.
- `list` and `parse_porcelain`: the worktree list from `git worktree list --porcelain -z`.
- `has_git_link`: whether a worktree directory still has its `.git` entry.
- `add`: `git worktree add` for a new or existing branch, after `git check-ref-format` accepts its name.
- `watch`: a channel that receives the full worktree list again after each change.

## Config example

```yaml
# devlanes.yaml
lanes:
  # Branch that gets lane 0.
  primary: main
  # Where devlanes lane new creates worktrees, relative to the project; this is the default.
  worktree_dir: ../${{ project.name }}-lanes
```

`devlanes lane new feat/login` runs `add` for `../acme-lanes/feat-login`.

## Behaviour

A worktree's `identity` is its administration name under the common Git directory, so `git worktree move` keeps the same lane. The common directory separates independent clones that share a project name. Without an explicit primary branch, `discover` takes origin's default branch, then `main`, then `master`.

`watch` relists on filesystem events and on a timer, because notifications can be dropped.

A worktree is `present` only while its directory holds its `.git` entry (`has_git_link`). On Windows, `git worktree remove` cannot delete directories that a running process uses as its working directory: it deletes `.git`, the files and its record, then fails, leaving the directories behind. Such a worktree counts as removed.

## License

Apache-2.0

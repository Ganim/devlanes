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

- `find_config`: the nearest `devlanes.yaml` in `cwd` or its ancestors.
- `discover`: the repository's common directory, root, primary branch and worktrees; `None` outside Git.
- `list` and `parse_porcelain`: the worktree list from `git worktree list --porcelain -z`.
- `add`: `git worktree add` for a new or existing branch, after `git check-ref-format` accepts its name.
- `watch`: a channel that receives the full worktree list again after each change.

## Behaviour

A worktree's `identity` is its administration name under the common Git directory, so `git worktree move` keeps the same lane. The common directory separates independent clones that share a project name. Without an explicit primary branch, `discover` takes origin's default branch, then `main`, then `master`.

`watch` relists on filesystem events and on a timer, because notifications can be dropped.

## License

Apache-2.0

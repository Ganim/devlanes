# devlanes-env-direnv

Launch contributor that applies a lane's `.envrc` to every task, service and infra command through `direnv export json`. The binary registers it; `devlanes-resolve` applies it after mise and before platform contributors.

## Usage

```rust
use std::{sync::Arc, time::Duration};
use devlanes_core::contrib::Contributor;
use devlanes_env_direnv::DirenvContributor;

let direnv: Arc<dyn Contributor> = Arc::new(DirenvContributor::default());
let pinned: Arc<dyn Contributor> =
    Arc::new(DirenvContributor::new("/usr/local/bin/direnv", Duration::from_secs(10)));
```

## Config example

There is no `devlanes.yaml` key. An allowed `.envrc` in the lane worktree turns it on:

```sh
# .envrc
export DATABASE_URL=postgres://localhost/app_dev
PATH_add bin
```

Every unit of the lane then gets what `direnv export json` reports: `DATABASE_URL` and the `PATH` with `bin` in front. Variables the `.envrc` unsets are removed from the client environment. Service `env` and `global_env` still override them.

## Behaviour

- Runs only when the lane worktree has an `.envrc`.
- Runs direnv in the worktree with the client environment, since direnv reports changes relative to the environment it runs in. Variables it sets go to the contributor layer; variables it unsets are removed from the client layer.
- Drops direnv's own `DIRENV_*` variables so shells inside units do not treat the `.envrc` as loaded.
- Runs once per lane resolution; the result is shared by all units of the lane.
- A blocked `.envrc` or a missing `direnv` binary becomes a note on the launch, not an error. devlanes never runs `direnv allow` for you.
- Other direnv failures and timeouts (15 seconds by default) fail the unit with direnv's last error line.

`parse_export_json` parses captured output without running direnv.

## License

Apache-2.0

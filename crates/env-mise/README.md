# devlanes-env-mise

Launch contributor that adds a lane's mise environment (tool `PATH`, `[env]` values) to every task, service and infra command. The binary registers it; `devlanes-resolve` applies it before platform contributors.

## Usage

```rust
use std::{sync::Arc, time::Duration};
use devlanes_core::contrib::Contributor;
use devlanes_env_mise::MiseContributor;

let mise: Arc<dyn Contributor> = Arc::new(MiseContributor::default());
let pinned: Arc<dyn Contributor> =
    Arc::new(MiseContributor::new("/opt/homebrew/bin/mise", Duration::from_secs(10)));
```

## Config example

There is no `devlanes.yaml` key. A mise config in the lane worktree turns it on:

```toml
# mise.toml
[tools]
node = "24"

[env]
API_MODE = "local"
```

Every unit of the lane then gets the variables `mise env --json` prints: `PATH` with the mise tool directories in front of the client's `PATH`, plus `API_MODE=local`. Service `env` and `global_env` still override them.

## Behaviour

- Runs only when the lane worktree has a mise config (`mise.toml`, `.tool-versions` and the other names in `CONFIG_FILES`).
- Runs `mise env --json` in the worktree with the client environment, so mise prepends its tools to the client's `PATH`. Every variable mise prints is set at the contributor layer.
- Runs once per lane resolution; the result is shared by all units of the lane.
- An untrusted config or a missing `mise` binary becomes a note on the launch, not an error. Run `mise trust` in the worktree to apply the config.
- Other mise failures and timeouts (15 seconds by default) fail the unit with mise's first error line.

`parse_env_json` parses captured output without running mise.

## License

Apache-2.0

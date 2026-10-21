# devlanes-platform-node

Launch contributors for Node.js units and for projects with Wrangler configs. The binary registers them and `devlanes-resolve` applies them while planning each launch.

## Usage

```rust
use std::sync::Arc;
use devlanes_core::contrib::Contributor;
use devlanes_platform_node::{CloudflareContributor, NodeContributor};

let contributors: Vec<Arc<dyn Contributor>> =
    vec![Arc::new(NodeContributor), Arc::new(CloudflareContributor)];
// Pass as `ResolveInput::contributors`.
```

## Config example

```yaml
services:
  web:
    run_script: web/package.json#dev   # platform inferred: node
  worker:
    platform: node
    run: node worker.mjs
```

Node units get:

- `NODE_EXTRA_CA_CERTS`: the devlanes root certificate. A different value from the shell is replaced and reported as a note, because Node reads only one extra file.
- `FORCE_COLOR=1`, unless the shell sets `FORCE_COLOR` or `NO_COLOR`.

## Behaviour

Without `platform`, `NodeContributor` claims a unit whose `run_script` source is a `package.json`, or whose working directory contains one. `devlanes-platform-bun` and `devlanes-platform-deno` claim the same directory more strongly when it has a Bun lockfile or a Deno config.

`CloudflareContributor` applies to every unit of a lane whose worktree contains `wrangler.toml`, `wrangler.json` or `wrangler.jsonc` within four directory levels, skipping `node_modules`, build output and hidden directories. It sets `WRANGLER_REGISTRY_PATH=<lane>/.devlanes/wrangler-registry`, so same-named workers in two lanes do not bind to each other. The scan runs on a blocking thread once per lane resolution.

## License

Apache-2.0

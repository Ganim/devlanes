# devlanes-platform-bun

Launch contributor for Bun units. The binary registers it and `devlanes-resolve` applies it to units whose platform is `bun`, set in the config or detected from the project files.

## Usage

```rust
use std::sync::Arc;
use devlanes_core::contrib::Contributor;
use devlanes_platform_bun::BunContributor;

let bun: Arc<dyn Contributor> = Arc::new(BunContributor);
// Pass in `ResolveInput::contributors`, next to the node contributor.
```

## Config example

```yaml
services:
  api:
    run_script: apps/api/package.json#dev   # bun.lock at the repo root: platform inferred
  edge:
    platform: bun
    run: bun run src/edge.ts
```

Bun units get:

- `NODE_EXTRA_CA_CERTS`: the devlanes root certificate. Bun reads it since 1.1.22, for the runtime and `bun install`.
- `FORCE_COLOR=1`, unless the shell sets `FORCE_COLOR` or `NO_COLOR`. Bun lets `FORCE_COLOR` override `NO_COLOR`, so a shell `NO_COLOR` is respected by not setting it.

## Detection

Without `platform`, Bun claims a unit only when the nearest package-manager decision, walking from the working directory up to the lane root, is Bun:

- `"packageManager": "bun@..."` in a `package.json` (a pin to another manager stops the walk);
- `bun.lock`, `bun.lockb` or `bunfig.toml`;
- a `package-lock.json`, `pnpm-lock.yaml` or `yarn.lock` found first means not Bun.

Its claim sits just above node's on the same `package.json`, so a plain `package.json` project stays node.

## License

Apache-2.0

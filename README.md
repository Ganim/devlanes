# devlanes

devlanes is a pre-alpha development-environment manager for macOS and Linux. A lane is a Git worktree with its own ports, hostnames and data. The planned per-user daemon owns services; the CLI and TUI are clients.

The repository currently contains bootstrap infrastructure. Configuration examples describe the v1 contract; they do not imply that the corresponding runtime features are implemented.

```yaml
project: myapp
services:
  api:
    run_script: api/package.json#dev
    hosts: ["api.${{ lane.domain }}"]
  web:
    preset: vite
    run_script: web/package.json#dev
    env: { VITE_API_URL: "${{ svc.api.public_url }}" }
    hosts: ["${{ lane.domain }}"]
```

This excerpt comes from [minimal.yaml](docs/examples/minimal.yaml), which also includes shared Postgres and per-lane setup. See [design](docs/design.md), [development conventions](AGENTS.md), [fixtures](fixtures/README.md), and [M0 commands](docs/milestones/M0.md).

## Inspirations

mprocs and dekit informed terminal interaction; pitchfork, process-compose, devenv, Coasts, portless, kache, overmind and Conductor informed process and worktree workflows.

Apache-2.0. See [LICENSE](LICENSE).

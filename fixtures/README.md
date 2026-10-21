# fixtures

Native service configurations for integration tests. They use `fixture-server` without Node or Python.

- `two-services/devlanes.yaml`: API/web routing and worktree isolation.
- `many-services/devlanes.yaml`: 40 services for terminal and concurrency checks.
- `dogfood-monorepo/devlanes.yaml` and `dogfood-desktop/devlanes.yaml`: config-check shapes of a pnpm monorepo and a Tauri desktop app, loaded and validated by `devlanes-config` tests. Their commands are not runnable.

## Usage

From the repository root, build the server and make it available to fixture commands:

```sh
cargo build -p devlanes-testing --bin fixture-server
export PATH="$PWD/target/debug:$PATH"
```

If `CARGO_TARGET_DIR` is set, use its `debug` directory instead. See [devlanes-testing](../crates/testing/README.md) for server endpoints, flags and process cleanup requirements.

## License

Apache-2.0

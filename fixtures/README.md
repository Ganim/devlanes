# fixtures

Native service configurations for integration tests. They use `fixture-server` without Node or Python.

- `two-services/devlanes.yaml`: API/web routing and worktree isolation.
- `many-services/devlanes.yaml`: 40 services for terminal and concurrency checks.
- `full/`: shared Postgres through Compose, a database per lane in `infra.setup`, a `migrate` task on every start, a `seed` task after the services, and two services. Needs Docker; the `docker_*` tests of `devlanes-e2e` run it. Set `FULL_PG_PORT` in the primary worktree's `.env` to move Postgres off 25432.
- `compose-pg-redis/`: a Postgres per lane (`infra.per_lane`) with a table from `infra.setup`, and one Redis shared by every lane. Needs Docker; `docker_pg_redis` in `devlanes-e2e` checks that each lane has its own data and that `lane clean` removes only that lane's container and volume.
- `dogfood-monorepo/devlanes.yaml` and `dogfood-desktop/devlanes.yaml`: config-check shapes of a pnpm monorepo and a Tauri desktop app, loaded and validated by `devlanes-config` tests. Their commands are not runnable.

## Usage

From the repository root, build the server and make it available to fixture commands:

```sh
cargo build -p devlanes-testing --bin fixture-server
export PATH="$PWD/target/debug:$PATH"
```

If `CARGO_TARGET_DIR` is set, use its `debug` directory instead. Without it, `devlanes start` fails with `api could not start: fixture-server: command not found on PATH`: services get the PATH of the shell that ran the command. See [devlanes-testing](../crates/testing/README.md) for server endpoints, flags and process cleanup requirements.

## License

Apache-2.0

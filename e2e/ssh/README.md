# e2e/ssh

Runs the `needs ssh` tests of `devlanes-transport-ssh` against a throwaway Debian sshd container. The box allows key login for user `devlanes` only and sets `AllowTcpForwarding no`.

## Usage

```sh
e2e/ssh/run.sh                                       # build the box binary, run everything
DEVLANES_TEST_SSH_BIN=path/to/linux/devlanes e2e/ssh/run.sh
DEVLANES_TEST_SSH_BIN=skip e2e/ssh/run.sh            # no devlanes on the box; tunnel test skips
```

Requires Docker (OrbStack and Docker Desktop work on macOS), `ssh`, `ssh-keygen` and `ssh-keyscan`. `CARGO_TARGET_DIR` applies to the host test build.

In CI, the `ssh` job in `.github/workflows/ci.yml` runs `bash e2e/ssh/run.sh` on `ubuntu-latest`.

## Behaviour

`run.sh` builds a Linux `devlanes` in `rust:1.99.1-bookworm` (cached in Docker volumes), builds the image, and generates a temporary key. It then starts the container on a random `127.0.0.1` port, writes a `known_hosts` entry with `ssh-keyscan`, and runs `cargo test -p devlanes-transport-ssh --test it -- --ignored`. The container and temporary files are removed on exit.

The tunnel test also skips when the box's `devlanes tunnel --stdio` reports `not_supported_yet`.

## License

Apache-2.0

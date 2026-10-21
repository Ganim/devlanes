#!/usr/bin/env bash
# Runs the `needs ssh` tests against a throwaway sshd container on a random localhost port.
#
#   e2e/ssh/run.sh [extra cargo test args]
#
# DEVLANES_TEST_SSH_BIN: prebuilt Linux devlanes for the box. Unset builds one in
#   rust:1.99.1-bookworm; `skip` starts the box without devlanes (tunnel test skips).
# CARGO_TARGET_DIR is passed through to the host test build.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
image=devlanes-e2e-ssh
work=$(mktemp -d "${TMPDIR:-/tmp}/devlanes-ssh.XXXXXX")
container=""

cleanup() {
  if [ -n "$container" ]; then docker rm -f "$container" >/dev/null 2>&1 || true; fi
  rm -rf "$work"
}
trap cleanup EXIT INT TERM

bin=${DEVLANES_TEST_SSH_BIN:-}
if [ -z "$bin" ]; then
  # Host builds are Mach-O on macOS, so the box binary is built for Linux in a container.
  # Named volumes keep the registry and target dir warm between local runs.
  docker run --rm \
    -v "$repo":/src -w /src \
    -v devlanes-e2e-ssh-target:/target -v devlanes-e2e-ssh-registry:/usr/local/cargo/registry \
    -v "$work":/out \
    -e CARGO_TARGET_DIR=/target -e RUSTUP_TOOLCHAIN=1.99.1 \
    rust:1.99.1-bookworm \
    sh -c 'cargo build -p devlanes && cp /target/debug/devlanes /out/devlanes'
  bin=$work/devlanes
elif [ "$bin" != skip ]; then
  # docker -v needs an absolute path.
  bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
fi

docker build -q -t "$image" "$here" >/dev/null
ssh-keygen -q -t ed25519 -N '' -C devlanes-e2e -f "$work/id_ed25519"

mount=()
if [ "$bin" != skip ]; then mount=(-v "$bin:/usr/local/bin/devlanes:ro"); fi
# ${mount[@]+...} keeps bash 3.2 (macOS) from failing on an empty array under `set -u`.
container=$(docker run -d -p 127.0.0.1::22 \
  -e AUTHORIZED_KEY="$(cat "$work/id_ed25519.pub")" \
  ${mount[@]+"${mount[@]}"} "$image")
port=$(docker port "$container" 22/tcp | head -n 1 | sed 's/.*://')

# sshd needs a moment after the container starts; keyscan doubles as the readiness check.
for _ in $(seq 1 50); do
  if ssh-keyscan -T 2 -p "$port" 127.0.0.1 >"$work/known_hosts" 2>/dev/null && [ -s "$work/known_hosts" ]; then
    break
  fi
  sleep 0.2
done
if [ ! -s "$work/known_hosts" ]; then
  docker logs "$container" >&2 || true
  echo "sshd did not come up on port $port" >&2
  exit 1
fi

export DEVLANES_TEST_SSH_DEST=devlanes@127.0.0.1
export DEVLANES_TEST_SSH_PORT=$port
export DEVLANES_TEST_SSH_KEY=$work/id_ed25519
export DEVLANES_TEST_SSH_KNOWN_HOSTS=$work/known_hosts
if [ "$bin" = skip ]; then export DEVLANES_TEST_SSH_ALLOW_MISSING=1; fi

cd "$repo"
# --nocapture keeps skip reasons visible in CI logs.
cargo test -p devlanes-transport-ssh --test it -- --ignored --nocapture "$@"

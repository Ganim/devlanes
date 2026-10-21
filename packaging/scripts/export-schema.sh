#!/bin/sh
# Writes OUT_DIR/v1.json (devlanes.yaml and devlanes.local.yaml) and
# OUT_DIR/user-v1.json (~/.devlanes/config.yaml) for hosting under
# https://devlanes.run/schema/. `devlanes schema` is answered by the daemon,
# which adds the options of the presets built into this binary, so a
# throwaway daemon runs in a temporary DEVLANES_HOME and is stopped afterwards.
set -eu

if [ $# -lt 1 ] || [ $# -gt 2 ]; then
	echo "usage: $0 OUT_DIR [DEVLANES_BIN]" >&2
	exit 2
fi
out=$1
bin=${2:-devlanes}
command -v jq >/dev/null 2>&1 || {
	echo "jq is required" >&2
	exit 1
}

# A short path keeps the daemon socket under the Unix socket path limit.
home=$(mktemp -d /tmp/devlanes-schema.XXXXXX)
cleanup() {
	DEVLANES_HOME=$home "$bin" daemon stop >/dev/null 2>&1 || true
	rm -rf "$home"
}
trap cleanup EXIT

mkdir -p "$out"
schemas=$home/schemas.json
DEVLANES_HOME=$home "$bin" --json schema >"$schemas"
jq '.project' "$schemas" >"$out/v1.json"
jq '.user' "$schemas" >"$out/user-v1.json"
for file in v1.json user-v1.json; do
	id=$(jq -r '."$id"' "$out/$file")
	if [ "$id" != "https://devlanes.run/schema/$file" ]; then
		echo "$out/$file has \$id $id" >&2
		exit 1
	fi
done
echo "$out/v1.json"
echo "$out/user-v1.json"

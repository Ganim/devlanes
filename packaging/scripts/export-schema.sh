#!/bin/sh
# Copy the committed schemas to their public URL filenames without starting devlanes.
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 OUT_DIR" >&2
    exit 2
fi
root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
mkdir -p "$1"
cp "$root/crates/config/schema.json" "$1/v1.json"
cp "$root/crates/config/user-schema.json" "$1/user-v1.json"
echo "$1/v1.json"
echo "$1/user-v1.json"

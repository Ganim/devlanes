#!/bin/sh
# Packs the release binaries for one target into OUT_DIR/devlanes-VERSION-TARGET.tar.gz.
# The archive holds one directory of the same name with devlanes, devlanes-hosts
# and LICENSE; release-manifest.json refers to devlanes by that member path.
set -eu

if [ $# -lt 3 ]; then
	echo "usage: $0 VERSION TARGET OUT_DIR [BIN_DIR]" >&2
	exit 2
fi
version=$1
target=$2
bin_dir=${4:-target/$target/release}
root=$(cd "$(dirname "$0")/../.." && pwd)
mkdir -p "$3"
out=$(cd "$3" && pwd)
name=devlanes-$version-$target

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir "$stage/$name"
for bin in devlanes devlanes-hosts; do
	if [ ! -x "$bin_dir/$bin" ]; then
		echo "missing executable $bin_dir/$bin" >&2
		exit 1
	fi
	cp "$bin_dir/$bin" "$stage/$name/$bin"
	chmod 755 "$stage/$name/$bin"
done
cp "$root/LICENSE" "$stage/$name/LICENSE"
chmod 644 "$stage/$name/LICENSE"

# macOS tar would otherwise store extended attributes (com.apple.provenance) as
# pax headers that GNU tar warns about, and AppleDouble ._ files.
COPYFILE_DISABLE=1 tar --no-xattrs -C "$stage" -czf "$out/$name.tar.gz" "$name"
echo "$out/$name.tar.gz"

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

# Same inputs give the same archive: fixed owners, order and mtimes
# (SOURCE_DATE_EPOCH, else the last commit's time) and a gzip header without a timestamp.
epoch=${SOURCE_DATE_EPOCH:-$(git -C "$root" log -1 --format=%ct 2>/dev/null || echo 0)}
if tar --version 2>/dev/null | grep -q 'GNU tar'; then
	set -- --sort=name --owner=0 --group=0 --numeric-owner --mtime="@$epoch"
else
	# bsdtar (macOS) has no --sort or --mtime; one directory with fixed names keeps
	# the order stable, and touch fixes the times.
	stamp=$(date -u -r "$epoch" +%Y%m%d%H%M.%S 2>/dev/null || date -u -d "@$epoch" +%Y%m%d%H%M.%S)
	find "$stage/$name" -exec touch -h -t "$stamp" {} +
	set -- --uid 0 --gid 0 --uname root --gname root
fi
# macOS tar would otherwise store extended attributes (com.apple.provenance) as
# pax headers that GNU tar warns about, and AppleDouble ._ files.
COPYFILE_DISABLE=1 tar --no-xattrs "$@" -C "$stage" -cf "$stage/$name.tar" "$name"
gzip -n -9 -c "$stage/$name.tar" >"$out/$name.tar.gz"
echo "$out/$name.tar.gz"

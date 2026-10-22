#!/bin/sh
# Generates the npm packages for VERSION from the release archives in DIST_DIR:
# OUT_DIR/devlanes-<platform>-<arch>/ (published as @devlanes/<platform>-<arch>)
# and OUT_DIR/devlanes/ (the launcher). Publish the platform packages first.
# Needs tar and, for the Windows archives, unzip.
set -eu

if [ $# -ne 3 ]; then
	echo "usage: $0 VERSION DIST_DIR OUT_DIR" >&2
	exit 2
fi
version=$1
dist=$2
out=$3
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)

if [ -e "$out" ]; then
	echo "$out already exists" >&2
	exit 1
fi
mkdir -p "$out"

for spec in \
	aarch64-apple-darwin:darwin:arm64:macOS \
	x86_64-apple-darwin:darwin:x64:macOS \
	aarch64-unknown-linux-gnu:linux:arm64:Linux \
	x86_64-unknown-linux-gnu:linux:x64:Linux \
	x86_64-pc-windows-msvc:win32:x64:Windows \
	aarch64-pc-windows-msvc:win32:arm64:Windows; do
	IFS=: read -r target os cpu label <<EOF
$spec
EOF
	name=devlanes-$version-$target
	archive=$dist/$name.tar.gz
	if [ "$os" = win32 ]; then archive=$dist/$name.zip; fi
	if [ ! -f "$archive" ]; then
		echo "missing $archive" >&2
		exit 1
	fi
	pkg=$out/devlanes-$os-$cpu
	mkdir -p "$pkg/bin"
	if [ "$os" = win32 ]; then
		# devlanes.exe looks for the logon launcher and the hosts service next to
		# itself. These are the archive's copies, so they carry its signatures.
		unzip -q -j "$archive" "$name/devlanes.exe" "$name/devlanesw.exe" \
			"$name/devlanes-hosts-service.exe" -d "$pkg/bin"
	else
		tar -xzf "$archive" -C "$pkg/bin" --strip-components 1 \
			"$name/devlanes" "$name/devlanes-hosts"
		chmod 755 "$pkg/bin/devlanes" "$pkg/bin/devlanes-hosts"
	fi
	cp "$root/LICENSE" "$pkg/LICENSE"
	# Linux builds link glibc; the libc field keeps npm from installing them on musl.
	libc=
	if [ "$os" = linux ]; then
		libc='
  "libc": ["glibc"],'
	fi
	cat >"$pkg/package.json" <<EOF
{
  "name": "@devlanes/$os-$cpu",
  "version": "$version",
  "description": "devlanes binaries for $label $cpu",
  "license": "Apache-2.0",
  "homepage": "https://devlanes.run",
  "repository": {
    "type": "git",
    "url": "git+https://github.com/devlanes-run/devlanes.git"
  },
  "os": ["$os"],
  "cpu": ["$cpu"],$libc
  "files": ["bin"],
  "preferUnplugged": true
}
EOF
	printf '# @devlanes/%s-%s\n\nThe devlanes binaries for %s %s. Install the devlanes package instead of this one.\n' \
		"$os" "$cpu" "$label" "$cpu" >"$pkg/README.md"
done

mkdir -p "$out/devlanes/bin"
sed "s/\"0\.0\.0\"/\"$version\"/" "$here/devlanes/package.json" >"$out/devlanes/package.json"
cp "$here/devlanes/README.md" "$out/devlanes/README.md"
cp "$here/devlanes/bin/devlanes.js" "$out/devlanes/bin/devlanes.js"
chmod 755 "$out/devlanes/bin/devlanes.js"
cp "$root/LICENSE" "$out/devlanes/LICENSE"
# The committed schemas, so editors can point yaml-language-server at node_modules.
cp "$root/crates/config/schema.json" "$out/devlanes/schema.json"
cp "$root/crates/config/user-schema.json" "$out/devlanes/user-schema.json"

ls -1 "$out"

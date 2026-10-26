#!/bin/sh
# Writes SHA256SUMS (every archive, the Windows .zip ones included) and
# release-manifest.json (the .tar.gz archives) for the archives in DIST_DIR.
# The daemon's update check reads the manifest's version; older clients'
# `remote connect --install` also reads its archives: HTTPS URLs, lowercase hex
# SHA-256, tar_gz members without `..`.
set -eu

if [ $# -ne 3 ]; then
	echo "usage: $0 VERSION DIST_DIR BASE_URL" >&2
	exit 2
fi
version=$1
dist=$2
base_url=${3%/}
case $base_url in
https://*) ;;
*)
	echo "BASE_URL must be https://, got $base_url" >&2
	exit 2
	;;
esac

sha256() {
	if command -v sha256sum >/dev/null 2>&1; then
		sha256sum "$1" | cut -d ' ' -f 1
	else
		shasum -a 256 "$1" | cut -d ' ' -f 1
	fi
}

sums=$dist/SHA256SUMS
manifest=$dist/release-manifest.json
: >"$sums"
entries=
for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
	case $target in
	*-apple-darwin) os=macos ;;
	*-linux-gnu) os=linux ;;
	esac
	arch=${target%%-*}
	name=devlanes-$version-$target
	file=$dist/$name.tar.gz
	if [ ! -f "$file" ]; then
		echo "missing $file" >&2
		exit 1
	fi
	sum=$(sha256 "$file")
	printf '%s  %s\n' "$sum" "$name.tar.gz" >>"$sums"
	entry=$(printf '    {"os": "%s", "arch": "%s", "url": "%s/%s.tar.gz", "sha256": "%s", "format": {"kind": "tar_gz", "member": "%s/devlanes"}}' \
		"$os" "$arch" "$base_url" "$name" "$sum" "$name")
	entries=${entries:+$entries,
}$entry
done

# The Windows archives are listed in SHA256SUMS only: older clients' SSH bootstrap
# installs onto Linux and macOS boxes, so the manifest names the .tar.gz archives alone.
for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
	name=devlanes-$version-$target.zip
	if [ ! -f "$dist/$name" ]; then
		echo "missing $dist/$name" >&2
		exit 1
	fi
	printf '%s  %s\n' "$(sha256 "$dist/$name")" "$name" >>"$sums"
done

printf '{\n  "version": "%s",\n  "artifacts": [\n%s\n  ]\n}\n' "$version" "$entries" >"$manifest"
echo "$sums"
echo "$manifest"

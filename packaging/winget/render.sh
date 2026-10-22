#!/bin/sh
# Writes the winget manifests for VERSION into OUT_DIR, with checksums taken from a
# release's SHA256SUMS. `komac submit OUT_DIR` opens the pull request to
# microsoft/winget-pkgs with them.
set -eu

if [ $# -ne 3 ]; then
	echo "usage: $0 VERSION SHA256SUMS OUT_DIR" >&2
	exit 2
fi
version=$1
sums=$2
out=$3
here=$(dirname "$0")

checksum() {
	sum=$(awk -v file="devlanes-$version-$1.zip" '$2 == file { print $1 }' "$sums")
	if [ -z "$sum" ]; then
		echo "no checksum for devlanes-$version-$1.zip in $sums" >&2
		exit 1
	fi
	printf '%s' "$sum"
}

intel=$(checksum x86_64-pc-windows-msvc)
arm=$(checksum aarch64-pc-windows-msvc)

mkdir -p "$out"
for template in "$here"/devlanes-run.devlanes*.yaml; do
	file=$out/$(basename "$template")
	# The first two lines describe the template, not the manifest.
	sed \
		-e "s/@VERSION@/$version/g" \
		-e "s/@SHA256_X86_64_PC_WINDOWS_MSVC@/$intel/g" \
		-e "s/@SHA256_AARCH64_PC_WINDOWS_MSVC@/$arm/g" \
		-e '1,2d' \
		"$template" >"$file"
	if grep -q '@[A-Z0-9_]*@' "$file"; then
		echo "unfilled placeholder in $template" >&2
		exit 1
	fi
	echo "$file"
done

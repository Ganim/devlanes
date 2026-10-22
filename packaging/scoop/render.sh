#!/bin/sh
# Prints the Scoop manifest (bucket/devlanes.json in devlanes-run/scoop-bucket) for
# VERSION, with checksums taken from a release's SHA256SUMS.
set -eu

if [ $# -ne 2 ]; then
	echo "usage: $0 VERSION SHA256SUMS" >&2
	exit 2
fi
version=$1
sums=$2
template=$(dirname "$0")/devlanes.json

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

manifest=$(sed \
	-e "s/@VERSION@/$version/g" \
	-e "s/@SHA256_X86_64_PC_WINDOWS_MSVC@/$intel/g" \
	-e "s/@SHA256_AARCH64_PC_WINDOWS_MSVC@/$arm/g" \
	"$template")

if printf '%s\n' "$manifest" | grep -q '@[A-Z0-9_]*@'; then
	echo "unfilled placeholder in $template" >&2
	exit 1
fi
printf '%s\n' "$manifest"

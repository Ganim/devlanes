#!/bin/sh
# Prints the Homebrew formula for VERSION, with checksums taken from a release's SHA256SUMS.
set -eu

if [ $# -ne 2 ]; then
	echo "usage: $0 VERSION SHA256SUMS" >&2
	exit 2
fi
version=$1
sums=$2
template=$(dirname "$0")/devlanes.rb

checksum() {
	sum=$(awk -v file="devlanes-$version-$1.tar.gz" '$2 == file { print $1 }' "$sums")
	if [ -z "$sum" ]; then
		echo "no checksum for devlanes-$version-$1.tar.gz in $sums" >&2
		exit 1
	fi
	printf '%s' "$sum"
}

arm_mac=$(checksum aarch64-apple-darwin)
intel_mac=$(checksum x86_64-apple-darwin)
arm_linux=$(checksum aarch64-unknown-linux-gnu)
intel_linux=$(checksum x86_64-unknown-linux-gnu)

formula=$(sed \
	-e "s/@VERSION@/$version/g" \
	-e "s/@SHA256_AARCH64_APPLE_DARWIN@/$arm_mac/g" \
	-e "s/@SHA256_X86_64_APPLE_DARWIN@/$intel_mac/g" \
	-e "s/@SHA256_AARCH64_UNKNOWN_LINUX_GNU@/$arm_linux/g" \
	-e "s/@SHA256_X86_64_UNKNOWN_LINUX_GNU@/$intel_linux/g" \
	-e '1,2d' \
	"$template")

if printf '%s\n' "$formula" | grep -q '@[A-Z0-9_]*@'; then
	echo "unfilled placeholder in $template" >&2
	exit 1
fi
printf '%s\n' "$formula"

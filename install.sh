#!/bin/sh
# Installs devlanes and devlanes-hosts from a GitHub release after checking the
# archive against the release's SHA256SUMS.
#
#   curl -fsSL https://devlanes.run/install.sh | sh
#   curl -fsSL https://devlanes.run/install.sh | sh -s -- --channel next
#   curl -fsSL https://devlanes.run/install.sh | sh -s -- --version 0.4.0
#
# -c, --channel stable (the default) installs the latest release, --channel next the
# latest build of main from the rolling `next` prerelease, and -v, --version X.Y.Z one
# release. -d, --dir DIR sets the target directory (default: ~/.local/bin).
# DEVLANES_VERSION (X.Y.Z or next) and DEVLANES_INSTALL_DIR do the same when the
# option isn't given.
# DEVLANES_INSTALL_SUDO=1 allows sudo when that directory is not writable.
# DEVLANES_INSTALL_BASE_URL replaces the release URL; it exists only so the
# script can be tested against a local directory of fake releases.
set -eu

base=${DEVLANES_INSTALL_BASE_URL:-https://github.com/devlanes-run/devlanes/releases}
proto='=https'
if [ -n "${DEVLANES_INSTALL_BASE_URL:-}" ]; then proto='=https,file'; fi
version=${DEVLANES_VERSION:-}
dir=${DEVLANES_INSTALL_DIR:-$HOME/.local/bin}

fail() {
	printf 'devlanes install: %s\n' "$1" >&2
	exit 1
}

usage='usage: sh install.sh [-c, --channel stable|next | -v, --version X.Y.Z] [-d, --dir DIR]'
usage() {
	printf '%s\n' "$usage" >&2
	exit 2
}

channel_set=
version_set=
while [ "$#" -gt 0 ]; do
	case $1 in
	-c | --channel | -v | --version | -d | --dir)
		[ "$#" -ge 2 ] || usage
		option=$1 value=$2
		shift 2
		;;
	--channel=* | --version=* | --dir=*)
		option=${1%%=*} value=${1#*=}
		shift
		;;
	-h | --help)
		printf '%s\n' "$usage"
		exit 0
		;;
	*)
		printf 'devlanes install: unknown option %s\n' "$1" >&2
		usage
		;;
	esac
	[ -n "$value" ] || usage
	case $option:$value in
	-c:stable | --channel:stable) version='' channel_set=1 ;;
	-c:next | --channel:next) version=next channel_set=1 ;;
	-c:* | --channel:*) fail "unknown channel $value; use stable or next" ;;
	-v:* | --version:*) version=$value version_set=1 ;;
	-d:* | --dir:*) dir=$value ;;
	esac
done
if [ -n "$channel_set" ] && [ -n "$version_set" ]; then
	fail "--channel and --version can't be combined"
fi

version=${version#v}
channel=
if [ "$version" = next ]; then
	channel=next
	version=
fi

for tool in curl tar uname mktemp awk; do
	command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
done
if command -v sha256sum >/dev/null 2>&1; then
	sha256() { sha256sum "$1" | cut -d ' ' -f 1; }
elif command -v shasum >/dev/null 2>&1; then
	sha256() { shasum -a 256 "$1" | cut -d ' ' -f 1; }
else
	fail "sha256sum or shasum is required"
fi

os=$(uname -s)
arch=$(uname -m)
# A shell running under Rosetta reports x86_64; the arm64 build is the native one.
if [ "$os" = Darwin ] && [ "$arch" = x86_64 ] &&
	[ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then
	arch=arm64
fi
case "$os:$arch" in
Darwin:arm64 | Darwin:aarch64) target=aarch64-apple-darwin ;;
Darwin:x86_64) target=x86_64-apple-darwin ;;
Linux:x86_64 | Linux:amd64) target=x86_64-unknown-linux-gnu ;;
Linux:aarch64 | Linux:arm64) target=aarch64-unknown-linux-gnu ;;
*) fail "no release build for $os $arch" ;;
esac
if [ "$os" = Linux ] && ls /lib/ld-musl-* >/dev/null 2>&1; then
	fail "the Linux builds need glibc; this system uses musl"
fi

tmp=$(mktemp -d)
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

download() {
	curl -fsSL --proto "$proto" --proto-redir "$proto" --tlsv1.2 "$1" -o "$2"
}

if [ -n "$channel" ]; then
	sums_url=$base/download/$channel/SHA256SUMS
elif [ -n "$version" ]; then
	sums_url=$base/download/v$version/SHA256SUMS
else
	sums_url=$base/latest/download/SHA256SUMS
fi
download "$sums_url" "$tmp/SHA256SUMS" || fail "cannot download $sums_url"

# The latest release is found through its SHA256SUMS, which names the archive and
# so the version; the archive then comes from that version's own release, or from
# the `next` release, which only ever holds its latest build.
line=$(awk -v suffix="-$target.tar.gz" '
	length($2) > length(suffix) && substr($2, length($2) - length(suffix) + 1) == suffix { print; exit }
' "$tmp/SHA256SUMS")
[ -n "$line" ] || fail "no $target archive listed in $sums_url"
expected=${line%% *}
archive=${line##* }
found=${archive#devlanes-}
found=${found%"-$target.tar.gz"}
if [ -n "$version" ] && [ "$found" != "$version" ]; then
	fail "$sums_url lists version $found, expected $version"
fi
version=$found
printf '%s\n' "$version" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$' ||
	fail "unexpected version $version in $sums_url"
printf '%s\n' "$expected" | grep -Eq '^[0-9a-f]{64}$' || fail "malformed checksum for $archive"

url=$base/download/${channel:-v$version}/$archive
download "$url" "$tmp/$archive" || fail "cannot download $url"
actual=$(sha256 "$tmp/$archive")
[ "$actual" = "$expected" ] || fail "checksum mismatch for $archive: expected $expected, got $actual"

# Extract only the exact entries release archives contain.
top=devlanes-$version-$target
listed=$(tar -tzf "$tmp/$archive" | sed 's#/$##' | LC_ALL=C sort | tr '\n' ' ')
[ "$listed" = "$top $top/LICENSE $top/devlanes $top/devlanes-hosts " ] ||
	fail "unexpected entries in $archive: $listed"
tar -xzf "$tmp/$archive" -C "$tmp"

case "$dir" in
/*) ;;
*) dir=$(pwd)/$dir ;;
esac
sudo=
if ! mkdir -p "$dir" 2>/dev/null || [ ! -w "$dir" ]; then
	if [ "${DEVLANES_INSTALL_SUDO:-}" != 1 ]; then
		fail "$dir is not writable; pass --dir with a writable directory, or DEVLANES_INSTALL_SUDO=1 to install there with sudo"
	fi
	command -v sudo >/dev/null 2>&1 || fail "$dir is not writable and sudo is not available"
	printf 'devlanes install: %s is not writable; using sudo to install there\n' "$dir"
	sudo=sudo
fi
$sudo mkdir -p "$dir"
for bin in devlanes devlanes-hosts; do
	# Rename into place so a running devlanes keeps its open executable.
	$sudo cp "$tmp/$top/$bin" "$dir/.$bin.new.$$"
	$sudo chmod 755 "$dir/.$bin.new.$$"
	$sudo mv -f "$dir/.$bin.new.$$" "$dir/$bin"
done

printf 'Installed devlanes %s to %s\n' "$version" "$dir"
case ":${PATH:-}:" in
*":$dir:"*)
	first=$(command -v devlanes 2>/dev/null || true)
	if [ -n "$first" ] && [ "$first" != "$dir/devlanes" ]; then
		printf 'Another devlanes comes first on PATH: %s\n' "$first"
	fi
	;;
*)
	printf '%s is not on PATH. Add it to your shell profile:\n  export PATH="%s:$%s"\n' "$dir" "$dir" PATH
	;;
esac

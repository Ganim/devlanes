#!/bin/sh
# Dry run of the release scripts against fake binaries: package.sh for every target,
# manifest.sh, the Homebrew formula, the npm packages (packed, then installed from
# the tarballs on macOS and Linux arm64/x64) and install.sh from a local release
# directory. Needs tar, gzip, jq, node, npm and curl; CI runs it on every push.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
cleanup() { rm -rf "$work"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

version=0.0.0-check
mkdir "$work/bin"
for bin in devlanes devlanes-hosts; do
	printf '#!/bin/sh\necho "%s %s"\n' "$bin" "$version" >"$work/bin/$bin"
	chmod 755 "$work/bin/$bin"
done

for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
	sh "$root/packaging/scripts/package.sh" "$version" "$target" "$work/dist" "$work/bin" >/dev/null
done
sh "$root/packaging/scripts/manifest.sh" "$version" "$work/dist" "https://example.invalid/download/v$version" >/dev/null
jq -e '.artifacts | length == 4' "$work/dist/release-manifest.json" >/dev/null

sh "$root/packaging/homebrew/render.sh" "$version" "$work/dist/SHA256SUMS" >"$work/devlanes.rb"
if command -v ruby >/dev/null 2>&1; then ruby -c "$work/devlanes.rb" >/dev/null; fi

sh "$root/packaging/npm/build.sh" "$version" "$work/dist" "$work/npm" >/dev/null
mkdir "$work/tgz"
for dir in "$work"/npm/*/; do
	jq -e .name "$dir/package.json" >/dev/null
	(cd "$dir" && npm pack --pack-destination "$work/tgz" >/dev/null 2>&1) || {
		echo "npm pack failed in $dir" >&2
		exit 1
	}
done

# Installs the launcher and this host's platform package from the local tarballs.
# The other platforms' optional dependencies are not on any registry; npm skips them.
# npm 12 blocks install scripts by default and matches --allow-scripts against the
# tarball path rather than the name, so this local-only install allows them all.
npm_install() {
	prefix=$1
	shift
	npm install -g --offline --no-audit --no-fund --no-update-notifier --loglevel=error --dangerously-allow-all-scripts \
		--cache "$work/npm-cache" --prefix "$prefix" "$@" \
		"$work/tgz/devlanes-$version.tgz" "$work/tgz/devlanes-$host-$version.tgz" >/dev/null
}
host=$(node -p 'process.platform + "-" + process.arch')
if [ -f "$work/tgz/devlanes-$host-$version.tgz" ]; then
	# postinstall swaps the launcher for the binary, with devlanes-hosts next to it.
	npm_install "$work/prefix"
	installed=$work/prefix/lib/node_modules/devlanes/bin
	[ "$("$work/prefix/bin/devlanes" --version)" = "devlanes $version" ]
	cmp -s "$installed/devlanes" "$work/bin/devlanes"
	cmp -s "$installed/devlanes-hosts" "$work/bin/devlanes-hosts"
	# A second install over the first replaces the files again.
	npm_install "$work/prefix"
	cmp -s "$installed/devlanes" "$work/bin/devlanes"
	# Without scripts the launcher stays and runs the binary.
	npm_install "$work/prefix-noscripts" --ignore-scripts
	[ "$(head -n 1 "$work/prefix-noscripts/lib/node_modules/devlanes/bin/devlanes")" = "#!/usr/bin/env node" ]
	[ "$("$work/prefix-noscripts/bin/devlanes" --version)" = "devlanes $version" ]
fi

release=$work/release
mkdir -p "$release/download/v$version" "$release/latest/download"
cp "$work"/dist/*.tar.gz "$work/dist/SHA256SUMS" "$release/download/v$version/"
cp "$work/dist/SHA256SUMS" "$release/latest/download/"
DEVLANES_INSTALL_BASE_URL="file://$release" DEVLANES_INSTALL_DIR="$work/installed" \
	sh "$root/install.sh" >/dev/null
[ "$("$work/installed/devlanes")" = "devlanes $version" ]
[ -x "$work/installed/devlanes-hosts" ]

echo "packaging scripts: dry run passed"

#!/bin/sh
# Dry run of the release scripts against fake binaries: package.sh and package.ps1
# for every target, manifest.sh, the Homebrew formula, the Scoop and winget
# manifests, the npm packages and install.sh from a local release directory. Needs
# tar, gzip, unzip, jq, npm, curl and pwsh; CI runs it on every push.
# check.ps1 covers install.ps1 on Windows.
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
mkdir "$work/winbin"
for bin in devlanes.exe devlanesw.exe devlanes-hosts-service.exe; do
	printf 'fake %s %s\n' "$bin" "$version" >"$work/winbin/$bin"
done
for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
	pwsh -NoProfile -NonInteractive -File "$root/packaging/scripts/package.ps1" \
		-Version "$version" -Target "$target" -OutDir "$work/dist" -BinDir "$work/winbin" >/dev/null
	listed=$(unzip -Z1 "$work/dist/devlanes-$version-$target.zip" | tr '\n' ' ')
	[ "$listed" = "devlanes-$version-$target/LICENSE devlanes-$version-$target/devlanes-hosts-service.exe devlanes-$version-$target/devlanes.exe devlanes-$version-$target/devlanesw.exe " ] || {
		echo "unexpected entries in the $target zip: $listed" >&2
		exit 1
	}
done
sh "$root/packaging/scripts/manifest.sh" "$version" "$work/dist" "https://example.invalid/download/v$version" >/dev/null
jq -e '.artifacts | length == 4' "$work/dist/release-manifest.json" >/dev/null
[ "$(wc -l <"$work/dist/SHA256SUMS")" -eq 6 ]

sh "$root/packaging/homebrew/render.sh" "$version" "$work/dist/SHA256SUMS" >"$work/devlanes.rb"
if command -v ruby >/dev/null 2>&1; then ruby -c "$work/devlanes.rb" >/dev/null; fi

sh "$root/packaging/scoop/render.sh" "$version" "$work/dist/SHA256SUMS" >"$work/devlanes.json"
jq -e --arg v "$version" '.version == $v and (.architecture | [.["64bit"].hash, .arm64.hash] | all(test("^[0-9a-f]{64}$")))' \
	"$work/devlanes.json" >/dev/null
sh "$root/packaging/winget/render.sh" "$version" "$work/dist/SHA256SUMS" "$work/winget" >/dev/null
set -- "$work"/winget/*.yaml
[ $# -eq 3 ]
grep -q "InstallerSha256: [0-9a-f]\{64\}$" "$work/winget/devlanes-run.devlanes.installer.yaml"

sh "$root/packaging/npm/build.sh" "$version" "$work/dist" "$work/npm" >/dev/null
for bin in devlanes.exe devlanesw.exe devlanes-hosts-service.exe; do
	[ "$(cat "$work/npm/devlanes-win32-arm64/bin/$bin")" = "fake $bin $version" ]
done
jq -e '.optionalDependencies | length == 6 and all(. == "'"$version"'")' "$work/npm/devlanes/package.json" >/dev/null
for dir in "$work"/npm/*/; do
	jq -e .name "$dir/package.json" >/dev/null
	(cd "$dir" && npm pack --dry-run >/dev/null 2>&1) || {
		echo "npm pack failed in $dir" >&2
		exit 1
	}
done

release=$work/release
mkdir -p "$release/download/v$version" "$release/latest/download"
cp "$work"/dist/*.tar.gz "$work/dist/SHA256SUMS" "$release/download/v$version/"
cp "$work/dist/SHA256SUMS" "$release/latest/download/"
DEVLANES_INSTALL_BASE_URL="file://$release" DEVLANES_INSTALL_DIR="$work/installed" \
	sh "$root/install.sh" >/dev/null
[ "$("$work/installed/devlanes")" = "devlanes $version" ]
[ -x "$work/installed/devlanes-hosts" ]

echo "packaging scripts: dry run passed"

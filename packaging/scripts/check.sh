#!/bin/sh
# Dry run of the release scripts against fake binaries: package.sh and package.ps1
# for every target, manifest.sh, the Homebrew formula, the npm packages (packed,
# then installed from the tarballs on macOS and Linux arm64/x64), the PyPI wheels
# (installed into a venv) and install.sh from a local release directory. Needs tar,
# gzip, unzip, jq, node, npm, python3 with venv, curl and pwsh, and on Linux glibc
# 2.35 or later for pip to take the wheel; CI runs it on every push.
# check.ps1 covers package.ps1, install.ps1, the launcher and the wheel on Windows.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
work=$(mktemp -d)
cleanup() { rm -rf "$work"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
trap 'exit 129' HUP

# A prerelease that PyPI also takes, so the wheels come from the same archives.
version=0.0.0-rc.1
mkdir "$work/bin"
for bin in devlanes devlanes-hosts; do
	printf '#!/bin/sh\necho "%s %s"\n' "$bin" "$version" >"$work/bin/$bin"
	chmod 755 "$work/bin/$bin"
done

for target in aarch64-apple-darwin x86_64-apple-darwin aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
	sh "$root/packaging/scripts/package.sh" "$version" "$target" "$work/dist" "$work/bin" >/dev/null
done
mkdir "$work/winbin"
printf 'fake devlanes.exe %s\n' "$version" >"$work/winbin/devlanes.exe"
for target in x86_64-pc-windows-msvc aarch64-pc-windows-msvc; do
	pwsh -NoProfile -NonInteractive -File "$root/packaging/scripts/package.ps1" \
		-Version "$version" -Target "$target" -OutDir "$work/dist" -BinDir "$work/winbin" >/dev/null
	listed=$(unzip -Z1 "$work/dist/devlanes-$version-$target.zip" | tr '\n' ' ')
	[ "$listed" = "devlanes-$version-$target/LICENSE devlanes-$version-$target/devlanes.exe " ] || {
		echo "unexpected entries in the $target zip: $listed" >&2
		exit 1
	}
done
sh "$root/packaging/scripts/manifest.sh" "$version" "$work/dist" "https://example.invalid/download/v$version" >/dev/null
jq -e '.artifacts | length == 4' "$work/dist/release-manifest.json" >/dev/null
[ "$(grep -c '\.tar\.gz$' "$work/dist/SHA256SUMS")" -eq 4 ]
[ "$(grep -c '\.zip$' "$work/dist/SHA256SUMS")" -eq 2 ]

sh "$root/packaging/homebrew/render.sh" "$version" "$work/dist/SHA256SUMS" >"$work/devlanes.rb"
if command -v ruby >/dev/null 2>&1; then ruby -c "$work/devlanes.rb" >/dev/null; fi

sh "$root/packaging/npm/build.sh" "$version" "$work/dist" "$work/npm" >/dev/null
for cpu in x64 arm64; do
	cmp -s "$work/npm/devlanes-win32-$cpu/bin/devlanes.exe" "$work/winbin/devlanes.exe"
	jq -e '.os == ["win32"] and .cpu == ["'"$cpu"'"]' "$work/npm/devlanes-win32-$cpu/package.json" >/dev/null
done
jq -e '.optionalDependencies | all(. == "'"$version"'")' "$work/npm/devlanes/package.json" >/dev/null
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
# npm's global layout and postinstall differ on Windows; check.ps1 covers the launcher there.
if [ -f "$work/tgz/devlanes-$host-$version.tgz" ] && [ "${host%%-*}" != win32 ]; then
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

pypi=$root/packaging/pypi/build.py
pep440() { python3 "$pypi" --pep440 "$1"; }
[ "$(pep440 1.2.3)" = 1.2.3 ]
[ "$(pep440 1.2.3-alpha)" = 1.2.3a0 ]
[ "$(pep440 1.2.3-beta.2)" = 1.2.3b2 ]
[ "$(pep440 1.2.3-rc.10)" = 1.2.3rc10 ]
for wrong in 1.2.3-next.4 1.2.3-check 1.2.3-rc1 1.2.3-rc.0 1.2; do
	if pep440 "$wrong" >/dev/null 2>&1; then
		echo "build.py --pep440 accepted $wrong" >&2
		exit 1
	fi
done
wheel_version=$(pep440 "$version")
python3 "$pypi" "$version" "$work/dist" "$work/wheels" >/dev/null
python3 "$pypi" "$version" "$work/dist" "$work/wheels-again" >/dev/null
for wheel in "$work"/wheels/*.whl; do
	cmp -s "$wheel" "$work/wheels-again/${wheel##*/}" || {
		echo "${wheel##*/} differs between two builds" >&2
		exit 1
	}
done
set -- "$work"/wheels/*.whl
[ $# -eq 6 ]
data=devlanes-$wheel_version.data/scripts
info=devlanes-$wheel_version.dist-info
listed=$(unzip -Z1 "$work/wheels/devlanes-$wheel_version-py3-none-manylinux_2_35_x86_64.whl" | tr '\n' ' ')
[ "$listed" = "$data/devlanes $data/devlanes-hosts $info/METADATA $info/WHEEL $info/licenses/LICENSE $info/RECORD " ] || {
	echo "unexpected entries in the Linux wheel: $listed" >&2
	exit 1
}
listed=$(unzip -Z1 "$work/wheels/devlanes-$wheel_version-py3-none-win_arm64.whl" | tr '\n' ' ')
[ "$listed" = "$data/devlanes.exe $info/METADATA $info/WHEEL $info/licenses/LICENSE $info/RECORD " ] || {
	echo "unexpected entries in the Windows wheel: $listed" >&2
	exit 1
}
# Every file is in RECORD with its hash and size, as installers expect.
python3 - "$work"/wheels/*.whl <<'EOF'
import base64, csv, hashlib, io, sys, zipfile
for path in sys.argv[1:]:
    with zipfile.ZipFile(path) as wheel:
        names = wheel.namelist()
        record = next(name for name in names if name.endswith(".dist-info/RECORD"))
        rows = list(csv.reader(io.StringIO(wheel.read(record).decode())))
        assert sorted(row[0] for row in rows) == sorted(names), path
        for name, digest, size in rows:
            if name == record:
                continue
            body = wheel.read(name)
            expected = base64.urlsafe_b64encode(hashlib.sha256(body).digest()).rstrip(b"=").decode()
            assert digest == "sha256=" + expected and int(size) == len(body), f"{path}: {name}"
EOF
# pip picks this machine's wheel and puts both binaries, executable, in the venv's bin.
python3 -m venv "$work/venv"
"$work/venv/bin/python" -m pip install --quiet --disable-pip-version-check --no-index \
	--find-links "$work/wheels" "devlanes==$wheel_version"
cmp -s "$work/venv/bin/devlanes" "$work/bin/devlanes"
[ "$("$work/venv/bin/devlanes" --version)" = "devlanes $version" ]
[ "$("$work/venv/bin/devlanes-hosts")" = "devlanes-hosts $version" ]

release=$work/release
mkdir -p "$release/download/v$version" "$release/latest/download"
cp "$work"/dist/*.tar.gz "$work/dist/SHA256SUMS" "$release/download/v$version/"
cp "$work/dist/SHA256SUMS" "$release/latest/download/"
DEVLANES_INSTALL_BASE_URL="file://$release" DEVLANES_INSTALL_DIR="$work/installed" \
	sh "$root/install.sh" >/dev/null
[ "$("$work/installed/devlanes")" = "devlanes $version" ]
[ -x "$work/installed/devlanes-hosts" ]

# The rolling `next` release keeps its archives under download/next, not download/vVERSION.
mkdir -p "$release/download/next"
cp "$work"/dist/*.tar.gz "$work/dist/SHA256SUMS" "$release/download/next/"
DEVLANES_VERSION=next DEVLANES_INSTALL_BASE_URL="file://$release" DEVLANES_INSTALL_DIR="$work/installed-next" \
	sh "$root/install.sh" >/dev/null
[ "$("$work/installed-next/devlanes")" = "devlanes $version" ]

# The same through options, as `curl | sh -s -- ...` passes them. Each source is the
# only one present, so a run that reads the wrong one fails.
install_with() {
	DEVLANES_INSTALL_BASE_URL="file://$release" DEVLANES_INSTALL_DIR="$work/installed-opt" sh -s -- "$@" <"$root/install.sh"
}
mv "$release/download/v$version" "$work/versioned"
install_with --channel next >/dev/null
mv "$work/versioned" "$release/download/v$version"
mv "$release/download/next" "$work/next"
install_with --version "$version" >/dev/null
install_with --version="v$version" >/dev/null
DEVLANES_VERSION=next install_with --channel=stable >/dev/null
[ "$("$work/installed-opt/devlanes")" = "devlanes $version" ]
mv "$work/next" "$release/download/next"
install_with -c next -d "$work/installed-short" >/dev/null
[ "$("$work/installed-short/devlanes")" = "devlanes $version" ]
[ "$(sh "$root/install.sh" --help)" = "$(sed -n "s/^usage='\(.*\)'$/\1/p" "$root/install.sh")" ]
for wrong in '--channel dev' "--channel next --version $version" '--release 1' '--version'; do
	# shellcheck disable=SC2086 # each case is several words
	if install_with $wrong >/dev/null 2>&1; then
		echo "install.sh accepted $wrong" >&2
		exit 1
	fi
done

echo "packaging scripts: dry run passed"

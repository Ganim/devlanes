# Packaging

Release archives, checksums, the SSH bootstrap manifest, the Homebrew formula and the npm packages all come from `.github/workflows/release.yml` and the scripts here; `.github/workflows/build.yml` builds the archives for it and for the `next` builds of `main`. `install.sh` and `install.ps1` at the repository root install from the same archives.

| Path | Purpose |
|---|---|
| `scripts/package.sh` | One `devlanes-VERSION-TARGET.tar.gz` per macOS and Linux target |
| `scripts/package.ps1` | One `devlanes-VERSION-TARGET.zip` per Windows target |
| `scripts/manifest.sh` | `SHA256SUMS` and `release-manifest.json` |
| `homebrew/devlanes.rb`, `homebrew/render.sh` | Formula template and the script that fills in version and checksums |
| `npm/devlanes/`, `npm/build.sh` | Launcher package and the script that builds all npm packages |
| `scripts/check.sh` | Dry run of all of the above and `install.sh` against fake binaries; CI runs it |
| `scripts/check.ps1` | PSScriptAnalyzer on the PowerShell scripts, `package.ps1`, `install.ps1` in Windows PowerShell 5.1 and PowerShell 7, and the npm launcher, against a fake `devlanes.exe`; CI runs it on Windows |

## Install channels

| Channel | Command |
|---|---|
| Homebrew | `brew install devlanes-run/tap/devlanes` |
| npm | `npm install -g devlanes` |
| cargo binstall | `cargo binstall --git https://github.com/devlanes-run/devlanes devlanes` |
| Script | `curl -fsSL https://devlanes.run/install.sh \| sh` |
| Script (Windows, PowerShell) | `irm https://devlanes.run/install.ps1 \| iex` |
| Source | `cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes devlanes-hosts-helper` (Windows: `devlanes` only) |
| Latest build of `main` | `curl -fsSL https://raw.githubusercontent.com/devlanes-run/devlanes/main/install.sh \| DEVLANES_VERSION=next sh`, or `$env:DEVLANES_VERSION = 'next'` before `install.ps1` |

Every channel except the source build installs the release archives. Homebrew has no Windows build.

## Next builds

`.github/workflows/next.yml` builds every commit on `main` after the CI workflow passes on it, with the same `build.yml` as releases. The version is the current one with the patch bumped and `-next.N` added, where N counts the commits since the last stable tag: after 0.3.0 that's `0.3.1-next.14`, which sorts after 0.3.0 and before any release that follows. That version is written into the build only, never committed. The archives, `SHA256SUMS`, `release-manifest.json`, the schemas and `install.sh` then replace the assets of the GitHub prerelease `next`, whose tag moves to that commit. Only `install.sh` reads it: npm, Homebrew, cargo binstall and the website stay on stable releases, and a prerelease is never GitHub's `latest`. Only the tip of `main` is built and published: CI runs can finish out of order, so a run for an older commit stops before building, a newer tip cancels a build still running, and a build that finishes after `main` moved isn't published. `remote connect --install` from a `next` build fetches the `next` manifest, which only matches while that build is still the latest. Repository settings must not enable immutable releases, which forbid replacing assets. Each publish is a deployment to the `next` environment.

## Releasing

Releases come from the change files in `.sampo/changesets` (`CONTRIBUTING.md` describes them). `.github/workflows/sampo.yml` runs [Sampo's GitHub Action](https://github.com/bruits/sampo/tree/main/crates/sampo-github-action) on every push to `main`:

1. While change files are pending, it keeps the pull request "Release (main)" from the branch `release/main` up to date. That runs `sampo release`, which consumes the change files, bumps the changed crates' versions, refreshes `Cargo.lock` and writes each crate's `CHANGELOG.md`. The workflow then copies the version into `agent-plugin/.claude-plugin/plugin.json` and regenerates the schemas, which name the version, on that branch.
2. Merging it leaves no change files, so the action runs `sampo publish`. The crates inherit `publish = false`, so that publishes nothing and only tags the new versions: `vX.Y.Z` for `devlanes` (`short_tags` in `.sampo/config.toml`) and `devlanes-<name>-vX.Y.Z` for the other crates. The workflow then dispatches `release.yml` on the new `vX.Y.Z` tag.

The release version is `bins/devlanes/Cargo.toml`'s. `.sampo/config.toml` keeps `devlanes` and `devlanes-hosts-helper` in a `fixed` group, so the two shipped binaries always match; library crates report the same version through `devlanes_version::VERSION`, which reads it from `bins/devlanes/Cargo.toml`. Every other crate has its own version in a `linked` group: a change to a crate bumps it and everything that depends on it up to `devlanes`, all with the biggest bump in the release, and leaves the rest alone. A `minor` change in `devlanes-daemon` releases daemon and devlanes as 0.4.0 while `devlanes-sync` stays at 0.3.0. The helper being in the `fixed` group also bumps `devlanes-resolver-hosts`, which depends on it, on every release. Don't edit versions by hand. Sampo applies plain SemVer, so until 1.0 a breaking change is `minor` (0.3.x to 0.4.0) and everything else `patch`. Release notes are GitHub's generated notes; the changelogs have the details.

`release.yml` also runs for a `v*` tag pushed by hand, and checks that the tag matches both versions. A version with a `-` suffix is a prerelease. It runs these jobs:

| Job | Does |
|---|---|
| `test` | Runs the workspace tests on Linux, macOS and Windows, in parallel with `build` |
| `build` | Runs `build.yml`. Its `build` job builds `devlanes` and `devlanes-hosts` for each macOS and Linux target, fails if the tag doesn't match the committed versions or if the committed config schemas differ from the generated ones, and packs the archive. Its `build-windows` job builds `devlanes.exe` for each Windows target, checks the schemas, runs `devlanes.exe --version`, fails if the executable imports the Visual C++ runtime (`dumpbin /dependents`), and packs the zip with `package.ps1` |
| `npm-preflight` | Fails when the `devlanes` package or one of its `optionalDependencies` does not exist on the npm registry, before any draft release exists |
| `release` | Writes `SHA256SUMS`, `release-manifest.json` and the formula, copies `schema.json`, `user-schema.json`, `install.sh` and `install.ps1`, attests the archives' build provenance, and uploads everything to a draft release. A rerun refills the draft and refuses to touch a published release |
| `publish` | Publishes the draft |
| `website` | Stable versions only. Triggers a rebuild of `devlanes-run/website`, which copies `install.sh`, `install.ps1`, `schema.json` and `user-schema.json` from the latest stable release to `https://devlanes.run/install.sh`, `https://devlanes.run/install.ps1`, `https://devlanes.run/v1/schema.json` and `https://devlanes.run/v1/user-schema.json` |
| `homebrew-verify` | Stable versions only. Installs the rendered formula on macOS from the published release, then runs `brew audit` and `brew test` |
| `homebrew-publish` | Commits `Formula/devlanes.rb` to `devlanes-run/homebrew-tap`. It keeps the tap's formula when that one belongs to a newer version |
| `npm` | Builds the npm packages from the archives and publishes the platform packages, then `devlanes`. Stable versions go to the `latest` dist-tag, prereleases to `next`. Versions already on the registry are skipped |

The release stays a draft until every asset is attached because Homebrew, npm, cargo binstall, `install.sh` and `install.ps1` all download from the public release URLs, and `releases/latest` must never point at a release with missing archives. Publishing in a separate job keeps that order visible and lets a failed upload be rerun against the draft.

Each job that publishes somewhere deploys to a GitHub environment named after that place: `publish` to `github-release`, `website` to `website`, `homebrew-publish` to `homebrew` and `npm` to `npm`. `next.yml` deploys to `next`. The repository page lists the latest deployment of each, so a release that reached GitHub but not npm shows as such.

| Target | Runner | Notes |
|---|---|---|
| `aarch64-apple-darwin` | `macos-latest` | |
| `x86_64-apple-darwin` | `macos-latest` | Cross-compiled; CI does not run it |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Built on 22.04 so the binary needs glibc 2.35, which Debian 12 and Ubuntu 22.04 provide |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | Same glibc baseline |
| `x86_64-pc-windows-msvc` | `windows-latest` | Windows 11 24H2 (build 26100) or later |
| `aarch64-pc-windows-msvc` | `windows-11-arm` | Native arm64 runner, so the smoke test runs it |

Each `.tar.gz` archive holds one directory, `devlanes-VERSION-TARGET/`, with `devlanes`, `devlanes-hosts` and `LICENSE`. `package.sh` fixes owners, file order and times (`SOURCE_DATE_EPOCH`, else the last commit's time) and writes gzip without a timestamp, so the same binaries give the same archive. Both binaries stay side by side in every package because `devlanes certs trust --hosts` copies the `devlanes-hosts` found next to `devlanes` into `/usr/local/libexec`. That installer also requires a release build: `devlanes-hosts version` must report `build=release`, which the `build` job checks on the natively built targets.

Each Windows `.zip` archive holds the same directory with `devlanes.exe` and `LICENSE`, and nothing else: `devlanes-hosts` is Unix-only, `daemon install` starts `devlanes.exe daemon run` itself (its manifest keeps a console from opening at logon), and hostnames need no helper on Windows. `.cargo/config.toml` links the C runtime statically, so the executable runs without the Visual C++ Redistributable; `build-windows` checks its imports. `package.ps1` writes the entries in ordinal order with a fixed time (`SOURCE_DATE_EPOCH`, else the last commit's time), so the same binary gives the same zip with the same PowerShell (Windows PowerShell 5.1 and PowerShell 7 ship different zlib builds; the release uses PowerShell 7). The zips are listed in `SHA256SUMS` after the `.tar.gz` archives, and `release-manifest.json` stays Unix-only. The Windows executables are not signed yet.

`devlanes daemon install` writes the path of the running executable into the launchd or systemd service (the HKCU Run key on Windows). When that path contains a version (a Homebrew Cellar directory, an npm package directory), rerun `devlanes daemon install` after upgrading.

## Verifying a download

Every archive has a build provenance attestation from the `release` job. With the GitHub CLI:

```sh
gh attestation verify devlanes-0.1.0-aarch64-apple-darwin.tar.gz --repo devlanes-run/devlanes
gh attestation verify devlanes-0.1.0-x86_64-pc-windows-msvc.zip --repo devlanes-run/devlanes
```

## Credentials

`sampo.yml` only uses the workflow's `GITHUB_TOKEN`. GitHub starts no workflows for what that token pushes, which is why the job dispatches `release.yml` itself and why CI on the release pull request waits for someone to approve the runs.

`homebrew-publish` pushes to the tap with the repository secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read and write on `devlanes-run/homebrew-tap` only. It expires on the date set when it was created; replace the secret before then. `website` sends its dispatch with `WEBSITE_DISPATCH_TOKEN`, a fine-grained token with Contents write on `devlanes-run/website` only.

The `npm` job has no npm secret. Each package (`devlanes` and the `@devlanes/*` platform packages) trusts `devlanes-run/devlanes`, workflow `release.yml`, environment `npm`, so npm exchanges the job's OIDC token for a short-lived publish token and adds provenance. Renaming `release.yml` or the job's environment breaks publishing until each package's trusted publisher names the new one. The `0.0.0` versions of the `@devlanes/*` packages are empty placeholders; the launcher pins its optional dependencies to its own version, so nothing installs them.

A trusted publisher can only be configured on a package that exists, so a new platform package is created by hand once: publish a `0.0.0` placeholder as its owner, then add the trusted publisher above, before the first release that lists it in `optionalDependencies`. `npm-preflight` stops a tag before a draft release is created when any listed package is missing from the registry.

## Release manifest

`release-manifest.json` has the shape `ReleaseManifest::from_json` in `crates/transport-ssh` accepts:

```json
{
  "version": "0.1.0",
  "artifacts": [
    {
      "os": "linux",
      "arch": "x86_64",
      "url": "https://github.com/devlanes-run/devlanes/releases/download/v0.1.0/devlanes-0.1.0-x86_64-unknown-linux-gnu.tar.gz",
      "sha256": "<64 lowercase hex digits>",
      "format": { "kind": "tar_gz", "member": "devlanes-0.1.0-x86_64-unknown-linux-gnu/devlanes" }
    }
  ]
}
```

A binary cannot embed the checksums of its own release archives, because they are computed after it is built. The manifest is therefore a release asset. `devlanes remote connect --install` downloads the manifest for its own version from `https://github.com/devlanes-run/devlanes/releases/download/vVERSION/release-manifest.json` to bootstrap a box, which trusts HTTPS to GitHub for the manifest and the manifest for the archive. Embedding the manifest at build time would need a second build after the archives exist, which changes the archives again. The daemon's update check reads only the `version` of `releases/latest/download/release-manifest.json`.

## crates.io and cargo binstall

devlanes is not published to crates.io. Every workspace crate sets `publish = false`, and the binary depends on the workspace's `devlanes-*` path crates, which crates.io would require to be published too.

`cargo binstall --git` skips crates.io: it clones the repository, reads `[package.metadata.binstall]` from the crate's manifest and downloads the release archive for the host target. `bins/devlanes/Cargo.toml` and `crates/hosts-helper/Cargo.toml` carry that metadata, so:

```sh
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes-hosts-helper
```

On Windows the overrides for `x86_64-pc-windows-msvc` and `aarch64-pc-windows-msvc` point `devlanes` at the `.zip` archive (exact targets, because older binstall versions do not read `cfg()` override keys), and `devlanes-hosts-helper` disables every strategy there, since it is Unix-only.

binstall accepts only one crate per `--git` invocation and always clones the default branch: it has no option for a tag, so it installs the release whose version is in `main`'s `Cargo.toml`. The metadata disables the third-party quick-install source. When no archive matches the host or that version, for example right after a version bump and before its tag is released, binstall falls back to compiling the crate from the clone. To pin a release, use `install.sh` with `DEVLANES_VERSION`, or build that tag from source:

```sh
cargo install --locked --git https://github.com/devlanes-run/devlanes --tag v0.1.0 devlanes devlanes-hosts-helper
```

## install.sh

`install.sh` picks the archive for the host (macOS or Linux, arm64 or x86_64; an arm64 Mac under Rosetta gets the arm64 build), reads `SHA256SUMS` from the latest release or from `DEVLANES_VERSION`, checks the archive's SHA-256 and its exact entries, and copies `devlanes` and `devlanes-hosts` to `DEVLANES_INSTALL_DIR` (default `~/.local/bin`). When that directory is not writable it stops, unless `DEVLANES_INSTALL_SUDO=1` allows sudo. Downloads use HTTPS with TLS 1.2 or later only. It refuses musl systems, does not edit shell profiles and does not update itself. `DEVLANES_INSTALL_BASE_URL` points it at a local directory of fake releases for testing. `https://devlanes.run/install.sh` serves the copy attached to the latest stable release, so changes on `main` reach users with the next release.

## install.ps1

`install.ps1` is the Windows counterpart, for Windows PowerShell 5.1 and PowerShell 7:

```powershell
irm https://devlanes.run/install.ps1 | iex
```

It reads the same `DEVLANES_VERSION`, `DEVLANES_INSTALL_DIR` and `DEVLANES_INSTALL_BASE_URL` variables (set with `$env:NAME = 'value'` before the command, since `irm | iex` passes no arguments); `DEVLANES_INSTALL_SUDO` has no Windows meaning. It refuses Windows builds below 26100 (`CurrentBuildNumber` in the registry), picks the target from the machine's architecture (the registry's machine `PROCESSOR_ARCHITECTURE`, so x64 PowerShell emulated on an arm64 machine still gets the arm64 build), checks the zip against `SHA256SUMS` and its exact entries, and installs `devlanes.exe` to `DEVLANES_INSTALL_DIR` (default `%LOCALAPPDATA%\Programs\devlanes`). The new file is extracted next to the old one first; a `devlanes.exe` that is running (usually the daemon) is renamed to `devlanes.exe.old-TIMESTAMP` and keeps running, since Windows allows renaming but not replacing a running executable. The script never stops the daemon; `devlanes daemon restart` switches to the new version, and `devlanes doctor` (or the next install) deletes old copies that nothing runs any more. Unlike `install.sh` it puts the directory first in the user `PATH`, keeping the value's registry type (`REG_EXPAND_SZ` entries such as `%USERPROFILE%\bin` keep working), and notifies running programs with `WM_SETTINGCHANGE`; the current session's `PATH` gains it too. The body runs in its own script block, so `irm | iex` leaves no variables or functions in the caller's session, and failures throw instead of calling `exit`, which would close the window. Only HTTPS is accepted, plus `file://` when `DEVLANES_INSTALL_BASE_URL` is set. Every release attaches the script, and `releases/latest/download/install.ps1` serves the latest stable release's copy.

## npm

The `devlanes` package follows the esbuild layout. Its `bin` entry is `bin/devlanes`, a Node launcher, and it lists one optional dependency per platform: `@devlanes/darwin-arm64`, `@devlanes/darwin-x64`, `@devlanes/linux-arm64`, `@devlanes/linux-x64`, `@devlanes/win32-arm64`, `@devlanes/win32-x64`. Each platform package declares `os` and `cpu` (and `libc: glibc` on Linux), so npm installs only the matching one. The Windows packages hold `bin/devlanes.exe` only.

The postinstall script `install.js` resolves the platform package from the `devlanes` package directory (which also finds it through pnpm's symlinks) and puts its `devlanes-hosts` and `devlanes` into `devlanes/bin/`, replacing the launcher. Each file is hard-linked (copied when that fails) to a temporary name in `bin/`, must report `<name> <package version>` (`devlanes --version`, `devlanes-hosts version`), and is then renamed over the target, so a reinstall or update never leaves a half-written file and never writes into a file pnpm shares with its store. npm, pnpm and Bun then run the binary directly. `devlanes-hosts` has to move too: the package manager's bin link resolves to `devlanes/bin/devlanes`, and `certs trust --hosts` looks for the helper next to the running executable and next to its resolved path, neither of which is the platform package. A symlink into the platform package would also resolve, but its relative target depends on the package manager's layout.

Any failure prints one line and keeps the launcher; the install still succeeds. The script does nothing on Windows and under Yarn: Plug'n'Play keeps packages in zip archives, and Yarn may rerun install scripts in a `node_modules` shared between platforms. Without the script (`--ignore-scripts`, or npm 12, pnpm and Bun, which run dependency scripts only when allowed) the launcher resolves the platform package, runs its `bin/devlanes` with inherited stdio and exits with the binary's status. SIGINT and SIGQUIT reach the binary from the terminal's process group; the launcher forwards SIGTERM and SIGHUP. On Windows the launcher, which npm's `devlanes.cmd` and `devlanes.ps1` shims run with Node, always runs `bin/devlanes.exe` from the platform package. Ctrl-C, Ctrl-Break and closing the console reach the binary through the shared console, so the launcher ignores SIGINT, SIGBREAK and SIGHUP and exits with the binary's exit code. An upgrade through npm replaces that `devlanes.exe`, which Windows refuses while the daemon runs from it, so on Windows the upgrade advice says to run `devlanes daemon stop` first (also for cargo and source builds, but not for `install.ps1`).

`npm/build.sh VERSION DIST_DIR OUT_DIR` extracts the release archives (the `.zip` ones with `unzip`) into `OUT_DIR/devlanes-<os>-<cpu>/` and writes `OUT_DIR/devlanes/` with the version filled in and the schemas copied in. `scripts/check.sh` installs the packed tarballs into temporary prefixes with npm and checks that the bin is the binary after postinstall and the launcher with `--ignore-scripts`.

## JSON Schemas

`crates/config/schema.json` and `crates/config/user-schema.json` are generated by `cargo build -p devlanes` and kept in Git. The project schema includes the registered preset options. Config tests reject stale generated files. Their `$id`s are `https://devlanes.run/v1/schema.json` and `https://devlanes.run/v1/user-schema.json`, the URLs that serve them. They ship in:

- every GitHub release, as `schema.json` and `user-schema.json`;
- the website, which serves the latest stable release's copies at their `$id`s. The `v1` path is fixed on the website, not derived from the release version;
- the `devlanes` npm package root, exported as `devlanes/schema.json` and `devlanes/user-schema.json`, so a project with devlanes installed can start `devlanes.yaml` with `# yaml-language-server: $schema=./node_modules/devlanes/schema.json`.

# Packaging

Release archives, checksums, the SSH bootstrap manifest, the Homebrew formula and the npm packages all come from `.github/workflows/release.yml` and the scripts here. `install.sh` at the repository root installs from the same archives.

| Path | Purpose |
|---|---|
| `scripts/package.sh` | One `devlanes-VERSION-TARGET.tar.gz` per target |
| `scripts/manifest.sh` | `SHA256SUMS` and `release-manifest.json` |
| `homebrew/devlanes.rb`, `homebrew/render.sh` | Formula template and the script that fills in version and checksums |
| `npm/devlanes/`, `npm/build.sh` | Launcher package and the script that builds all npm packages |
| `scripts/check.sh` | Dry run of all of the above and `install.sh` against fake binaries; CI runs it |

## Install channels

| Channel | Command |
|---|---|
| Homebrew | `brew install devlanes-run/tap/devlanes` |
| npm | `npm install -g devlanes` |
| cargo binstall | `cargo binstall --git https://github.com/devlanes-run/devlanes devlanes` |
| Script | `curl -fsSL https://raw.githubusercontent.com/devlanes-run/devlanes/main/install.sh \| sh` |
| Source | `cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes devlanes-hosts-helper` |

Every channel except the source build installs the release archives.

## Releasing

1. Set `version` in `[workspace.package]` of the root `Cargo.toml` and in `agent-plugin/.claude-plugin/plugin.json`, and merge.
2. Push a tag `vX.Y.Z` that matches both. The workflow fails if they differ. A version with a `-` suffix is a prerelease.

The workflow then runs these jobs:

| Job | Does |
|---|---|
| `test` | Runs the workspace tests on Linux and macOS, in parallel with `build` |
| `build` | Builds `devlanes` and `devlanes-hosts` for each target, fails if the committed config schemas differ from the generated ones, and packs the archive |
| `release` | Writes `SHA256SUMS`, `release-manifest.json` and the formula, copies `schema.json` and `user-schema.json`, attests the archives' build provenance, and uploads everything to a draft release. A rerun refills the draft and refuses to touch a published release |
| `publish` | Publishes the draft |
| `homebrew-verify` | Stable versions only. Installs the rendered formula on macOS from the published release, then runs `brew audit` and `brew test` |
| `homebrew-publish` | Commits `Formula/devlanes.rb` to `devlanes-run/homebrew-tap`. It keeps the tap's formula when that one belongs to a newer version |
| `npm` | Builds the npm packages from the archives and publishes the platform packages, then `devlanes`. Stable versions go to the `latest` dist-tag, prereleases to `next`. Versions already on the registry are skipped |

The release stays a draft until every asset is attached because Homebrew, npm, cargo binstall and `install.sh` all download from the public release URLs, and `releases/latest` must never point at a release with missing archives. Publishing in a separate job keeps that order visible and lets a failed upload be rerun against the draft.

| Target | Runner | Notes |
|---|---|---|
| `aarch64-apple-darwin` | `macos-latest` | |
| `x86_64-apple-darwin` | `macos-latest` | Cross-compiled; CI does not run it |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Built on 22.04 so the binary needs glibc 2.35, which Debian 12 and Ubuntu 22.04 provide |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | Same glibc baseline |

Each archive holds one directory, `devlanes-VERSION-TARGET/`, with `devlanes`, `devlanes-hosts` and `LICENSE`. `package.sh` fixes owners, file order and times (`SOURCE_DATE_EPOCH`, else the last commit's time) and writes gzip without a timestamp, so the same binaries give the same archive. Both binaries stay side by side in every package because `devlanes certs trust --hosts` copies the `devlanes-hosts` found next to `devlanes` into `/usr/local/libexec`. That installer also requires a release build: `devlanes-hosts version` must report `build=release`, which the `build` job checks on the natively built targets.

`devlanes daemon install` writes the path of the running executable into the launchd or systemd service. When that path contains a version (a Homebrew Cellar directory, an npm package directory), rerun `devlanes daemon install` after upgrading.

## Verifying a download

Every archive has a build provenance attestation from the `release` job. With the GitHub CLI:

```sh
gh attestation verify devlanes-0.1.0-aarch64-apple-darwin.tar.gz --repo devlanes-run/devlanes
```

## Credentials

`homebrew-publish` pushes to the tap with the repository secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read and write on `devlanes-run/homebrew-tap` only. It expires on the date set when it was created; replace the secret before then.

The `npm` job has no npm secret. Each package (`devlanes` and the `@devlanes/*` platform packages) trusts `devlanes-run/devlanes`, workflow `release.yml`, no environment, so npm exchanges the job's OIDC token for a short-lived publish token and adds provenance. Renaming `release.yml` breaks publishing until each package's trusted publisher names the new file. The `0.0.0` versions of the `@devlanes/*` packages are empty placeholders; the launcher pins its optional dependencies to its own version, so nothing installs them.

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

A binary cannot embed the checksums of its own release archives, because they are computed after it is built. The manifest is therefore a release asset. The client can download the manifest for its own version from `https://github.com/devlanes-run/devlanes/releases/download/vVERSION/release-manifest.json` when it needs to bootstrap a box, which trusts HTTPS to GitHub for the manifest and the manifest for the archive. Embedding the manifest at build time would need a second build after the archives exist, which changes the archives again. Nothing in the client reads the manifest yet.

## crates.io and cargo binstall

devlanes is not published to crates.io. Every workspace crate sets `publish = false`, and the binary depends on the workspace's `devlanes-*` path crates, which crates.io would require to be published too.

`cargo binstall --git` skips crates.io: it clones the repository, reads `[package.metadata.binstall]` from the crate's manifest and downloads the release archive for the host target. `bins/devlanes/Cargo.toml` and `crates/hosts-helper/Cargo.toml` carry that metadata, so:

```sh
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes-hosts-helper
```

binstall accepts only one crate per `--git` invocation and always clones the default branch: it has no option for a tag, so it installs the release whose version is in `main`'s `Cargo.toml`. The metadata disables the third-party quick-install source. When no archive matches the host or that version, for example right after a version bump and before its tag is released, binstall falls back to compiling the crate from the clone. To pin a release, use `install.sh` with `DEVLANES_VERSION`, or build that tag from source:

```sh
cargo install --locked --git https://github.com/devlanes-run/devlanes --tag v0.1.0 devlanes devlanes-hosts-helper
```

## install.sh

`install.sh` picks the archive for the host (macOS or Linux, arm64 or x86_64; an arm64 Mac under Rosetta gets the arm64 build), reads `SHA256SUMS` from the latest release or from `DEVLANES_VERSION`, checks the archive's SHA-256 and its exact entries, and copies `devlanes` and `devlanes-hosts` to `DEVLANES_INSTALL_DIR` (default `~/.local/bin`). When that directory is not writable it stops, unless `DEVLANES_INSTALL_SUDO=1` allows sudo. Downloads use HTTPS with TLS 1.2 or later only. It refuses musl systems, does not edit shell profiles and does not update itself. `DEVLANES_INSTALL_BASE_URL` points it at a local directory of fake releases for testing.

## npm

The `devlanes` package follows the esbuild and Biome layout. It contains a Node launcher (`bin/devlanes.js`) and lists one optional dependency per platform: `@devlanes/darwin-arm64`, `@devlanes/darwin-x64`, `@devlanes/linux-arm64`, `@devlanes/linux-x64`. Each platform package declares `os` and `cpu` (and `libc: glibc` on Linux), so npm installs only the matching one. The launcher resolves that package, runs its `bin/devlanes` with inherited stdio and exits with the binary's status. SIGINT and SIGQUIT reach the binary from the terminal's process group; the launcher forwards SIGTERM and SIGHUP.

`npm/build.sh VERSION DIST_DIR OUT_DIR` extracts the release archives into `OUT_DIR/devlanes-<os>-<cpu>/` and writes `OUT_DIR/devlanes/` with the version filled in and the schemas copied in.

## JSON Schemas

`crates/config/schema.json` and `crates/config/user-schema.json` are generated by `cargo build -p devlanes` and kept in Git. The project schema includes the registered preset options. Config tests reject stale generated files. Their `$id`s are `https://devlanes.run/schema/v1.json` and `https://devlanes.run/schema/user-v1.json`. They ship in:

- every GitHub release, as `schema.json` and `user-schema.json`;
- the `devlanes` npm package root, exported as `devlanes/schema.json` and `devlanes/user-schema.json`, so a project with devlanes installed can start `devlanes.yaml` with `# yaml-language-server: $schema=./node_modules/devlanes/schema.json`.

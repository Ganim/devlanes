# Packaging

Release archives, checksums, the SSH bootstrap manifest, the Homebrew formula and the npm packages all come from `.github/workflows/release.yml` and the scripts here. `install.sh` at the repository root installs from the same archives.

| Path | Purpose |
|---|---|
| `scripts/package.sh` | One `devlanes-VERSION-TARGET.tar.gz` per target |
| `scripts/manifest.sh` | `SHA256SUMS` and `release-manifest.json` |
| `scripts/export-schema.sh` | Copies the committed config schemas as `v1.json` and `user-v1.json` |
| `homebrew/devlanes.rb`, `homebrew/render.sh` | Formula template and the script that fills in version and checksums |
| `npm/devlanes/`, `npm/build.sh` | Launcher package and the script that builds all npm packages |

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

1. Set `version` in `[workspace.package]` of the root `Cargo.toml` and merge.
2. Push a tag `vX.Y.Z` that matches it. The workflow fails if they differ. A version with a `-` suffix is a prerelease.

The workflow then runs these jobs:

| Job | Does |
|---|---|
| `build` | Builds `devlanes` and `devlanes-hosts` for each target and packs the archive |
| `release` | Writes `SHA256SUMS`, `release-manifest.json` and the formula, copies `schema.json` and `user-schema.json`, and uploads everything to a draft release. A rerun refills the draft and refuses to touch a published release |
| `publish` | Publishes the draft |
| `homebrew-verify` | Stable versions only. Installs the rendered formula on macOS from the published release, then runs `brew audit` and `brew test` |
| `homebrew-publish` | Commits `Formula/devlanes.rb` to `devlanes-run/homebrew-tap`. It keeps the tap's formula when that one belongs to a newer version |
| `npm` | Builds the npm packages from the archives and publishes the four platform packages, then `devlanes`. Stable versions go to the `latest` dist-tag, prereleases to `next`. Versions already on the registry are skipped |

The release stays a draft until every asset is attached because Homebrew, npm, cargo binstall and `install.sh` all download from the public release URLs, and `releases/latest` must never point at a release with missing archives. Publishing in a separate job keeps that order visible and lets a failed upload be rerun against the draft.

| Target | Runner | Notes |
|---|---|---|
| `aarch64-apple-darwin` | `macos-latest` | |
| `x86_64-apple-darwin` | `macos-latest` | Cross-compiled; CI does not run it |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Built on 22.04 so the binary needs glibc 2.35, which Debian 12 and Ubuntu 22.04 provide |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | Same glibc baseline |

Each archive holds one directory, `devlanes-VERSION-TARGET/`, with `devlanes`, `devlanes-hosts` and `LICENSE`. The two binaries stay side by side in every package because `devlanes trust --hosts` copies the `devlanes-hosts` found next to `devlanes` into `/usr/local/libexec`. That installer also requires a release build: `devlanes-hosts version` must report `build=release`, which the `build` job checks on the natively built targets.

`devlanes daemon install` writes the path of the running executable into the launchd or systemd service. When that path contains a version (a Homebrew Cellar directory, an npm package directory), rerun `devlanes daemon install` after upgrading.

## One-time setup

### Homebrew tap token

`homebrew-publish` pushes with the repository secret `HOMEBREW_TAP_TOKEN`. Create a fine-grained personal access token with:

- resource owner: `devlanes-run`;
- repository access: only `devlanes-run/homebrew-tap`;
- repository permissions: Contents, read and write (Metadata read-only is added automatically).

Store it in `devlanes-run/devlanes` under Settings, Secrets and variables, Actions, as `HOMEBREW_TAP_TOKEN`. If the organization requires approval for fine-grained tokens, approve the request. The token expires on the date chosen at creation; renew it before then.

### npm trusted publishing

The `npm` job has no npm secret. It runs with `id-token: write`, and npm exchanges the job's OIDC token for a short-lived publish token and adds provenance. This needs npm 11.5.1 or later and Node 22.14.0 or later; the job installs npm 11 on Node 24.

Each of the five packages needs its own trusted publisher on npmjs.com: `devlanes`, `@devlanes/darwin-arm64`, `@devlanes/darwin-x64`, `@devlanes/linux-arm64` and `@devlanes/linux-x64`. On the package page, open Settings, Trusted publishing, choose GitHub Actions and enter:

| Field | Value |
|---|---|
| Organization or user | `devlanes-run` |
| Repository | `devlanes` |
| Workflow filename | `release.yml` |
| Environment name | leave empty |
| Allowed actions | `npm publish` (new configurations allow only `npm stage publish` until this is selected) |

The same from a terminal, with npm 11.15.0 or later and 2FA on the account:

```sh
for pkg in devlanes @devlanes/darwin-arm64 @devlanes/darwin-x64 @devlanes/linux-arm64 @devlanes/linux-x64; do
  npm trust github "$pkg" --repo devlanes-run/devlanes --file release.yml --allow-publish --yes
  sleep 2
done
```

npm only accepts a trusted publisher for a package that already exists, and a new configuration expires unless a publish uses it within 2 days. `devlanes` exists (0.0.1); the four `@devlanes/*` packages do not. Publish a placeholder `0.0.0` of each from a maintainer machine first:

```sh
for platform in darwin-arm64 darwin-x64 linux-arm64 linux-x64; do
  dir=$(mktemp -d)
  printf '{"name":"@devlanes/%s","version":"0.0.0","description":"Placeholder before the first devlanes release","license":"Apache-2.0","repository":{"type":"git","url":"git+https://github.com/devlanes-run/devlanes.git"}}\n' "$platform" >"$dir/package.json"
  npm publish "$dir" --access public
done
```

Then add the five trusted publishers and push the first release tag within 2 days. The placeholders contain only a `package.json`, and the launcher pins its optional dependencies to its own version, so nothing ever installs them. After the first trusted publish succeeds, set Settings, Publishing access to "Require two-factor authentication and disallow tokens" on each package.

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

devlanes is not published to crates.io. Every workspace crate sets `publish = false`, and the binary depends on about 50 `devlanes-*` path crates, which crates.io would require to be published too.

`cargo binstall --git` skips crates.io: it clones the repository, reads `[package.metadata.binstall]` from the crate's manifest and downloads the release archive for the host target. `bins/devlanes/Cargo.toml` and `crates/hosts-helper/Cargo.toml` carry that metadata, so:

```sh
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes
cargo binstall --git https://github.com/devlanes-run/devlanes devlanes-hosts-helper
```

binstall accepts only one crate per `--git` invocation. The metadata disables the third-party quick-install source. When no archive matches the host or version, binstall falls back to compiling the crate from the clone.

## install.sh

`install.sh` picks the archive for the host (macOS or Linux, arm64 or x86_64; an arm64 Mac under Rosetta gets the arm64 build), reads `SHA256SUMS` from the latest release or from `DEVLANES_VERSION`, checks the archive's SHA-256 and its exact entries, and copies `devlanes` and `devlanes-hosts` to `DEVLANES_INSTALL_DIR` (default `~/.local/bin`). It uses sudo only when that directory is not writable, and says so first. It refuses musl systems, does not edit shell profiles and does not update itself. `DEVLANES_INSTALL_BASE_URL` points it at a local directory of fake releases for testing.

## npm

The `devlanes` package follows the esbuild and Biome layout. It contains a Node launcher (`bin/devlanes.js`) and lists one optional dependency per platform: `@devlanes/darwin-arm64`, `@devlanes/darwin-x64`, `@devlanes/linux-arm64`, `@devlanes/linux-x64`. Each platform package declares `os` and `cpu` (and `libc: glibc` on Linux), so npm installs only the matching one. The launcher resolves that package, runs its `bin/devlanes` with inherited stdio and exits with the binary's status. SIGINT and SIGQUIT reach the binary from the terminal's process group; the launcher forwards SIGTERM and SIGHUP.

`npm/build.sh VERSION DIST_DIR OUT_DIR` extracts the release archives into `OUT_DIR/devlanes-<os>-<cpu>/` and writes `OUT_DIR/devlanes/` with the version filled in and the two schemas copied in.

## JSON Schemas

`crates/config/schema.json` and `crates/config/user-schema.json` are generated by `cargo build -p devlanes` and kept in Git. The project schema includes the registered preset options. Config tests reject stale generated files. They ship in three places:

- every GitHub release, as `schema.json` and `user-schema.json`;
- the `devlanes` npm package root, exported as `devlanes/schema.json` and `devlanes/user-schema.json`, so a project with devlanes installed can start `devlanes.yaml` with `# yaml-language-server: $schema=./node_modules/devlanes/schema.json`;
- `https://devlanes.run/schema/v1.json` and `https://devlanes.run/schema/user-v1.json`, the schemas' `$id`s, once the site serves them. `sh packaging/scripts/export-schema.sh site/schema` copies them under those names without running a binary.

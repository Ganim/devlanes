# Packaging

Release archives, checksums, the SSH bootstrap manifest, the Homebrew formula, the npm packages and the JSON Schemas all come from `.github/workflows/release.yml` and the scripts here.

| Path | Purpose |
|---|---|
| `scripts/package.sh` | One `devlanes-VERSION-TARGET.tar.gz` per target |
| `scripts/manifest.sh` | `SHA256SUMS` and `release-manifest.json` |
| `scripts/export-schema.sh` | `v1.json` and `user-v1.json` from `devlanes schema` |
| `homebrew/devlanes.rb`, `homebrew/render.sh` | Formula template and the script that fills in version and checksums |
| `npm/devlanes/`, `npm/build.sh` | Launcher package and the script that builds the platform packages |
| `schemastore/catalog-entry.json` | Entries for the SchemaStore catalog |

## Releasing

1. Set `version` in `[workspace.package]` of the root `Cargo.toml` and merge.
2. Push a tag `vX.Y.Z` that matches it. The workflow fails if they differ.
3. The workflow builds `devlanes` and `devlanes-hosts` in release mode for each target, packs them, and creates a draft GitHub release with the archives, `SHA256SUMS`, `release-manifest.json`, `v1.json` and `user-v1.json`. A version with a `-` suffix becomes a prerelease.
4. The run's `packages` artifact holds the rendered `devlanes.rb` and the npm package directories. Publishing them needs credentials and is manual:
   - publish the draft release;
   - copy `devlanes.rb` to `Formula/devlanes.rb` in `devlanes-run/homebrew-tap`;
   - publish the npm packages, platform packages first so the launcher's optional dependencies resolve:
     ```sh
     for pkg in packages/npm/devlanes-*/; do npm publish --access public "$pkg"; done
     npm publish packages/npm/devlanes
     ```
   - upload `v1.json` and `user-v1.json` to `https://devlanes.run/schema/`.

| Target | Runner | Notes |
|---|---|---|
| `aarch64-apple-darwin` | `macos-latest` | |
| `x86_64-apple-darwin` | `macos-latest` | Cross-compiled; CI does not run it |
| `x86_64-unknown-linux-gnu` | `ubuntu-22.04` | Built on 22.04 so the binary needs glibc 2.35, which Debian 12 and Ubuntu 22.04 provide |
| `aarch64-unknown-linux-gnu` | `ubuntu-22.04-arm` | Same glibc baseline |

Each archive holds one directory, `devlanes-VERSION-TARGET/`, with `devlanes`, `devlanes-hosts` and `LICENSE`. The two binaries stay side by side in every package because `devlanes trust --hosts` copies the `devlanes-hosts` found next to `devlanes` into `/usr/local/libexec`.

`devlanes daemon install` writes the path of the running executable into the launchd or systemd service. When that path contains a version (a Homebrew Cellar directory, an npm package directory), rerun `devlanes daemon install` after upgrading.

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

## Release profile

The root `Cargo.toml` has no `[profile.release]` yet. Suggested:

```toml
[profile.release]
lto = "thin"
codegen-units = 1
strip = true
```

`devlanes-hosts` reports `build=release` from any profile without debug assertions, which the hosts installer requires.

## cargo install

`cargo install devlanes` does not work today. Every workspace crate sets `publish = false`, and the binary depends on about 50 `devlanes-*` path crates. crates.io accepts a crate only when all of its dependencies are published too.

The options are publishing every crate under its `devlanes-` name, in dependency order and in lockstep on each release, or folding the crates into a few published ones. Publishing everything also turns each internal crate API into a public, versioned one. Folding undoes the per-integration crate layout the workspace is built on.

Recommendation: ship binaries through GitHub releases, Homebrew and npm now, and do not advertise `cargo install devlanes`. Rust users can build from Git, which resolves the path dependencies inside the repository:

```sh
cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes
cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes-hosts-helper
```

Publish the `devlanes-*` crates once their APIs settle.

## npm

The `devlanes` package follows the esbuild and Biome layout. It contains a Node launcher (`bin/devlanes.js`) and lists one optional dependency per platform: `@devlanes/darwin-arm64`, `@devlanes/darwin-x64`, `@devlanes/linux-arm64`, `@devlanes/linux-x64`. Each platform package declares `os` and `cpu` (and `libc: glibc` on Linux), so npm installs only the matching one. The launcher resolves that package, runs its `bin/devlanes` with inherited stdio and exits with the binary's status. SIGINT and SIGQUIT reach the binary from the terminal's process group; the launcher forwards SIGTERM and SIGHUP.

`npm/build.sh VERSION DIST_DIR OUT_DIR` extracts the release archives into `OUT_DIR/devlanes-<os>-<cpu>/` and writes `OUT_DIR/devlanes/` with the version filled in.

## JSON Schemas and SchemaStore

The daemon answers `devlanes schema` with `{"project": ..., "user": ...}`. The project schema includes the options of the presets built into the binary, and its `$id` is `https://devlanes.run/schema/v1.json`; the user schema's `$id` is `https://devlanes.run/schema/user-v1.json`. To write both files:

```sh
packaging/scripts/export-schema.sh site/schema target/release/devlanes
```

The script needs `jq`. It runs a temporary daemon in a scratch `DEVLANES_HOME` and stops it afterwards, so it does not touch a running daemon.

`schemastore/catalog-entry.json` holds the two entries to add to `src/api/json/catalog.json` in `SchemaStore/schemastore`, once the schemas are served from those URLs. The project entry matches `devlanes.yaml` and `devlanes.local.yaml`, the only names the config loader reads; `devlanes.yml` is left out for that reason. The user entry matches `**/.devlanes/config.yaml`. Nothing has been submitted.

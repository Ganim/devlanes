# Packaging

Release archives, checksums, the SSH bootstrap manifest, the Homebrew formula and the npm packages all come from `.github/workflows/release.yml` and the scripts here. `install.sh` at the repository root installs from the same archives.

| Path | Purpose |
|---|---|
| `scripts/package.sh` | One `devlanes-VERSION-TARGET.tar.gz` per target |
| `scripts/manifest.sh` | `SHA256SUMS` and `release-manifest.json` |
| `homebrew/devlanes.rb`, `homebrew/render.sh` | Formula template and the script that fills in version and checksums |
| `npm/devlanes/`, `npm/build.sh` | Launcher package and the script that builds all npm packages |
| `scripts/check.sh` | Dry run of all of the above and `install.sh` against fake binaries; CI runs it |
| `scripts/package.ps1`, `scripts/check.ps1` | Windows archives and their dry run; see [Windows](#windows) |

## Install channels

| Channel | Command |
|---|---|
| Homebrew | `brew install devlanes-run/tap/devlanes` |
| npm | `npm install -g devlanes` |
| cargo binstall | `cargo binstall --git https://github.com/devlanes-run/devlanes devlanes` |
| Script | `curl -fsSL https://devlanes.run/install.sh \| sh` |
| Source | `cargo install --locked --git https://github.com/devlanes-run/devlanes devlanes devlanes-hosts-helper` |

Every channel except the source build installs the release archives.

## Releasing

1. Set `version` in `[workspace.package]` of the root `Cargo.toml` and in `agent-plugin/.claude-plugin/plugin.json`, and merge.
2. Push a tag `vX.Y.Z` that matches both. The workflow fails if they differ. A version with a `-` suffix is a prerelease.

The workflow then runs these jobs:

| Job | Does |
|---|---|
| `test` | Runs the workspace tests on Linux and macOS, in parallel with `build` |
| `build` | Builds `devlanes` and `devlanes-hosts` for each target (the three `.exe` files on Windows), fails if the committed config schemas differ from the generated ones, and packs the archive (on Windows it uploads the executables instead) |
| `windows` | Signs the Windows executables when signing is configured and packs the two zip archives |
| `release` | Writes `SHA256SUMS`, `release-manifest.json` and the formula, copies `schema.json`, `user-schema.json`, `install.sh` and `install.ps1`, attests the archives' build provenance, and uploads everything to a draft release. A rerun refills the draft and refuses to touch a published release |
| `publish` | Publishes the draft |
| `website` | Stable versions only. Triggers a rebuild of `devlanes-run/website`, which copies `install.sh`, `schema.json` and `user-schema.json` from the latest stable release to `https://devlanes.run/install.sh`, `https://devlanes.run/v1/schema.json` and `https://devlanes.run/v1/user-schema.json` |
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
| `x86_64-pc-windows-msvc` | `windows-latest` | Static C runtime |
| `aarch64-pc-windows-msvc` | `windows-11-arm` | Static C runtime |

Each archive holds one directory, `devlanes-VERSION-TARGET/`, with `devlanes`, `devlanes-hosts` and `LICENSE`. `package.sh` fixes owners, file order and times (`SOURCE_DATE_EPOCH`, else the last commit's time) and writes gzip without a timestamp, so the same binaries give the same archive. Both binaries stay side by side in every package because `devlanes certs trust --hosts` copies the `devlanes-hosts` found next to `devlanes` into `/usr/local/libexec`. That installer also requires a release build: `devlanes-hosts version` must report `build=release`, which the `build` job checks on the natively built targets.

`devlanes daemon install` writes the path of the running executable into the launchd or systemd service. When that path contains a version (a Homebrew Cellar directory, an npm package directory), rerun `devlanes daemon install` after upgrading.

## Verifying a download

Every archive has a build provenance attestation from the `release` job. With the GitHub CLI:

```sh
gh attestation verify devlanes-0.1.0-aarch64-apple-darwin.tar.gz --repo devlanes-run/devlanes
```

## Credentials

`homebrew-publish` pushes to the tap with the repository secret `HOMEBREW_TAP_TOKEN`, a fine-grained token with Contents read and write on `devlanes-run/homebrew-tap` only. It expires on the date set when it was created; replace the secret before then. `website` sends its dispatch with `WEBSITE_DISPATCH_TOKEN`, a fine-grained token with Contents write on `devlanes-run/website` only.

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

A binary cannot embed the checksums of its own release archives, because they are computed after it is built. The manifest is therefore a release asset. `devlanes remote connect --install` downloads the manifest for its own version from `https://github.com/devlanes-run/devlanes/releases/download/vVERSION/release-manifest.json` to bootstrap a box, which trusts HTTPS to GitHub for the manifest and the manifest for the archive. Embedding the manifest at build time would need a second build after the archives exist, which changes the archives again. The daemon's update check reads only the `version` of `releases/latest/download/release-manifest.json`.

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

`install.sh` picks the archive for the host (macOS or Linux, arm64 or x86_64; an arm64 Mac under Rosetta gets the arm64 build), reads `SHA256SUMS` from the latest release or from `DEVLANES_VERSION`, checks the archive's SHA-256 and its exact entries, and copies `devlanes` and `devlanes-hosts` to `DEVLANES_INSTALL_DIR` (default `~/.local/bin`). When that directory is not writable it stops, unless `DEVLANES_INSTALL_SUDO=1` allows sudo. Downloads use HTTPS with TLS 1.2 or later only. It refuses musl systems, does not edit shell profiles and does not update itself. `DEVLANES_INSTALL_BASE_URL` points it at a local directory of fake releases for testing. `https://devlanes.run/install.sh` serves the copy attached to the latest stable release, so changes on `main` reach users with the next release.

## npm

The `devlanes` package follows the esbuild and Biome layout. It contains a Node launcher (`bin/devlanes.js`) and lists one optional dependency per platform: `@devlanes/darwin-arm64`, `@devlanes/darwin-x64`, `@devlanes/linux-arm64`, `@devlanes/linux-x64`. Each platform package declares `os` and `cpu` (and `libc: glibc` on Linux), so npm installs only the matching one. The launcher resolves that package, runs its `bin/devlanes` with inherited stdio and exits with the binary's status. SIGINT and SIGQUIT reach the binary from the terminal's process group; the launcher forwards SIGTERM and SIGHUP.

`npm/build.sh VERSION DIST_DIR OUT_DIR` extracts the release archives into `OUT_DIR/devlanes-<os>-<cpu>/` and writes `OUT_DIR/devlanes/` with the version filled in and the schemas copied in.

## Windows

Windows releases install through `install.ps1`, npm and cargo binstall.

Each Windows archive, `devlanes-VERSION-TARGET.zip`, holds one directory of the same name with `devlanes.exe`, `devlanesw.exe` (the logon launcher, package `devlanes-logon-launcher`), `devlanes-hosts-service.exe` and `LICENSE`. The three executables must stay side by side: `daemon install` and `certs trust --hosts` look for the other two next to `devlanes.exe`. They link the C runtime statically (`.cargo/config.toml`), so no Visual C++ Redistributable is needed.

`scripts/package.ps1 -Version V -Target T -OutDir DIR [-BinDir DIR]` writes the zip with fixed entry order, times (`SOURCE_DATE_EPOCH`, else the last commit's time) and attributes, so the same binaries give the same bytes under the same PowerShell version (Windows PowerShell 5.1 and PowerShell 7 compress differently). It uses .NET's `ZipArchive` rather than a shell script, because Git for Windows has no `zip` and `Compress-Archive` cannot fix times. `SHA256SUMS` lists the zips; `release-manifest.json` does not, because SSH bootstrap installs onto Linux and macOS boxes.

### Signing

The `windows` job signs all six executables (both targets) with Azure Artifact Signing (formerly Trusted Signing) before packing, so the archives, binstall and the npm packages (copied from the archives) carry signed files. `Get-AuthenticodeSignature` must then report every file `Valid`. It runs on `windows-latest` because the signing action does not support arm64 Windows runners. Without the settings below the job prints a warning and packs unsigned files.

| Name | Kind | Value |
|---|---|---|
| `AZURE_TENANT_ID` | secret | Tenant of the app registration |
| `AZURE_CLIENT_ID` | secret | App registration with the Artifact Signing Certificate Profile Signer role, and a federated credential for the subject `repo:devlanes-run/devlanes:environment:signing` (the job runs in the `signing` environment, so the subject stays the same for every tag) |
| `AZURE_SIGNING_ENDPOINT` | variable | Regional endpoint of the signing account, such as `https://eus.codesigning.azure.net/` |
| `AZURE_SIGNING_ACCOUNT` | variable | Signing account name |
| `AZURE_SIGNING_PROFILE` | variable | Certificate profile name |

`azure/login` signs in with the job's OIDC token (`allow-no-subscriptions`), so no client secret exists.

### install.ps1

`install.ps1` at the repository root is attached to every release, so this works once a release has it:

```powershell
irm https://github.com/devlanes-run/devlanes/releases/latest/download/install.ps1 | iex
```

It picks the archive for the machine (an x64 PowerShell emulated on arm64 gets the arm64 build), reads `SHA256SUMS` from the latest release or from `DEVLANES_VERSION`, checks the archive's SHA-256 and its exact entries, and installs into `DEVLANES_INSTALL_DIR` (default `%LOCALAPPDATA%\Programs\devlanes`). It puts that directory first in the user `PATH` (kept as `REG_EXPAND_SZ`) unless `DEVLANES_NO_MODIFY_PATH=1` or `-NoModifyPath`. Windows refuses to overwrite a running executable but lets it be renamed, so an executable in use (usually the daemon's `devlanes.exe`) becomes `devlanes.exe.old-TIMESTAMP` and keeps running; the script prints that `devlanes daemon restart` switches over, and `devlanes doctor` deletes the old file once nothing runs it. It never stops the daemon. It runs in Windows PowerShell 5.1 (with TLS 1.2 forced on) and PowerShell 7. `DEVLANES_INSTALL_BASE_URL` accepts `http://` and `file://` for tests. The update notice names this URL until the website serves `install.ps1` the way it serves `install.sh`.

`scripts/check.ps1` tests the script's helpers (`PATH` editing on fake values, architecture, `SHA256SUMS` parsing), packs twice and compares the hashes, and installs from a `file://` release with `DEVLANES_NO_MODIFY_PATH=1`, as a file and through `Invoke-Expression`. With `-BinDir` pointing at built binaries it also reinstalls while a daemon (with its own `DEVLANES_HOME` and ports) runs from the installed `devlanes.exe`, and checks the rename, that the daemon survived, and that `doctor` deletes the old file after `daemon stop`. The `packaging-windows` CI job runs it after PSScriptAnalyzer (settings in `PSScriptAnalyzerSettings.psd1`).

### npm on Windows

The `npm` release job publishes `@devlanes/win32-x64` and `@devlanes/win32-arm64` with the other platform packages, with `devlanes.exe`, `devlanesw.exe` and `devlanes-hosts-service.exe` in `bin\`. Both packages must exist (a `0.0.0` placeholder) with the same trusted publisher as the others before the first release that lists them; otherwise the `npm` job fails before publishing `devlanes`.

### cargo binstall on Windows

`devlanes`, `devlanes-logon-launcher` and `devlanes-hosts-service` each carry binstall metadata that points at the zip, one `cargo binstall --git` run per crate, so all three executables land side by side in `~\.cargo\bin`. `devlanes-hosts-helper` refuses the Windows targets, and the launcher and the service refuse the macOS and Linux targets.

npm and cargo binstall cannot replace `devlanes.exe` while the daemon runs from it, and neither can `cargo install`, so on Windows the update notice and `doctor` say to run `devlanes daemon stop` before the upgrade command. Only `install.ps1` renames the running file instead.

The npm launcher runs `bin\devlanes.exe`. Ctrl-C, Ctrl-Break and closing the console reach the binary directly because it shares the console, so the launcher only ignores `SIGINT`, `SIGBREAK` and `SIGHUP` and exits with the binary's exit code, including NTSTATUS codes such as `0xC000013A`.

## JSON Schemas

`crates/config/schema.json` and `crates/config/user-schema.json` are generated by `cargo build -p devlanes` and kept in Git. The project schema includes the registered preset options. Config tests reject stale generated files. Their `$id`s are `https://devlanes.run/v1/schema.json` and `https://devlanes.run/v1/user-schema.json`, the URLs that serve them. They ship in:

- every GitHub release, as `schema.json` and `user-schema.json`;
- the website, which serves the latest stable release's copies at their `$id`s. The `v1` path is fixed on the website, not derived from the release version;
- the `devlanes` npm package root, exported as `devlanes/schema.json` and `devlanes/user-schema.json`, so a project with devlanes installed can start `devlanes.yaml` with `# yaml-language-server: $schema=./node_modules/devlanes/schema.json`.

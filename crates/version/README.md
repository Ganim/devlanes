# devlanes-version

What identifies a devlanes release: the version, the build ID and the repository URLs. Crates that report the product version (the client/daemon handshake, update checks, the remote tunnel, the TUI) use it instead of their own `CARGO_PKG_VERSION`, because each crate keeps its own version.

## Usage

```rust
let label = format!("devlanes {} ({})", devlanes_version::VERSION, devlanes_version::BUILD_ID);
let manifest = devlanes_version::LATEST_RELEASE_MANIFEST_URL;
```

## API

- `VERSION`: the release version, such as `0.4.0`.
- `BUILD_ID`: `DEVLANES_BUILD_ID` at compile time (the release tag in official builds), else `local`.
- `REPOSITORY`: the GitHub repository releases come from.
- `LATEST_RELEASE_MANIFEST_URL`: the manifest of the latest stable release, which the update check reads.
- `MINIMAL_CONFIG_URL`: the minimal example config on the default branch.

## Versioning

`build.rs` reads the version from `bins/devlanes/Cargo.toml`, which `.sampo/config.toml` keeps in the `fixed` group with `devlanes-hosts-helper`. This crate's own version stays put, so depending on it doesn't bump a crate on every release.

## License

Apache-2.0

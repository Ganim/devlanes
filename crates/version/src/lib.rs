//! What identifies a devlanes release: its version, the build that produced it and
//! where it is published.

/// The repository URL as a literal, so the URLs below can be built with `concat!`.
macro_rules! repository {
    () => {
        "https://github.com/devlanes-run/devlanes"
    };
}

/// The release version of devlanes, such as `0.4.0`: the version in
/// `bins/devlanes/Cargo.toml`, which `build.rs` reads. Crates keep their own package
/// versions, which only move when they or their dependencies change.
pub const VERSION: &str = env!("DEVLANES_RELEASE_VERSION");

/// The release tag of an official build, such as `v0.4.0`, from `DEVLANES_BUILD_ID` at
/// compile time; `local` for every other build. The daemon handshake and `doctor` show it.
pub const BUILD_ID: &str = match option_env!("DEVLANES_BUILD_ID") {
    Some(id) => id,
    None => "local",
};

/// The GitHub repository that releases are published from and that `cargo install --git`
/// and `cargo binstall --git` build from.
pub const REPOSITORY: &str = repository!();

/// A minimal `devlanes.yaml` on the default branch, for users who have none yet.
pub const MINIMAL_CONFIG_URL: &str =
    concat!(repository!(), "/blob/main/docs/examples/minimal.yaml");

/// The manifest of the latest stable release. GitHub's `latest` skips drafts and
/// prereleases, `next` included.
pub const LATEST_RELEASE_MANIFEST_URL: &str = concat!(
    repository!(),
    "/releases/latest/download/release-manifest.json"
);

/// The manifest of the release `version` (without a leading `v`), which lists its
/// archives and their checksums. A build of `main` (`X.Y.Z-next.N`) has no release of
/// its own, so its manifest is the rolling `next` one, which may already list a newer build.
pub fn release_manifest_url(version: &str) -> String {
    if version.contains("-next.") {
        format!("{REPOSITORY}/releases/download/next/release-manifest.json")
    } else {
        format!("{REPOSITORY}/releases/download/v{version}/release-manifest.json")
    }
}

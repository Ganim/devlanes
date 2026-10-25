use devlanes_version::{
    BUILD_ID, LATEST_RELEASE_MANIFEST_URL, MINIMAL_CONFIG_URL, REPOSITORY, VERSION,
    release_manifest_url,
};

#[test]
fn version_is_the_binarys() {
    let manifest = include_str!("../../../../bins/devlanes/Cargo.toml");
    assert!(
        manifest
            .lines()
            .any(|line| line == format!("version = \"{VERSION}\"")),
        "bins/devlanes/Cargo.toml does not declare version {VERSION}"
    );
    assert!(!BUILD_ID.is_empty());
}

#[test]
fn urls_point_at_the_repository() {
    assert_eq!(REPOSITORY, "https://github.com/devlanes-run/devlanes");
    assert_eq!(
        LATEST_RELEASE_MANIFEST_URL,
        format!("{REPOSITORY}/releases/latest/download/release-manifest.json")
    );
    assert_eq!(
        release_manifest_url("0.4.0"),
        format!("{REPOSITORY}/releases/download/v0.4.0/release-manifest.json")
    );
    assert_eq!(
        release_manifest_url("0.4.1-next.12"),
        format!("{REPOSITORY}/releases/download/next/release-manifest.json")
    );
    assert!(MINIMAL_CONFIG_URL.starts_with(REPOSITORY));
}

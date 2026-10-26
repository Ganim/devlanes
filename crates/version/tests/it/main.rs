use devlanes_version::{
    BUILD_ID, LATEST_RELEASE_MANIFEST_URL, MINIMAL_CONFIG_URL, REPOSITORY, VERSION,
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
    assert!(MINIMAL_CONFIG_URL.starts_with(REPOSITORY));
}

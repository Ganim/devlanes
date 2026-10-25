//! Reads the release version from the devlanes binary's manifest.
//!
//! This crate keeps its own package version, so depending on it doesn't tie every
//! library crate's version to each release; the version it reports is `devlanes`'s.

use std::env;
use std::fs;
use std::path::Path;

fn main() {
    let manifest = Path::new(&env::var("CARGO_MANIFEST_DIR").expect("set by cargo"))
        .join("../../bins/devlanes/Cargo.toml");
    println!("cargo:rerun-if-changed={}", manifest.display());
    let text = fs::read_to_string(&manifest)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest.display()));
    // The first top-level `version = "..."` is the [package] one; dependency tables
    // write their versions inline.
    let version = text
        .lines()
        .find_map(|line| {
            line.strip_prefix("version = \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
        .unwrap_or_else(|| panic!("no version in {}", manifest.display()));
    println!("cargo:rustc-env=DEVLANES_RELEASE_VERSION={version}");
}

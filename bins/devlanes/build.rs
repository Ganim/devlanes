use std::{error::Error, fs, path::Path};

/// Generate served schema artifacts from the registered integration types.
fn main() -> Result<(), Box<dyn Error>> {
    // Cargo tracks build-dependency changes. Exclude generated outputs to avoid rebuild loops.
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").ok_or("missing CARGO_MANIFEST_DIR")?;
    let config = Path::new(&manifest).join("../../crates/config");
    let project = devlanes_config::project_schema_with_presets(&[
        (
            devlanes_preset_vite::NAME,
            devlanes_preset_vite::options_schema(),
        ),
        (
            devlanes_preset_next::NAME,
            devlanes_preset_next::options_schema(),
        ),
        (
            devlanes_preset_wrangler::NAME,
            devlanes_preset_wrangler::options_schema(),
        ),
        (
            devlanes_preset_tauri::NAME,
            devlanes_preset_tauri::options_schema(),
        ),
    ]);
    // These publish=false workspace packages keep served artifacts in the source tree.
    // The JSON $comment marks them as generated; do not edit them by hand.
    write_changed(
        &config.join("schema.json"),
        &devlanes_config::schema_artifact(project)?,
    )?;
    write_changed(
        &config.join("user-schema.json"),
        &devlanes_config::schema_artifact(devlanes_config::user_schema())?,
    )?;
    embed_manifest(Path::new(&manifest));
    Ok(())
}

/// Embeds `devlanes.manifest` into the `devlanes` binary on Windows (MSVC),
/// so the logon Run key starts `daemon run` without a console window. Only
/// that binary: this package's test executables keep the default console.
fn embed_manifest(package: &Path) {
    let windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    if !(windows && msvc) {
        return;
    }
    let file = package.join("devlanes.manifest");
    println!("cargo:rerun-if-changed=devlanes.manifest");
    println!("cargo:rustc-link-arg-bin=devlanes=/MANIFEST:EMBED");
    println!(
        "cargo:rustc-link-arg-bin=devlanes=/MANIFESTINPUT:{}",
        file.display()
    );
}

/// Preserve file mtimes when generation produces identical bytes.
fn write_changed(path: &Path, content: &str) -> std::io::Result<()> {
    match fs::read(path) {
        Ok(existing) if existing == content.as_bytes() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::write(path, content)
}

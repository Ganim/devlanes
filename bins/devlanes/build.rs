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
    Ok(())
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

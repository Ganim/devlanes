//! `doctor`'s `wsl` facts: the WSL distro this CLI runs in and the settings that
//! change how devlanes behaves there. The CLI and its daemon share the distro,
//! so the CLI reads them for both doctor paths. WSL is a runtime fact of a Linux
//! build, so this module checks [`devlanes_sys::wsl`] instead of `cfg`; on macOS
//! and Windows it adds nothing.

use std::path::Path;

use serde_json::{Value, json};

/// Adds `wsl: {distro, version, interop, systemd}` when this process runs
/// inside WSL.
pub async fn annotate(data: &mut Value) {
    if !data.is_object() {
        return;
    }
    // The first WSL detection stats the drive mounts through the 9P file
    // server, so it runs off the async threads.
    let found = tokio::task::spawn_blocking(|| {
        // systemd's own test for "booted with systemd" (sd_booted(3)).
        devlanes_sys::wsl().map(|info| (info, Path::new("/run/systemd/system").is_dir()))
    })
    .await;
    let Ok(Some((info, systemd))) = found else {
        return;
    };
    data["wsl"] = json!({
        "distro": info.distro,
        "version": info.version,
        "interop": info.interop,
        "systemd": systemd,
    });
}

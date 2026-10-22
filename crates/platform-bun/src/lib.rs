//! Bun launch contributor (`platform: bun`, or detected from a bun lockfile,
//! `bunfig.toml` or `"packageManager": "bun@..."`).
//!
//! Bun reads `NODE_EXTRA_CA_CERTS` since 1.1.22 (runtime and `bun install`), so
//! trust uses the devlanes root the same way as node. Bun's `FORCE_COLOR`
//! overrides `NO_COLOR`, so color is forced only when the shell set neither.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{
        CA_CERT_ENV, ContribCx, Contributor, DETECT_MANIFEST, DETECT_REFINE, DETECT_SCRIPT_SOURCE,
        DetectCx, LaunchDraft,
    },
    env,
};

/// Platform name matched against `platform:` (explicit or inferred).
pub const PLATFORM: &str = "bun";

/// Files that mark a directory as a Bun project.
pub const MARKERS: &[&str] = &["bun.lock", "bun.lockb", "bunfig.toml"];

/// Lockfiles of other package managers. The nearest lockfile decides, so one of
/// these below a bun lockfile (a nested npm package) is not a bun project.
const OTHER_LOCKFILES: &[&str] = &[
    "package-lock.json",
    "npm-shrinkwrap.json",
    "pnpm-lock.yaml",
    "yarn.lock",
];

/// Contributor for `platform: bun` units.
#[derive(Debug, Default, Clone, Copy)]
pub struct BunContributor;

#[async_trait]
impl Contributor for BunContributor {
    fn id(&self) -> &'static str {
        "platform-bun"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn platform(&self) -> Option<&'static str> {
        Some(PLATFORM)
    }

    /// Claims only when a Bun marker applies to the working directory, a little
    /// above node's claim on the same `package.json`. Without a marker a
    /// `package.json` project stays node.
    async fn detect(&self, cx: &DetectCx<'_>) -> Option<u8> {
        let cwd = cx.cwd.to_path_buf();
        let root = cx.lane_path.to_path_buf();
        let marked = tokio::task::spawn_blocking(move || is_bun_project(&cwd, &root))
            .await
            .unwrap_or(false);
        if !marked {
            return None;
        }
        let package_script = cx
            .script
            .and_then(|s| s.source.file_name())
            .is_some_and(|name| name == "package.json");
        Some(if package_script {
            DETECT_SCRIPT_SOURCE + DETECT_REFINE
        } else {
            DETECT_MANIFEST + DETECT_REFINE
        })
    }

    /// Sets `NODE_EXTRA_CA_CERTS` from the CA and `FORCE_COLOR=1` unless the
    /// client already chose a color policy.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.platform != Some(PLATFORM) {
            return Ok(());
        }
        if let Some(cert) = cx.ca_env.get(CA_CERT_ENV) {
            // One extra file only, as in node: a shell value is replaced and reported.
            if let Some(previous) = env::get(cx.client_env, "NODE_EXTRA_CA_CERTS")
                && previous != cert
            {
                draft.notes.push(format!(
                    "NODE_EXTRA_CA_CERTS from the shell ({previous}) replaced by the devlanes CA"
                ));
            }
            draft
                .env
                .insert("NODE_EXTRA_CA_CERTS".to_owned(), cert.clone());
        }
        if !env::contains_key(cx.client_env, "FORCE_COLOR")
            && !env::contains_key(cx.client_env, "NO_COLOR")
        {
            draft.env.insert("FORCE_COLOR".to_owned(), "1".to_owned());
        }
        Ok(())
    }
}

/// Walks from `cwd` up to `root` (inclusive) and reports whether the nearest
/// package-manager decision is Bun. In each directory a `packageManager` field
/// decides first, then Bun markers, then other lockfiles. When `cwd` is outside
/// `root`, only `cwd` is checked. Blocking.
pub fn is_bun_project(cwd: &Path, root: &Path) -> bool {
    let mut dir: Option<PathBuf> = Some(cwd.to_path_buf());
    while let Some(current) = dir {
        if let Some(manager) = package_manager(&current.join("package.json")) {
            return manager.starts_with("bun@") || manager == "bun";
        }
        if MARKERS.iter().any(|m| current.join(m).exists()) {
            return true;
        }
        if OTHER_LOCKFILES.iter().any(|m| current.join(m).exists()) {
            return false;
        }
        if current == root || !current.starts_with(root) {
            break;
        }
        dir = current.parent().map(Path::to_path_buf);
    }
    false
}

/// Reads `packageManager` from a `package.json`; missing or invalid files count
/// as unset.
fn package_manager(manifest: &Path) -> Option<String> {
    let text = std::fs::read_to_string(manifest).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value.get("packageManager")?.as_str().map(str::to_owned)
}

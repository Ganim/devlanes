//! Node.js launch contributors.
//!
//! - [`NodeContributor`] (`platform: node`, or detected from `package.json`):
//!   trusts the devlanes CA through `NODE_EXTRA_CA_CERTS` and forces color, since
//!   output often goes through pipes.
//!   Node needs no unbuffering switch: `process.stdout` writes are not held back
//!   in a userspace buffer.
//! - [`CloudflareContributor`]: when the lane worktree contains a Wrangler config,
//!   points every process at a per-lane dev registry, so same-named workers in two
//!   lanes do not bind to each other.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{
        CA_CERT_ENV, ContribCx, Contributor, DETECT_MANIFEST, DETECT_SCRIPT_SOURCE, DetectCx,
        LaunchDraft,
    },
    env,
};

/// Platform name matched against `platform:` (explicit or inferred).
pub const PLATFORM: &str = "node";

/// Manifest that makes a directory a Node project.
pub const MANIFEST: &str = "package.json";

/// Returns whether the unit's `run_script` source is named `file`.
fn script_file_is(cx: &DetectCx<'_>, file: &str) -> bool {
    cx.script
        .and_then(|s| s.source.file_name())
        .is_some_and(|name| name == file)
}

/// Non-blocking existence check; unreadable paths count as absent.
async fn exists(path: &Path) -> bool {
    tokio::fs::try_exists(path).await.unwrap_or(false)
}

/// Wrangler's dev registry variable (see `devlanes-preset-wrangler`).
pub const REGISTRY_ENV: &str = "WRANGLER_REGISTRY_PATH";

/// Registry directory relative to the lane worktree, shared by the lane's workers.
pub const REGISTRY_DIR: &str = ".devlanes/wrangler-registry";

/// Wrangler config file names.
const WRANGLER_FILES: &[&str] = &["wrangler.toml", "wrangler.json", "wrangler.jsonc"];

/// Directories never searched for Wrangler configs: dependencies, VCS data and
/// build output can be large and never hold the project's own worker configs.
const SKIPPED_DIRS: &[&str] = &["node_modules", "target", "dist", "build", "out"];

/// How deep below the worktree root to look; monorepos keep workers at
/// `apps/<name>/wrangler.toml` or `workers/<group>/<name>/wrangler.jsonc`.
const SEARCH_DEPTH: usize = 4;

/// Contributor for `platform: node` units.
#[derive(Debug, Default, Clone, Copy)]
pub struct NodeContributor;

#[async_trait]
impl Contributor for NodeContributor {
    fn id(&self) -> &'static str {
        "platform-node"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn platform(&self) -> Option<&'static str> {
        Some(PLATFORM)
    }

    /// Claims `package.json` scripts and working directories with a `package.json`.
    async fn detect(&self, cx: &DetectCx<'_>) -> Option<u8> {
        if script_file_is(cx, MANIFEST) {
            return Some(DETECT_SCRIPT_SOURCE);
        }
        exists(&cx.cwd.join(MANIFEST))
            .await
            .then_some(DETECT_MANIFEST)
    }

    /// Sets `NODE_EXTRA_CA_CERTS` from the CA and `FORCE_COLOR=1` unless the
    /// client already chose a color policy.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.platform != Some(PLATFORM) {
            return Ok(());
        }
        if let Some(cert) = cx.ca_env.get(CA_CERT_ENV) {
            // Node reads one extra file only, so a user-set value is replaced and reported.
            if let Some(previous) = env::get(cx.client_env, "NODE_EXTRA_CA_CERTS")
                && previous != cert
            {
                draft.notes.push(format!(
                    "NODE_EXTRA_CA_CERTS from the shell ({previous}) replaced by the devlanes CA; add that CA to the devlanes bundle or set it in the service env"
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

/// Sets `WRANGLER_REGISTRY_PATH` for every process of lanes whose worktree
/// contains a Wrangler config.
#[derive(Debug, Default, Clone, Copy)]
pub struct CloudflareContributor;

#[async_trait]
impl Contributor for CloudflareContributor {
    fn id(&self) -> &'static str {
        "cloudflare"
    }

    fn priority(&self) -> i32 {
        60
    }

    /// Scans the worktree once per lane resolution (cached in the memo).
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        let key = format!("cloudflare:{}", cx.lane_path.display());
        let found = match cx.memo.get::<bool>(&key) {
            Some(found) => *found,
            None => {
                let root = cx.lane_path.to_path_buf();
                let found = tokio::task::spawn_blocking(move || has_wrangler_config(&root))
                    .await
                    .map_err(|e| Error::Integration(format!("wrangler config scan failed: {e}")))?;
                cx.memo.insert(key, Arc::new(found));
                found
            }
        };
        if found {
            draft.env.insert(
                REGISTRY_ENV.to_owned(),
                registry_path(cx.lane_path).display().to_string(),
            );
        }
        Ok(())
    }
}

/// Returns `<lane>/.devlanes/wrangler-registry`.
pub fn registry_path(lane: &Path) -> PathBuf {
    lane.join(REGISTRY_DIR)
}

/// Looks for `wrangler.toml|json|jsonc` up to [`SEARCH_DEPTH`] levels below
/// `root`, skipping hidden directories and [`SKIPPED_DIRS`]. Blocking.
pub fn has_wrangler_config(root: &Path) -> bool {
    let mut pending = vec![(root.to_path_buf(), 0_usize)];
    while let Some((dir, depth)) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_file() && WRANGLER_FILES.contains(&name.as_ref()) {
                return true;
            }
            if kind.is_dir()
                && depth < SEARCH_DEPTH
                && !name.starts_with('.')
                && !SKIPPED_DIRS.contains(&name.as_ref())
            {
                pending.push((entry.path(), depth + 1));
            }
        }
    }
    false
}

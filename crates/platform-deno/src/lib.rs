//! Deno launch contributor (`platform: deno`, or detected from `deno.json` or
//! `deno.jsonc`).
//!
//! `DENO_CERT` adds the certificates of one PEM file to Deno's root store
//! (Mozilla roots by default), so the devlanes root alone is enough. Deno honours
//! `FORCE_COLOR` since 2.2.4 and `NO_COLOR` always. `DENO_NO_UPDATE_CHECK` keeps
//! every service start from printing upgrade notices.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{
        CA_BUNDLE_ENV, CA_CERT_ENV, ContribCx, Contributor, DETECT_MANIFEST, DETECT_REFINE,
        DETECT_SCRIPT_SOURCE, DetectCx, LaunchDraft,
    },
    env,
};

/// Platform name matched against `platform:` (explicit or inferred).
pub const PLATFORM: &str = "deno";

/// Config files that make a directory a Deno project.
pub const CONFIG_FILES: &[&str] = &["deno.json", "deno.jsonc"];

/// Claim for a Deno config in an ancestor of the working directory (a Deno
/// workspace root). Below node's claim on a `package.json` in the working
/// directory itself, which is the closer signal.
const ANCESTOR_CONFIDENCE: u8 = DETECT_MANIFEST - 15;

/// Contributor for `platform: deno` units.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenoContributor;

#[async_trait]
impl Contributor for DenoContributor {
    fn id(&self) -> &'static str {
        "platform-deno"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn platform(&self) -> Option<&'static str> {
        Some(PLATFORM)
    }

    /// Claims `deno.json` script sources, a Deno config in the working directory
    /// (above node, since Deno also reads `package.json`), and more weakly a
    /// config in an ancestor inside the lane.
    async fn detect(&self, cx: &DetectCx<'_>) -> Option<u8> {
        let source = cx.script.and_then(|s| s.source.file_name());
        if source.is_some_and(|name| CONFIG_FILES.iter().any(|c| name == *c)) {
            return Some(DETECT_SCRIPT_SOURCE);
        }
        let cwd = cx.cwd.to_path_buf();
        let root = cx.lane_path.to_path_buf();
        let depth = tokio::task::spawn_blocking(move || config_depth(&cwd, &root))
            .await
            .ok()
            .flatten()?;
        Some(if depth == 0 {
            DETECT_MANIFEST + DETECT_REFINE
        } else {
            ANCESTOR_CONFIDENCE
        })
    }

    /// Sets `DENO_CERT` (the devlanes root, else the combined bundle),
    /// `DENO_NO_UPDATE_CHECK=1` and `FORCE_COLOR=1` unless the client already
    /// chose a color policy.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.platform != Some(PLATFORM) {
            return Ok(());
        }
        if let Some(cert) = cx
            .ca_env
            .get(CA_CERT_ENV)
            .or_else(|| cx.ca_env.get(CA_BUNDLE_ENV))
        {
            // DENO_CERT names one file, so a shell value is replaced and reported.
            if let Some(previous) = env::get(cx.client_env, "DENO_CERT")
                && previous != cert
            {
                draft.notes.push(format!(
                    "DENO_CERT from the shell ({previous}) replaced by the devlanes CA"
                ));
            }
            draft.env.insert("DENO_CERT".to_owned(), cert.clone());
        }
        draft
            .env
            .insert("DENO_NO_UPDATE_CHECK".to_owned(), "1".to_owned());
        if !env::contains_key(cx.client_env, "FORCE_COLOR")
            && !env::contains_key(cx.client_env, "NO_COLOR")
        {
            draft.env.insert("FORCE_COLOR".to_owned(), "1".to_owned());
        }
        Ok(())
    }
}

/// Returns how many levels above `cwd` the nearest Deno config is (0 for `cwd`
/// itself), searching up to `root` inclusive. When `cwd` is outside `root`, only
/// `cwd` is checked. Blocking.
pub fn config_depth(cwd: &Path, root: &Path) -> Option<usize> {
    let mut dir: Option<PathBuf> = Some(cwd.to_path_buf());
    let mut depth = 0;
    while let Some(current) = dir {
        if CONFIG_FILES.iter().any(|c| current.join(c).exists()) {
            return Some(depth);
        }
        if current == root || !current.starts_with(root) {
            break;
        }
        dir = current.parent().map(Path::to_path_buf);
        depth += 1;
    }
    None
}

//! Rust launch contributor (`platform: rust`, or detected from `Cargo.toml`).
//!
//! `SSL_CERT_FILE` points at the combined bundle (system roots plus the devlanes
//! root): OpenSSL (`openssl`, and `native-tls` on Linux) and `rustls-native-certs`
//! read that file instead of the system store, so it must contain both.
//! `native-tls` on macOS uses the keychain, which `devlanes certs trust` covers.
//! Programs built on `webpki-roots` trust only their compiled-in roots and ignore
//! every variable. `RUST_LOG` is never set: logging levels belong to the project.

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{
        CA_BUNDLE_ENV, ContribCx, Contributor, DETECT_MANIFEST, DETECT_SCRIPT_SOURCE, DetectCx,
        LaunchDraft,
    },
    env,
};

/// Platform name matched against `platform:` (explicit or inferred).
pub const PLATFORM: &str = "rust";

/// Manifest that makes a directory a Cargo package or workspace.
pub const MANIFEST: &str = "Cargo.toml";

/// Claim for a `Cargo.toml` in the working directory. Below node's, so a
/// directory with both (a napi-rs package) runs its `package.json` scripts as node.
const CWD_CONFIDENCE: u8 = DETECT_MANIFEST - 5;

/// Contributor for `platform: rust` units.
#[derive(Debug, Default, Clone, Copy)]
pub struct RustContributor;

#[async_trait]
impl Contributor for RustContributor {
    fn id(&self) -> &'static str {
        "platform-rust"
    }

    fn priority(&self) -> i32 {
        50
    }

    fn platform(&self) -> Option<&'static str> {
        Some(PLATFORM)
    }

    /// Claims `Cargo.toml` script sources and working directories with a `Cargo.toml`.
    async fn detect(&self, cx: &DetectCx<'_>) -> Option<u8> {
        let source = cx.script.and_then(|s| s.source.file_name());
        if source.is_some_and(|name| name == MANIFEST) {
            return Some(DETECT_SCRIPT_SOURCE);
        }
        tokio::fs::try_exists(cx.cwd.join(MANIFEST))
            .await
            .unwrap_or(false)
            .then_some(CWD_CONFIDENCE)
    }

    /// Sets `CARGO_TERM_COLOR=always` unless the client chose a color policy, and
    /// `SSL_CERT_FILE` to the combined bundle when the CA provides one.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.platform != Some(PLATFORM) {
            return Ok(());
        }
        if !env::contains_key(cx.client_env, "CARGO_TERM_COLOR")
            && !env::contains_key(cx.client_env, "NO_COLOR")
        {
            draft
                .env
                .insert("CARGO_TERM_COLOR".to_owned(), "always".to_owned());
        }
        if let Some(bundle) = cx.ca_env.get(CA_BUNDLE_ENV) {
            // SSL_CERT_FILE replaces the trust store, so a shell value is dropped; say so.
            if let Some(previous) = env::get(cx.client_env, "SSL_CERT_FILE")
                && previous != bundle
            {
                draft.notes.push(format!(
                    "SSL_CERT_FILE from the shell ({previous}) replaced by the devlanes bundle"
                ));
            }
            draft.env.insert("SSL_CERT_FILE".to_owned(), bundle.clone());
        }
        Ok(())
    }
}

//! Python launch contributor (`platform: python`).
//!
//! Python block-buffers stdout when it is a pipe, so logs of `pty: false`
//! services would arrive late without `PYTHONUNBUFFERED`. `ssl`, `requests` and
//! `httpx` replace their root store with the file they are given, so both
//! variables point at the combined bundle (system roots plus the devlanes root).

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{CA_BUNDLE_ENV, ContribCx, Contributor, LaunchDraft},
};

/// Platform name matched against `platform:` (explicit or inferred).
pub const PLATFORM: &str = "python";

/// Contributor for `platform: python` units.
#[derive(Debug, Default, Clone, Copy)]
pub struct PythonContributor;

#[async_trait]
impl Contributor for PythonContributor {
    fn id(&self) -> &'static str {
        "platform-python"
    }

    fn priority(&self) -> i32 {
        50
    }

    /// Sets `PYTHONUNBUFFERED=1` and, when the CA provides a bundle,
    /// `SSL_CERT_FILE` and `REQUESTS_CA_BUNDLE`.
    async fn apply(&self, draft: &mut LaunchDraft, cx: &ContribCx<'_>) -> Result<(), Error> {
        if cx.platform != Some(PLATFORM) {
            return Ok(());
        }
        draft
            .env
            .insert("PYTHONUNBUFFERED".to_owned(), "1".to_owned());
        if let Some(bundle) = cx.ca_env.get(CA_BUNDLE_ENV) {
            for key in ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE"] {
                draft.env.insert(key.to_owned(), bundle.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_core::contrib::{ContribMemo, DraftCommand, UnitKind};
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::{Path, PathBuf},
    };

    /// Applies the contributor to an empty draft for `platform`.
    async fn run(platform: &str, ca: &BTreeMap<String, String>) -> BTreeMap<String, String> {
        let memo = ContribMemo::default();
        let empty = BTreeMap::new();
        let ports = BTreeMap::new();
        let cx = ContribCx {
            kind: UnitKind::Service,
            project: "acme",
            name: "ml",
            replica: Some(1),
            lane_index: 0,
            lane_name: "main",
            lane_path: Path::new("/w"),
            primary_path: Path::new("/w"),
            platform: Some(platform),
            preset: None,
            preset_options: None,
            port: Some(21_000),
            ports: &ports,
            public_url: None,
            pty: false,
            script: None,
            client_env: &empty,
            ca_env: ca,
            memo: &memo,
        };
        let mut draft = LaunchDraft {
            command: DraftCommand::Shell("uv run app".into()),
            args: Vec::new(),
            cwd: PathBuf::from("/w"),
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        PythonContributor
            .apply(&mut draft, &cx)
            .await
            .expect("apply");
        draft.env
    }

    #[tokio::test]
    /// Python units get unbuffered output and the combined bundle.
    async fn python_env() {
        let ca = BTreeMap::from([(CA_BUNDLE_ENV.to_owned(), "/ca/bundle.pem".to_owned())]);
        let env = run("python", &ca).await;
        assert_eq!(env["PYTHONUNBUFFERED"], "1");
        assert_eq!(env["SSL_CERT_FILE"], "/ca/bundle.pem");
        assert_eq!(env["REQUESTS_CA_BUNDLE"], "/ca/bundle.pem");
        let without_ca = run("python", &BTreeMap::new()).await;
        assert!(!without_ca.contains_key("SSL_CERT_FILE"));
        assert!(run("node", &ca).await.is_empty());
    }
}

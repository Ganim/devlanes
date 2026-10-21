//! Rust detection on tempdir projects and env on fake launch drafts.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use devlanes_core::contrib::{
    CA_BUNDLE_ENV, ContribCx, ContribMemo, Contributor, DETECT_MANIFEST, DETECT_SCRIPT_SOURCE,
    DetectCx, DraftCommand, LaunchDraft, ScriptInfo, UnitKind,
};
use devlanes_platform_rust::RustContributor;

/// Detection context for a service running in `dir`.
fn detect_cx<'a>(dir: &'a Path, script: Option<&'a ScriptInfo>) -> DetectCx<'a> {
    DetectCx {
        kind: UnitKind::Service,
        name: "api",
        lane_path: dir,
        cwd: dir,
        script,
    }
}

#[tokio::test]
/// `Cargo.toml` claims below node in the cwd and strongly as a script source.
async fn detection() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        RustContributor.detect(&detect_cx(dir.path(), None)).await,
        None
    );
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("write");
    let cwd = RustContributor
        .detect(&detect_cx(dir.path(), None))
        .await
        .expect("claim");
    assert!(cwd < DETECT_MANIFEST);
    let script = ScriptInfo {
        source: dir.path().join("Cargo.toml"),
        name: "server".into(),
        body: None,
        cwd: dir.path().to_path_buf(),
        runner: "cargo",
    };
    assert_eq!(
        RustContributor
            .detect(&detect_cx(dir.path(), Some(&script)))
            .await,
        Some(DETECT_SCRIPT_SOURCE)
    );
}

/// Applies the contributor for a unit with `platform`.
async fn apply(
    platform: &str,
    client: &BTreeMap<String, String>,
    ca: &BTreeMap<String, String>,
) -> LaunchDraft {
    let lane = PathBuf::from("/w");
    let memo = ContribMemo::default();
    let ports = BTreeMap::new();
    let cx = ContribCx {
        kind: UnitKind::Service,
        project: "acme",
        name: "api",
        replica: Some(1),
        lane_index: 1,
        lane_name: "lane1",
        lane_path: &lane,
        primary_path: &lane,
        platform: Some(platform),
        preset: None,
        preset_options: None,
        port: Some(21_000),
        ports: &ports,
        public_url: None,
        pty: false,
        script: None,
        client_env: client,
        ca_env: ca,
        memo: &memo,
    };
    let mut draft = LaunchDraft {
        command: DraftCommand::Argv(vec!["cargo".into(), "run".into()]),
        args: Vec::new(),
        cwd: lane.clone(),
        env: BTreeMap::new(),
        remove: BTreeSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
    };
    RustContributor.apply(&mut draft, &cx).await.expect("apply");
    draft
}

#[tokio::test]
/// Color and the combined bundle; RUST_LOG is never touched.
async fn env() {
    let ca = BTreeMap::from([(CA_BUNDLE_ENV.to_owned(), "/ca/bundle.pem".to_owned())]);
    let draft = apply("rust", &BTreeMap::new(), &ca).await;
    assert_eq!(draft.env["CARGO_TERM_COLOR"], "always");
    assert_eq!(draft.env["SSL_CERT_FILE"], "/ca/bundle.pem");
    assert!(!draft.env.contains_key("RUST_LOG"));

    let shell = BTreeMap::from([
        ("CARGO_TERM_COLOR".to_owned(), "never".to_owned()),
        ("SSL_CERT_FILE".to_owned(), "/corp.pem".to_owned()),
        ("RUST_LOG".to_owned(), "debug".to_owned()),
    ]);
    let draft = apply("rust", &shell, &ca).await;
    assert!(!draft.env.contains_key("CARGO_TERM_COLOR"));
    assert!(!draft.env.contains_key("RUST_LOG"));
    assert_eq!(draft.notes.len(), 1);
    assert!(apply("python", &BTreeMap::new(), &ca).await.env.is_empty());
}

//! Deno detection on tempdir projects and env on fake launch drafts.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use devlanes_core::contrib::{
    CA_BUNDLE_ENV, CA_CERT_ENV, ContribCx, ContribMemo, Contributor, DETECT_MANIFEST,
    DETECT_REFINE, DETECT_SCRIPT_SOURCE, DetectCx, DraftCommand, LaunchDraft, ScriptInfo, UnitKind,
};
use devlanes_platform_deno::DenoContributor;

/// Detection context for a service running in `cwd` of the lane at `lane`.
fn detect_cx<'a>(lane: &'a Path, cwd: &'a Path, script: Option<&'a ScriptInfo>) -> DetectCx<'a> {
    DetectCx {
        kind: UnitKind::Service,
        name: "api",
        lane_path: lane,
        cwd,
        script,
    }
}

#[tokio::test]
/// Script sources and configs in the cwd claim above node; ancestors below it.
async fn detection() {
    let lane = tempfile::tempdir().expect("tempdir");
    let root = lane.path();
    let app = root.join("apps/api");
    std::fs::create_dir_all(&app).expect("mkdir");
    assert_eq!(
        DenoContributor.detect(&detect_cx(root, &app, None)).await,
        None
    );

    std::fs::write(root.join("deno.json"), "{}").expect("write");
    let ancestor = DenoContributor
        .detect(&detect_cx(root, &app, None))
        .await
        .expect("ancestor claim");
    assert!(ancestor < DETECT_MANIFEST);

    std::fs::write(app.join("deno.jsonc"), "{}").expect("write");
    assert_eq!(
        DenoContributor.detect(&detect_cx(root, &app, None)).await,
        Some(DETECT_MANIFEST + DETECT_REFINE)
    );
    let script = ScriptInfo {
        source: app.join("deno.jsonc"),
        name: "dev".into(),
        body: None,
        cwd: app.clone(),
        runner: "deno",
    };
    assert_eq!(
        DenoContributor
            .detect(&detect_cx(root, &app, Some(&script)))
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
        lane_data_path: std::path::Path::new("/data/demo/1"),
        primary_path: &lane,
        platform: Some(platform),
        preset: None,
        preset_options: None,
        port: Some(21_000),
        ports: &ports,
        public_url: None,
        pty: true,
        script: None,
        client_env: client,
        ca_env: ca,
        memo: &memo,
    };
    let mut draft = LaunchDraft {
        command: DraftCommand::Shell("deno task dev".into()),
        args: Vec::new(),
        cwd: lane.clone(),
        env: BTreeMap::new(),
        remove: BTreeSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
    };
    DenoContributor.apply(&mut draft, &cx).await.expect("apply");
    draft
}

#[tokio::test]
/// Deno units get DENO_CERT, no update checks and color; a shell DENO_CERT is reported.
async fn env() {
    let ca = BTreeMap::from([
        (CA_CERT_ENV.to_owned(), "/ca/root.pem".to_owned()),
        (CA_BUNDLE_ENV.to_owned(), "/ca/bundle.pem".to_owned()),
    ]);
    let draft = apply("deno", &BTreeMap::new(), &ca).await;
    assert_eq!(draft.env["DENO_CERT"], "/ca/root.pem");
    assert_eq!(draft.env["DENO_NO_UPDATE_CHECK"], "1");
    assert_eq!(draft.env["FORCE_COLOR"], "1");
    assert!(draft.notes.is_empty());

    let bundle_only = BTreeMap::from([(CA_BUNDLE_ENV.to_owned(), "/ca/bundle.pem".to_owned())]);
    let shell = BTreeMap::from([
        ("DENO_CERT".to_owned(), "/corp.pem".to_owned()),
        ("NO_COLOR".to_owned(), "1".to_owned()),
    ]);
    let draft = apply("deno", &shell, &bundle_only).await;
    assert_eq!(draft.env["DENO_CERT"], "/ca/bundle.pem");
    assert_eq!(draft.notes.len(), 1);
    assert!(!draft.env.contains_key("FORCE_COLOR"));
    assert!(apply("node", &BTreeMap::new(), &ca).await.env.is_empty());
}

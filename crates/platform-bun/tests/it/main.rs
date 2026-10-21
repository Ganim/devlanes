//! Bun detection on tempdir projects and env on fake launch drafts.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use devlanes_core::contrib::{
    CA_CERT_ENV, ContribCx, ContribMemo, Contributor, DETECT_MANIFEST, DETECT_REFINE,
    DETECT_SCRIPT_SOURCE, DetectCx, DraftCommand, LaunchDraft, ScriptInfo, UnitKind,
};
use devlanes_platform_bun::{BunContributor, is_bun_project};

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

/// Writes `contents` to `dir/name`, creating `dir`.
fn write(dir: &Path, name: &str, contents: &str) {
    std::fs::create_dir_all(dir).expect("mkdir");
    std::fs::write(dir.join(name), contents).expect("write");
}

#[tokio::test]
/// Markers, the packageManager pin and the nearest lockfile decide.
async fn detection() {
    let lane = tempfile::tempdir().expect("tempdir");
    let root = lane.path();
    let app = root.join("apps/api");
    write(&app, "package.json", "{}");
    assert_eq!(
        BunContributor.detect(&detect_cx(root, &app, None)).await,
        None
    );

    write(root, "bun.lock", "");
    assert_eq!(
        BunContributor.detect(&detect_cx(root, &app, None)).await,
        Some(DETECT_MANIFEST + DETECT_REFINE)
    );
    let script = ScriptInfo {
        source: app.join("package.json"),
        name: "dev".into(),
        body: None,
        cwd: app.clone(),
        runner: "npm",
    };
    assert_eq!(
        BunContributor
            .detect(&detect_cx(root, &app, Some(&script)))
            .await,
        Some(DETECT_SCRIPT_SOURCE + DETECT_REFINE)
    );

    // A nearer npm lockfile wins over the root bun lockfile.
    write(&app, "package-lock.json", "{}");
    assert!(!is_bun_project(&app, root));
    // An explicit pin beats any lockfile in the same directory.
    write(&app, "package.json", r#"{"packageManager":"bun@1.3.2"}"#);
    assert!(is_bun_project(&app, root));
    write(&app, "package.json", r#"{"packageManager":"pnpm@10.0.0"}"#);
    std::fs::remove_file(app.join("package-lock.json")).expect("remove");
    assert!(!is_bun_project(&app, root), "pin to another manager");
}

#[test]
/// The walk stops at the lane root and never reads above it.
fn stays_inside_the_lane() {
    let outer = tempfile::tempdir().expect("tempdir");
    write(outer.path(), "bun.lockb", "");
    let lane = outer.path().join("lane");
    write(&lane, "package.json", "{}");
    assert!(!is_bun_project(&lane, &lane));
    let elsewhere = tempfile::tempdir().expect("tempdir");
    write(elsewhere.path(), "bunfig.toml", "");
    assert!(
        is_bun_project(elsewhere.path(), &lane),
        "cwd itself is checked"
    );
}

/// Owned data behind a [`ContribCx`].
struct Fixture {
    lane: PathBuf,
    client: BTreeMap<String, String>,
    ca: BTreeMap<String, String>,
    ports: BTreeMap<String, u16>,
    memo: ContribMemo,
}

impl Fixture {
    /// Fixture with a devlanes root certificate.
    fn new() -> Self {
        Self {
            lane: PathBuf::from("/w"),
            client: BTreeMap::new(),
            ca: BTreeMap::from([(CA_CERT_ENV.to_owned(), "/ca/root.pem".to_owned())]),
            ports: BTreeMap::new(),
            memo: ContribMemo::default(),
        }
    }

    /// Applies the contributor for a unit with `platform`.
    async fn apply(&self, platform: &str) -> LaunchDraft {
        let cx = ContribCx {
            kind: UnitKind::Service,
            project: "acme",
            name: "api",
            replica: Some(1),
            lane_index: 1,
            lane_name: "lane1",
            lane_path: &self.lane,
            primary_path: &self.lane,
            platform: Some(platform),
            preset: None,
            preset_options: None,
            port: Some(21_000),
            ports: &self.ports,
            public_url: None,
            pty: true,
            script: None,
            client_env: &self.client,
            ca_env: &self.ca,
            memo: &self.memo,
        };
        let mut draft = LaunchDraft {
            command: DraftCommand::Shell("bun run dev".into()),
            args: Vec::new(),
            cwd: self.lane.clone(),
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        BunContributor.apply(&mut draft, &cx).await.expect("apply");
        draft
    }
}

#[tokio::test]
/// Bun units get the CA and color unless the shell opted out.
async fn env() {
    let mut fixture = Fixture::new();
    let draft = fixture.apply("bun").await;
    assert_eq!(draft.env["NODE_EXTRA_CA_CERTS"], "/ca/root.pem");
    assert_eq!(draft.env["FORCE_COLOR"], "1");
    assert!(fixture.apply("node").await.env.is_empty());
    fixture.client.insert("NO_COLOR".into(), "1".into());
    assert!(!fixture.apply("bun").await.env.contains_key("FORCE_COLOR"));
}

//! Node and Cloudflare contributor behavior on fake launch drafts.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

use devlanes_core::contrib::{
    CA_CERT_ENV, ContribCx, ContribMemo, Contributor, DETECT_MANIFEST, DETECT_SCRIPT_SOURCE,
    DetectCx, DraftCommand, LaunchDraft, ScriptInfo, UnitKind,
};
use devlanes_platform_node::{CloudflareContributor, NodeContributor, REGISTRY_ENV};

/// Owned data behind a [`ContribCx`].
struct Fixture {
    lane: PathBuf,
    data: PathBuf,
    client: BTreeMap<String, String>,
    ca: BTreeMap<String, String>,
    ports: BTreeMap<String, u16>,
    memo: ContribMemo,
}

impl Fixture {
    /// Fixture for a lane at `lane` with a devlanes CA.
    fn new(lane: PathBuf) -> Self {
        Self {
            lane,
            data: PathBuf::from("/data/acme/3"),
            client: BTreeMap::new(),
            ca: BTreeMap::from([(CA_CERT_ENV.to_owned(), "/ca/root.pem".to_owned())]),
            ports: BTreeMap::new(),
            memo: ContribMemo::default(),
        }
    }

    /// Context for a service with `platform`.
    fn cx(&self, platform: Option<&'static str>) -> ContribCx<'_> {
        ContribCx {
            kind: UnitKind::Service,
            project: "acme",
            name: "web",
            replica: Some(1),
            lane_index: 1,
            lane_name: "lane1",
            lane_path: &self.lane,
            lane_data_path: &self.data,
            primary_path: &self.lane,
            platform,
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
        }
    }
}

/// An empty draft.
fn draft() -> LaunchDraft {
    LaunchDraft {
        command: DraftCommand::Shell("node server.js".into()),
        args: Vec::new(),
        cwd: PathBuf::from("/w"),
        env: BTreeMap::new(),
        remove: BTreeSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
    }
}

#[tokio::test]
/// `package.json` claims: strongly as a script source, at manifest level in the cwd.
async fn node_detection() {
    /// Detection context for a task running in `dir`.
    fn cx<'a>(dir: &'a Path, script: Option<&'a ScriptInfo>) -> DetectCx<'a> {
        DetectCx {
            kind: UnitKind::Task,
            name: "build",
            lane_path: dir,
            cwd: dir,
            script,
        }
    }
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(NodeContributor.detect(&cx(dir.path(), None)).await, None);
    std::fs::write(dir.path().join("package.json"), "{}").expect("write");
    assert_eq!(
        NodeContributor.detect(&cx(dir.path(), None)).await,
        Some(DETECT_MANIFEST)
    );
    let script = ScriptInfo {
        source: dir.path().join("web/package.json"),
        name: "dev".into(),
        body: None,
        cwd: dir.path().join("web"),
        runner: "npm",
    };
    assert_eq!(
        NodeContributor.detect(&cx(dir.path(), Some(&script))).await,
        Some(DETECT_SCRIPT_SOURCE)
    );
    assert_eq!(NodeContributor.platform(), Some("node"));
    assert_eq!(CloudflareContributor.platform(), None);
}

#[tokio::test]
/// Node units get the CA and color; other platforms are untouched.
async fn node_env() {
    let fixture = Fixture::new("/w".into());
    let mut node = draft();
    NodeContributor
        .apply(&mut node, &fixture.cx(Some("node")))
        .await
        .expect("apply");
    assert_eq!(node.env["NODE_EXTRA_CA_CERTS"], "/ca/root.pem");
    assert_eq!(node.env["FORCE_COLOR"], "1");
    assert!(node.notes.is_empty());

    let mut python = draft();
    NodeContributor
        .apply(&mut python, &fixture.cx(Some("python")))
        .await
        .expect("apply");
    assert!(python.env.is_empty());
}

#[tokio::test]
/// A shell color choice wins; a shell CA is replaced with a note.
async fn node_respects_client_choices() {
    let mut fixture = Fixture::new("/w".into());
    fixture.client.insert("NO_COLOR".into(), "1".into());
    fixture
        .client
        .insert("NODE_EXTRA_CA_CERTS".into(), "/other.pem".into());
    let mut node = draft();
    NodeContributor
        .apply(&mut node, &fixture.cx(Some("node")))
        .await
        .expect("apply");
    assert!(!node.env.contains_key("FORCE_COLOR"));
    assert_eq!(node.env["NODE_EXTRA_CA_CERTS"], "/ca/root.pem");
    assert_eq!(node.notes.len(), 1);
}

#[tokio::test]
/// The registry variable appears only when the worktree holds a Wrangler config.
async fn cloudflare_registry() {
    let dir = tempfile::tempdir().expect("tempdir");
    let fixture = Fixture::new(dir.path().to_path_buf());
    let mut none = draft();
    CloudflareContributor
        .apply(&mut none, &fixture.cx(None))
        .await
        .expect("apply");
    assert!(none.env.is_empty());

    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(dir.path().join("apps/gateway")).expect("mkdir");
    std::fs::create_dir_all(dir.path().join("node_modules/x")).expect("mkdir");
    std::fs::write(dir.path().join("node_modules/x/wrangler.toml"), "").expect("write");
    let fixture = Fixture::new(dir.path().to_path_buf());
    let mut only_dependency = draft();
    CloudflareContributor
        .apply(&mut only_dependency, &fixture.cx(Some("python")))
        .await
        .expect("apply");
    assert!(only_dependency.env.is_empty(), "node_modules is skipped");

    std::fs::write(dir.path().join("apps/gateway/wrangler.jsonc"), "{}").expect("write");
    let fixture = Fixture::new(dir.path().to_path_buf());
    let mut found = draft();
    CloudflareContributor
        .apply(&mut found, &fixture.cx(Some("python")))
        .await
        .expect("apply");
    assert_eq!(found.env[REGISTRY_ENV], "/data/acme/3/wrangler-registry");
    // A second lane on the same folder gets its own registry.
    let mut other = Fixture::new(dir.path().to_path_buf());
    other.data = PathBuf::from("/data/acme/4");
    let mut second = draft();
    CloudflareContributor
        .apply(&mut second, &other.cx(None))
        .await
        .expect("apply");
    assert_eq!(second.env[REGISTRY_ENV], "/data/acme/4/wrangler-registry");
    // Cached per lane: removing the file does not change this resolution.
    std::fs::remove_file(dir.path().join("apps/gateway/wrangler.jsonc")).expect("remove");
    let mut cached = draft();
    CloudflareContributor
        .apply(&mut cached, &fixture.cx(None))
        .await
        .expect("apply");
    assert!(cached.env.contains_key(REGISTRY_ENV));
}

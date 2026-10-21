//! `MiseContributor` against fake `mise` scripts, plus an opt-in real-binary test.

use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use devlanes_core::contrib::{
    ContribCx, ContribMemo, Contributor, DraftCommand, LaunchDraft, UnitKind,
};
use devlanes_env_mise::MiseContributor;

/// Writes an executable shell script and returns its path.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// Applies `contributor` once for a service in `lane` and returns the draft.
async fn apply(
    contributor: &MiseContributor,
    lane: &Path,
    memo: &ContribMemo,
) -> Result<LaunchDraft, devlanes_core::Error> {
    let client = BTreeMap::from([("PATH".to_owned(), "/usr/bin:/bin".to_owned())]);
    let empty = BTreeMap::new();
    let cx = ContribCx {
        kind: UnitKind::Service,
        project: "acme",
        name: "api",
        replica: Some(1),
        lane_index: 1,
        lane_name: "lane1",
        lane_path: lane,
        primary_path: lane,
        platform: None,
        preset: None,
        preset_options: None,
        port: Some(21_000),
        ports: &BTreeMap::new(),
        public_url: None,
        pty: true,
        script: None,
        client_env: &client,
        ca_env: &empty,
        memo,
    };
    let mut draft = LaunchDraft {
        command: DraftCommand::Shell("server".into()),
        args: Vec::new(),
        cwd: lane.to_path_buf(),
        env: BTreeMap::new(),
        remove: BTreeSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
    };
    contributor.apply(&mut draft, &cx).await?;
    Ok(draft)
}

#[tokio::test]
/// mise runs in the lane with the client env, once per resolution.
async fn runs_mise_env_once_per_lane() {
    let bin = tempfile::tempdir().expect("tempdir");
    let lane = tempfile::tempdir().expect("tempdir");
    std::fs::write(lane.path().join("mise.toml"), "[env]\nA = '1'\n").expect("write");
    let calls = bin.path().join("calls");
    let fake = script(
        bin.path(),
        "mise",
        &format!(
            r#"[ "$1 $2" = "env --json" ] || exit 2
echo run >> '{}'
printf '{{"PATH":"/tools/bin:%s","FROM_DIR":"%s"}}' "$PATH" "$(pwd -P)""#,
            calls.display()
        ),
    );
    let contributor = MiseContributor::new(fake, Duration::from_secs(5));
    let memo = ContribMemo::default();
    let first = apply(&contributor, lane.path(), &memo)
        .await
        .expect("apply");
    assert_eq!(first.env["PATH"], "/tools/bin:/usr/bin:/bin");
    let lane_dir = lane.path().canonicalize().expect("canonical");
    assert_eq!(first.env["FROM_DIR"], lane_dir.display().to_string());
    apply(&contributor, lane.path(), &memo)
        .await
        .expect("apply");
    let runs = std::fs::read_to_string(&calls).expect("calls");
    assert_eq!(runs.lines().count(), 1, "memo caches per lane");
}

#[tokio::test]
/// No mise config means no subprocess; a missing binary or untrusted config is a note.
async fn skips_and_notes() {
    let lane = tempfile::tempdir().expect("tempdir");
    let missing = MiseContributor::new("/nonexistent/mise", Duration::from_secs(1));
    let draft = apply(&missing, lane.path(), &ContribMemo::default())
        .await
        .expect("apply");
    assert!(draft.env.is_empty() && draft.notes.is_empty());

    std::fs::write(lane.path().join(".tool-versions"), "node 24\n").expect("write");
    let draft = apply(&missing, lane.path(), &ContribMemo::default())
        .await
        .expect("apply");
    assert_eq!(draft.notes.len(), 1);

    let bin = tempfile::tempdir().expect("tempdir");
    let untrusted = script(
        bin.path(),
        "mise",
        "echo 'mise ERROR Config files in ./mise.toml are not trusted.' >&2; exit 1",
    );
    let contributor = MiseContributor::new(untrusted, Duration::from_secs(5));
    let draft = apply(&contributor, lane.path(), &ContribMemo::default())
        .await
        .expect("apply");
    assert!(draft.notes[0].contains("mise trust"));
}

#[tokio::test]
/// Other failures and hangs fail the unit with a bounded error.
async fn failures_and_timeouts() {
    let lane = tempfile::tempdir().expect("tempdir");
    std::fs::write(lane.path().join("mise.toml"), "").expect("write");
    let bin = tempfile::tempdir().expect("tempdir");
    let broken = script(
        bin.path(),
        "broken",
        "echo 'mise ERROR parse error' >&2; exit 1",
    );
    let error = apply(
        &MiseContributor::new(broken, Duration::from_secs(5)),
        lane.path(),
        &ContribMemo::default(),
    )
    .await
    .err()
    .expect("fails");
    assert!(error.to_string().contains("parse error"));

    let slow = script(bin.path(), "slow", "exec sleep 5");
    let started = std::time::Instant::now();
    let error = apply(
        &MiseContributor::new(slow, Duration::from_millis(200)),
        lane.path(),
        &ContribMemo::default(),
    )
    .await
    .err()
    .expect("times out");
    assert!(error.to_string().contains("timed out"));
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
#[ignore = "needs mise"]
/// Real mise: `[env]` values from mise.toml reach the draft.
async fn real_mise() {
    let lane = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        lane.path().join("mise.toml"),
        "[env]\nDEVLANES_MISE_TEST = 'yes'\n",
    )
    .expect("write");
    let mut client: BTreeMap<String, String> = std::env::vars().collect();
    client.insert(
        "MISE_TRUSTED_CONFIG_PATHS".into(),
        lane.path().display().to_string(),
    );
    let contributor = MiseContributor::default();
    let memo = ContribMemo::default();
    let empty = BTreeMap::new();
    let cx = ContribCx {
        kind: UnitKind::Task,
        project: "acme",
        name: "t",
        replica: None,
        lane_index: 0,
        lane_name: "main",
        lane_path: lane.path(),
        primary_path: lane.path(),
        platform: None,
        preset: None,
        preset_options: None,
        port: None,
        ports: &BTreeMap::new(),
        public_url: None,
        pty: false,
        script: None,
        client_env: &client,
        ca_env: &empty,
        memo: &memo,
    };
    let mut draft = LaunchDraft {
        command: DraftCommand::Shell("true".into()),
        args: Vec::new(),
        cwd: lane.path().to_path_buf(),
        env: BTreeMap::new(),
        remove: BTreeSet::new(),
        files: Vec::new(),
        notes: Vec::new(),
    };
    contributor.apply(&mut draft, &cx).await.expect("mise env");
    assert_eq!(draft.env["DEVLANES_MISE_TEST"], "yes");
}

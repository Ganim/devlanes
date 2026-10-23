//! `DirenvContributor` against fake `direnv` programs, plus an opt-in real-binary test.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    time::Duration,
};

use devlanes_core::contrib::{
    ContribCx, ContribMemo, Contributor, DraftCommand, LaunchDraft, UnitKind,
};
use devlanes_env_direnv::DirenvContributor;
use devlanes_testing::fake_tool::Behaviour;

/// Writes an executable shell script running `body` and returns its path.
#[cfg(unix)]
fn fake(dir: &Path, name: &str, body: &str, _windows: Behaviour) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// Installs `fake-tool` doing what the Unix script `_unix` does, since Windows
/// cannot run scripts as programs.
#[cfg(windows)]
fn fake(dir: &Path, name: &str, _unix: &str, behaviour: Behaviour) -> PathBuf {
    devlanes_testing::fake_tool::install(dir, name, &behaviour).expect("install fake-tool")
}

/// Applies `contributor` once for a task in `lane` with `client` as the client env.
async fn apply(
    contributor: &DirenvContributor,
    lane: &Path,
    client: &BTreeMap<String, String>,
) -> Result<LaunchDraft, devlanes_core::Error> {
    let memo = ContribMemo::default();
    let empty = BTreeMap::new();
    let cx = ContribCx {
        kind: UnitKind::Task,
        project: "acme",
        name: "migrate",
        replica: None,
        lane_index: 1,
        lane_name: "lane1",
        lane_path: lane,
        lane_data_path: std::path::Path::new("/data/demo/1"),
        primary_path: lane,
        platform: None,
        preset: None,
        preset_options: None,
        port: None,
        ports: &BTreeMap::new(),
        public_url: None,
        pty: false,
        script: None,
        client_env: client,
        ca_env: &empty,
        memo: &memo,
    };
    let mut draft = LaunchDraft {
        command: DraftCommand::Shell("migrate".into()),
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
/// Sets and unsets reach the draft; the client env is what direnv diffs against.
async fn applies_export_diff() {
    let lane = tempfile::tempdir().expect("tempdir");
    std::fs::write(lane.path().join(".envrc"), "export A=1\n").expect("write");
    let bin = tempfile::tempdir().expect("tempdir");
    let fake = fake(
        bin.path(),
        "direnv",
        r#"[ "$1 $2" = "export json" ] || exit 2
echo 'direnv: loading .envrc' >&2
printf '{"A":"1","FROM_CLIENT":"%s","GONE":null,"DIRENV_DIR":"-x"}' "$CLIENT_MARK""#,
        Behaviour::new()
            .args(["export", "json"])
            .stderr("direnv: loading .envrc\n")
            .stdout_json(serde_json::json!({
                "A": "1",
                "FROM_CLIENT": "{env:CLIENT_MARK}",
                "GONE": null,
                "DIRENV_DIR": "-x",
            })),
    );
    let client = BTreeMap::from([
        ("CLIENT_MARK".to_owned(), "seen".to_owned()),
        ("GONE".to_owned(), "old".to_owned()),
    ]);
    let draft = apply(
        &DirenvContributor::new(fake, Duration::from_secs(5)),
        lane.path(),
        &client,
    )
    .await
    .expect("apply");
    assert_eq!(draft.env["A"], "1");
    assert_eq!(draft.env["FROM_CLIENT"], "seen");
    assert!(!draft.env.contains_key("DIRENV_DIR"));
    assert!(draft.remove.contains("GONE"));
}

#[tokio::test]
/// No `.envrc` means no subprocess; a blocked one is a note, other failures errors.
async fn blocked_missing_and_failures() {
    let lane = tempfile::tempdir().expect("tempdir");
    let client = BTreeMap::new();
    let missing = DirenvContributor::new("/nonexistent/direnv", Duration::from_secs(1));
    let draft = apply(&missing, lane.path(), &client).await.expect("apply");
    assert!(draft.env.is_empty() && draft.notes.is_empty());

    std::fs::write(lane.path().join(".envrc"), "export A=1\n").expect("write");
    let bin = tempfile::tempdir().expect("tempdir");
    let blocked = fake(
        bin.path(),
        "blocked",
        "echo 'direnv: error /w/.envrc is blocked. Run `direnv allow` to approve its content' >&2; exit 1",
        Behaviour::new()
            .stderr(
                "direnv: error /w/.envrc is blocked. Run `direnv allow` to approve its content\n",
            )
            .exit(1),
    );
    let draft = apply(
        &DirenvContributor::new(blocked, Duration::from_secs(5)),
        lane.path(),
        &client,
    )
    .await
    .expect("apply");
    assert!(draft.notes[0].contains("direnv allow"));
    assert!(draft.env.is_empty());

    let broken = fake(
        bin.path(),
        "broken",
        "echo 'direnv: loading .envrc' >&2; echo 'direnv: error exit status 1' >&2; exit 1",
        Behaviour::new()
            .stderr("direnv: loading .envrc\ndirenv: error exit status 1\n")
            .exit(1),
    );
    let error = apply(
        &DirenvContributor::new(broken, Duration::from_secs(5)),
        lane.path(),
        &client,
    )
    .await
    .err()
    .expect("fails");
    assert!(error.to_string().contains("direnv: error exit status 1"));

    let slow = fake(
        bin.path(),
        "slow",
        "exec /bin/sleep 5",
        Behaviour::new().sleep(Duration::from_secs(5)),
    );
    let error = apply(
        &DirenvContributor::new(slow, Duration::from_millis(200)),
        lane.path(),
        &client,
    )
    .await
    .err()
    .expect("times out");
    assert!(error.to_string().contains("timed out"));
}

#[tokio::test]
#[ignore = "needs direnv"]
/// Real direnv: an allowed `.envrc` exports its variables.
async fn real_direnv() {
    let lane = tempfile::tempdir().expect("tempdir");
    let data = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        lane.path().join(".envrc"),
        "export DEVLANES_DIRENV_TEST=yes\n",
    )
    .expect("write");
    // The allow list lives under XDG_DATA_HOME; keep it test-owned.
    let mut client: BTreeMap<String, String> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("DIRENV_"))
        .collect();
    client.insert("XDG_DATA_HOME".into(), data.path().display().to_string());
    let allowed = std::process::Command::new("direnv")
        .arg("allow")
        .current_dir(lane.path())
        .env_clear()
        .envs(&client)
        .status()
        .expect("direnv allow");
    assert!(allowed.success());
    let draft = apply(&DirenvContributor::default(), lane.path(), &client)
        .await
        .expect("apply");
    assert_eq!(draft.env["DEVLANES_DIRENV_TEST"], "yes");
}

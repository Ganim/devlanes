//! `FnoxProvider` against fake `fnox` scripts, plus an opt-in real-binary test.

use std::{
    collections::BTreeMap,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use devlanes_core::contrib::{SecretsError, SecretsProvider, SecretsRequest};
use devlanes_secrets_fnox::FnoxProvider;

/// Writes an executable shell script and returns its path.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// A request for `worktree`.
fn request<'a>(
    worktree: &'a Path,
    profile: Option<&'a str>,
    explicit: bool,
    env: &'a BTreeMap<String, String>,
) -> SecretsRequest<'a> {
    SecretsRequest {
        worktree,
        profile,
        explicit,
        base_env: env,
    }
}

#[tokio::test]
/// fnox runs in the worktree with the profile flag and the client env.
async fn resolves_through_fnox() {
    let bin = tempfile::tempdir().expect("tempdir");
    let lane = tempfile::tempdir().expect("tempdir");
    std::fs::write(lane.path().join("fnox.toml"), "").expect("write");
    let fake = script(
        bin.path(),
        "fnox",
        r#"[ "$*" = "--profile dev env --json --non-interactive" ] || { echo "bad args: $*" >&2; exit 2; }
printf '{"schema":1,"set":{"TOKEN":"t","MARK":"%s","DIR":"%s"},"remove":["FNOX_AGE_KEY"],"files":{},"missing":[]}\n' "$CLIENT_MARK" "$(pwd -P)""#,
    );
    let env = BTreeMap::from([("CLIENT_MARK".to_owned(), "client".to_owned())]);
    let provider = FnoxProvider::new(fake, Duration::from_secs(5));
    let secrets = provider
        .env(&request(lane.path(), Some("dev"), false, &env))
        .await
        .expect("resolves");
    assert_eq!(secrets.set["TOKEN"], "t");
    assert_eq!(secrets.set["MARK"], "client");
    assert_eq!(
        secrets.set["DIR"],
        lane.path()
            .canonicalize()
            .expect("canonical")
            .display()
            .to_string()
    );
    assert!(secrets.remove.contains("FNOX_AGE_KEY"));
}

#[tokio::test]
/// Without a config or an explicit `fnox:` key, fnox is not run at all.
async fn skips_without_config() {
    let lane = tempfile::tempdir().expect("tempdir");
    let provider = FnoxProvider::new("/nonexistent/fnox", Duration::from_secs(1));
    let env = BTreeMap::new();
    let secrets = provider
        .env(&request(lane.path(), None, false, &env))
        .await
        .expect("empty");
    assert!(secrets.set.is_empty());
    let error = provider
        .env(&request(lane.path(), None, true, &env))
        .await
        .err()
        .expect("explicit fnox needs the binary");
    assert!(matches!(error, SecretsError::Other(m) if m.contains("not installed")));
}

#[tokio::test]
/// Error documents map to locked or other; missing documents and hangs are errors.
async fn failures() {
    let bin = tempfile::tempdir().expect("tempdir");
    let lane = tempfile::tempdir().expect("tempdir");
    let env = BTreeMap::new();
    let locked = script(
        bin.path(),
        "locked",
        r#"echo '{"schema":1,"error":{"kind":"resolution","message":"1Password requires an interactive sign in"}}'; exit 1"#,
    );
    let error = FnoxProvider::new(locked, Duration::from_secs(5))
        .env(&request(lane.path(), None, true, &env))
        .await
        .err()
        .expect("locked");
    assert!(matches!(error, SecretsError::Locked { .. }));

    let old = script(
        bin.path(),
        "old",
        "echo \"error: unrecognized subcommand 'env'\" >&2; exit 2",
    );
    let error = FnoxProvider::new(old, Duration::from_secs(5))
        .env(&request(lane.path(), None, true, &env))
        .await
        .err()
        .expect("no document");
    assert!(
        matches!(&error, SecretsError::Other(m) if m.contains("1.39+") && m.contains("unrecognized"))
    );

    let slow = script(bin.path(), "slow", "exec /bin/sleep 5");
    let error = FnoxProvider::new(slow, Duration::from_millis(200))
        .env(&request(lane.path(), None, true, &env))
        .await
        .err()
        .expect("timeout");
    assert!(matches!(error, SecretsError::Other(m) if m.contains("timed out")));
}

#[tokio::test]
#[ignore = "needs fnox 1.39+"]
/// Real fnox: a plain default secret resolves.
async fn real_fnox() {
    let lane = tempfile::tempdir().expect("tempdir");
    std::fs::write(
        lane.path().join("fnox.toml"),
        "[secrets]\nDEVLANES_FNOX_TEST = { default = \"yes\" }\n",
    )
    .expect("write");
    let env: BTreeMap<String, String> = std::env::vars().collect();
    let secrets = FnoxProvider::default()
        .env(&request(lane.path(), None, true, &env))
        .await
        .expect("fnox env");
    assert_eq!(secrets.set["DEVLANES_FNOX_TEST"], "yes");
}

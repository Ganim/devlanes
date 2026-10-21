//! The `devlanes-hosts` binary against a temp hosts file.
//!
//! `--hosts-file` exists only in debug builds, so these tests are compiled out of
//! release test runs. The helper only runs on Unix.
#![cfg(all(unix, debug_assertions))]

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use devlanes_hosts_helper::block::{BEGIN_MARKER, END_MARKER, NOTE};
use devlanes_hosts_helper::exit;

const LIMIT: Duration = Duration::from_secs(10);

/// Runs the helper on `hosts` with `stdin`, outside sudo.
fn helper(hosts: &Path, args: &[&str], stdin: &[u8]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_devlanes-hosts"));
    command
        .arg("--hosts-file")
        .arg(hosts)
        .args(args)
        .env_remove("SUDO_UID");
    run(command, stdin)
}

/// Runs `command` with `stdin`, killing it after [`LIMIT`].
fn run(mut command: Command, stdin: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn helper");
    if let Some(mut pipe) = child.stdin.take() {
        // The helper may exit before reading, e.g. on a refused --tld.
        match pipe.write_all(stdin) {
            Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                panic!("write stdin: {error}")
            }
            _ => {}
        }
    }
    let started = Instant::now();
    while child.try_wait().expect("poll helper").is_none() {
        if started.elapsed() > LIMIT {
            let _ = child.kill();
            panic!("helper did not exit within {LIMIT:?}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    child.wait_with_output().expect("collect output")
}

fn code(output: &Output) -> u8 {
    u8::try_from(output.status.code().expect("exit code")).expect("small code")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn seeded(content: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let hosts = dir.path().join("hosts");
    fs::write(&hosts, content).expect("seed hosts");
    fs::set_permissions(&hosts, fs::Permissions::from_mode(0o644)).expect("chmod");
    (dir, hosts)
}

const STOCK: &[u8] = b"127.0.0.1\tlocalhost\n::1\tlocalhost\n";

#[test]
fn apply_status_clear_round_trip() {
    let (_dir, hosts) = seeded(STOCK);
    let applied = helper(
        &hosts,
        &["apply"],
        b"web.acme-2.localhost\napi.acme-2.localhost\n",
    );
    assert_eq!(code(&applied), exit::OK, "{applied:?}");
    assert_eq!(stdout(&applied), "changed names=2\n");
    let block = format!(
        "{BEGIN_MARKER}\n{NOTE}\n127.0.0.1 api.acme-2.localhost\n::1 api.acme-2.localhost\n\
         127.0.0.1 web.acme-2.localhost\n::1 web.acme-2.localhost\n{END_MARKER}\n"
    );
    let mut expected = STOCK.to_vec();
    expected.extend_from_slice(block.as_bytes());
    assert_eq!(fs::read(&hosts).expect("read"), expected);
    assert_eq!(
        fs::metadata(&hosts).expect("stat").permissions().mode() & 0o777,
        0o644
    );

    let again = helper(
        &hosts,
        &["apply"],
        b"api.acme-2.localhost\nweb.acme-2.localhost\n",
    );
    assert_eq!(stdout(&again), "unchanged names=2\n");

    let status = helper(&hosts, &["status"], b"");
    assert_eq!(code(&status), exit::OK);
    assert_eq!(stdout(&status), block);

    let cleared = helper(&hosts, &["clear"], b"");
    assert_eq!(stdout(&cleared), "changed names=0\n");
    assert_eq!(fs::read(&hosts).expect("read"), STOCK);
    assert_eq!(stdout(&helper(&hosts, &["status"], b"")), "");
    assert_eq!(
        stdout(&helper(&hosts, &["clear"], b"")),
        "unchanged names=0\n"
    );
}

#[test]
fn tld_widens_the_allowed_names() {
    let (_dir, hosts) = seeded(STOCK);
    let refused = helper(&hosts, &["apply"], b"web.acme.test\n");
    assert_eq!(code(&refused), exit::INVALID_NAMES);
    let applied = helper(
        &hosts,
        &["apply", "--tld", "test"],
        b"web.acme.test\nweb.localhost\n",
    );
    assert_eq!(code(&applied), exit::OK, "{applied:?}");
    let public = helper(&hosts, &["apply", "--tld", "com"], b"web.example.com\n");
    assert_eq!(code(&public), exit::INVALID_NAMES);
    assert!(String::from_utf8_lossy(&public.stderr).contains("the TLD must be"));
}

#[test]
fn invalid_input_leaves_the_file_untouched() {
    let (_dir, hosts) = seeded(STOCK);
    for input in [
        &b"ok.localhost\n*.wild.localhost\n"[..],
        b"Upper.localhost\n",
        b"evil.com\n",
        b"a.localhost 10.0.0.1\n",
    ] {
        let output = helper(&hosts, &["apply"], input);
        assert_eq!(code(&output), exit::INVALID_NAMES, "{input:?}");
        assert!(String::from_utf8_lossy(&output.stderr).starts_with("devlanes-hosts: "));
    }
    let too_many: String = (0..=devlanes_hosts_helper::names::MAX_NAMES)
        .map(|i| format!("n{i}.localhost\n"))
        .collect();
    assert_eq!(
        code(&helper(&hosts, &["apply"], too_many.as_bytes())),
        exit::INVALID_NAMES
    );
    assert_eq!(fs::read(&hosts).expect("read"), STOCK);
}

#[test]
fn refuses_symlinks_and_duplicate_markers() {
    let (dir, hosts) = seeded(STOCK);
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&hosts, &link).expect("symlink");
    let output = helper(&link, &["apply"], b"a.localhost\n");
    assert_eq!(code(&output), exit::REFUSED_FILE);
    assert_eq!(fs::read(&hosts).expect("read"), STOCK);

    let twice = format!("{BEGIN_MARKER}\n{END_MARKER}\n{BEGIN_MARKER}\n{END_MARKER}\n");
    let (_dir2, doubled) = seeded(twice.as_bytes());
    let output = helper(&doubled, &["apply"], b"a.localhost\n");
    assert_eq!(code(&output), exit::REFUSED_FILE);
    assert!(String::from_utf8_lossy(&output.stderr).contains("appears on lines 1, 3"));
    assert_eq!(
        code(&helper(&doubled, &["status"], b"")),
        exit::REFUSED_FILE
    );
    assert_eq!(fs::read(&doubled).expect("read"), twice.as_bytes());
}

#[test]
fn version_and_usage() {
    let (_dir, hosts) = seeded(STOCK);
    let version = helper(&hosts, &["version"], b"");
    assert_eq!(
        stdout(&version),
        format!(
            "devlanes-hosts {} protocol={} build=debug\n",
            env!("CARGO_PKG_VERSION"),
            devlanes_hosts_helper::PROTOCOL
        )
    );
    assert_eq!(code(&helper(&hosts, &["frobnicate"], b"")), exit::USAGE);
    let missing = helper(&hosts.with_file_name("absent"), &["status"], b"");
    assert_eq!(code(&missing), exit::IO);
}

#[test]
fn hosts_file_override_is_refused_under_sudo() {
    let (_dir, hosts) = seeded(STOCK);
    let mut command = Command::new(env!("CARGO_BIN_EXE_devlanes-hosts"));
    command
        .arg("--hosts-file")
        .arg(&hosts)
        .arg("clear")
        .env("SUDO_UID", "501");
    assert_eq!(code(&run(command, b"")), exit::USAGE);
}

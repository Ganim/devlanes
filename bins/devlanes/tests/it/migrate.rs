//! `devlanes migrate` through the real binary. It never contacts the daemon, so each test
//! only needs its own project directory and home.

use std::{
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

/// Longest a migrate run may take before the test kills it.
const TIMEOUT: Duration = Duration::from_secs(60);

/// Runs `devlanes <args>` in `dir` with an isolated home; kills it after [`TIMEOUT`].
fn devlanes(dir: &Path, home: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_devlanes"))
        .args(args)
        .current_dir(dir)
        .env("HOME", home)
        .env("DEVLANES_HOME", home.join(".devlanes"))
        .env("DEVLANES_AGENT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn devlanes");
    let started = Instant::now();
    loop {
        if child.try_wait().expect("wait").is_some() {
            return child.wait_with_output().expect("output");
        }
        if started.elapsed() > TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            panic!("devlanes {args:?} did not finish within {TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A project directory under a temporary home.
fn project(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
    let home = tempfile::tempdir().expect("tempdir");
    let dir = home.path().join("acme");
    std::fs::create_dir_all(dir.join(".git")).expect("project");
    for (name, text) in files {
        std::fs::write(dir.join(name), text).expect("write");
    }
    (home, dir)
}

/// Text of a stream.
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn dry_run_prints_the_yaml_and_writes_nothing() {
    let (home, dir) = project(&[("Procfile", "web: bin/rails server -p $PORT\n")]);
    let out = devlanes(
        &dir,
        home.path(),
        &["migrate", "--dry-run", "--output", "plain"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let stdout = text(&out.stdout);
    assert!(stdout.contains("project: acme\n"), "{stdout}");
    assert!(
        stdout.contains("run: bin/rails server -p $PORT\n"),
        "{stdout}"
    );
    assert!(!dir.join("devlanes.yaml").exists());
}

#[test]
fn writes_once_and_refuses_to_overwrite_without_force() {
    let (home, dir) = project(&[("mprocs.yaml", "procs:\n  web: npm run dev\n")]);
    let out = devlanes(&dir, home.path(), &["migrate", "--output", "plain"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).starts_with("wrote devlanes.yaml from mprocs.yaml (1 service, 0 tasks)")
    );
    std::fs::write(dir.join("devlanes.yaml"), "project: kept\n").expect("write");

    let out = devlanes(&dir, home.path(), &["migrate", "--output", "plain"]);
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(stderr.contains("devlanes.yaml already exists"), "{stderr}");
    assert!(
        stderr.contains("hint: devlanes migrate --output plain --force"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("devlanes.yaml")).expect("read"),
        "project: kept\n"
    );

    let out = devlanes(
        &dir,
        home.path(),
        &["migrate", "--force", "--output", "plain"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let written = std::fs::read_to_string(dir.join("devlanes.yaml")).expect("read");
    assert!(written.contains("run: npm run dev"), "{written}");
}

#[test]
fn no_source_exits_3_naming_the_files() {
    let (home, dir) = project(&[]);
    let out = devlanes(&dir, home.path(), &["migrate", "--output", "plain"]);
    assert_eq!(out.status.code(), Some(3));
    let stderr = text(&out.stderr);
    for name in [
        "mprocs.yaml",
        "dekit.yaml",
        "process-compose.yaml",
        "Procfile",
        "ecosystem.config.js",
        ".tmuxinator.yml",
        ".tmuxp.yaml",
        "package.json",
        "mise.toml",
    ] {
        assert!(stderr.contains(name), "{name}: {stderr}");
    }
    let out = devlanes(
        &dir,
        home.path(),
        &["migrate", "--from", "overmind", "--output", "plain"],
    );
    assert_eq!(out.status.code(), Some(3));
    assert!(
        text(&out.stderr).contains("no procfile setup"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn json_output_has_path_sources_yaml_and_notes() {
    let (home, dir) = project(&[(
        "mprocs.yaml",
        "procs:\n  web:\n    shell: vite --port 5173\n    autorestart: true\n",
    )]);
    let out = devlanes(&dir, home.path(), &["migrate", "--json"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let data: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(data["written"], true);
    assert!(
        std::path::Path::new(data["path"].as_str().expect("path"))
            .ends_with(std::path::Path::new("acme").join("devlanes.yaml"))
    );
    assert_eq!(data["sources"][0]["source"], "mprocs");
    assert_eq!(data["sources"][0]["files"][0], "mprocs.yaml");
    let yaml = data["yaml"].as_str().expect("yaml");
    assert!(yaml.contains("run: vite --port ${{ self.port }}"), "{yaml}");
    assert_eq!(
        std::fs::read_to_string(dir.join("devlanes.yaml")).expect("read"),
        yaml
    );
    let notes = data["notes"].as_array().expect("notes");
    assert!(notes.iter().any(|n| {
        n["unit"] == "web"
            && n["message"]
                .as_str()
                .is_some_and(|m| m.contains("autorestart"))
    }));
}

#[test]
fn conflicting_sources_without_a_terminal_list_the_commands() {
    let (home, dir) = project(&[
        ("mprocs.yaml", "procs:\n  web: npm run dev\n"),
        ("Procfile", "web: bin/rails server\n"),
    ]);
    let out = devlanes(
        &dir,
        home.path(),
        &["migrate", "--dry-run", "--output", "plain"],
    );
    assert_eq!(out.status.code(), Some(2));
    let stderr = text(&out.stderr);
    assert!(
        stderr.contains("mprocs and procfile define the same names: web"),
        "{stderr}"
    );
    assert!(
        stderr.contains("devlanes migrate --from procfile --dry-run"),
        "{stderr}"
    );
    let out = devlanes(
        &dir,
        home.path(),
        &[
            "migrate",
            "--from",
            "procfile",
            "--dry-run",
            "--output",
            "plain",
        ],
    );
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
}

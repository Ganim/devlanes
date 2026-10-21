use devlanes_core::contrib::{ScriptRequest, ScriptRunner};
use devlanes_run_script_cargo::{BinRequest, CargoBins, CargoError, package_bins, plan};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Creates `files` (path, contents) under `root`, with parent directories.
fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write file");
    }
}

fn request<'a>(
    worktree: &'a Path,
    source: &'a str,
    name: &'a str,
    extra_args: &'a [String],
) -> BinRequest<'a> {
    BinRequest {
        worktree,
        source: Path::new(source),
        name,
        extra_args,
    }
}

/// A workspace with a root package, members with every target style and an excluded member.
fn fixture(root: &Path) {
    write_files(
        root,
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"acme\"\n[workspace]\nmembers = [\"crates/*\", \"tools/gen\"]\nexclude = [\"crates/old\"]\n",
            ),
            ("src/main.rs", "fn main() {}"),
            (
                "crates/api/Cargo.toml",
                "[package]\nname = \"acme-api\"\n\n[[bin]]\nname = \"server\"\npath = \"src/main.rs\"\n",
            ),
            ("crates/api/src/main.rs", "fn main() {}"),
            ("crates/api/src/bin/migrate.rs", "fn main() {}"),
            ("crates/api/src/bin/seed/main.rs", "fn main() {}"),
            (
                "crates/lib/Cargo.toml",
                "[package]\nname = \"acme-lib\"\nautobins = false\n",
            ),
            ("crates/lib/src/main.rs", "fn main() {}"),
            ("crates/old/Cargo.toml", "[package]\nname = \"old\"\n"),
            ("crates/old/src/bin/server.rs", "fn main() {}"),
            ("tools/gen/Cargo.toml", "[package]\nname = \"gen\"\n"),
            ("tools/gen/src/bin/migrate.rs", "fn main() {}"),
        ],
    );
}

/// Target discovery follows Cargo's rules.
#[test]
fn discovers_binaries() {
    let temp = tempfile::tempdir().expect("tempdir");
    fixture(temp.path());
    let bins = |path: &str| {
        package_bins(&temp.path().join(path))
            .expect("read")
            .map(|p| p.bins)
    };
    assert_eq!(
        bins("crates/api/Cargo.toml"),
        Some(vec!["migrate".into(), "seed".into(), "server".into()])
    );
    assert_eq!(bins("crates/lib/Cargo.toml"), Some(Vec::new()));
    assert_eq!(bins("Cargo.toml"), Some(vec!["acme".into()]));
}

/// Member manifests run in place; a root manifest selects the member with `-p`.
#[test]
fn renders_cargo_run() {
    let temp = tempfile::tempdir().expect("tempdir");
    fixture(temp.path());
    write_files(
        temp.path(),
        &[
            ("virtual/Cargo.toml", "[workspace]\nmembers = [\"svc\"]\n"),
            ("virtual/svc/Cargo.toml", "[package]\nname = \"svc\"\n"),
            ("virtual/svc/src/main.rs", "fn main() {}"),
        ],
    );
    let args = ["--port".to_owned(), "20010".to_owned()];
    let mut rendered = String::new();
    for (source, name, args) in [
        ("crates/api/Cargo.toml", "server", &args[..]),
        ("crates/api/Cargo.toml", "seed", &[][..]),
        ("Cargo.toml", "acme", &[][..]),
        ("Cargo.toml", "server", &args[..]),
        ("virtual/Cargo.toml", "svc", &[][..]),
    ] {
        let plan = plan(&request(temp.path(), source, name, args)).expect("plan");
        let cwd = plan
            .cwd
            .strip_prefix(temp.path())
            .expect("inside worktree")
            .display()
            .to_string();
        rendered.push_str(&format!(
            "{source}#{name}: cwd={cwd:?} package={} argv={}\n",
            plan.package,
            plan.argv.join(" ")
        ));
    }
    insta::assert_snapshot!(rendered, @r#"
    crates/api/Cargo.toml#server: cwd="crates/api" package=acme-api argv=cargo run --bin server -- --port 20010
    crates/api/Cargo.toml#seed: cwd="crates/api" package=acme-api argv=cargo run --bin seed
    Cargo.toml#acme: cwd="" package=acme argv=cargo run --bin acme
    Cargo.toml#server: cwd="" package=acme-api argv=cargo run -p acme-api --bin server -- --port 20010
    virtual/Cargo.toml#svc: cwd="virtual" package=svc argv=cargo run -p svc --bin svc
    "#);
}

/// Missing, ambiguous and disabled binaries are errors with the alternatives.
#[test]
fn errors() {
    let temp = tempfile::tempdir().expect("tempdir");
    fixture(temp.path());
    let message = |source: &str, name: &str| {
        plan(&request(temp.path(), source, name, &[]))
            .expect_err("error")
            .to_string()
            .replace(&temp.path().display().to_string(), "<lane>")
    };
    insta::assert_snapshot!(
        [
            message("crates/api/Cargo.toml", "worker"),
            message("crates/lib/Cargo.toml", "acme-lib"),
            message("Cargo.toml", "migrate"),
        ]
        .join("\n"),
        @r"
    `<lane>/crates/api/Cargo.toml` has no binary `worker` (binaries: migrate, seed, server)
    `<lane>/crates/lib/Cargo.toml` has no binary `acme-lib` (it builds no binaries)
    binary `migrate` exists in several workspace members (acme-api, gen); point run_script at the member's Cargo.toml
    "
    );
    assert!(matches!(
        plan(&request(temp.path(), "../Cargo.toml", "x", &[])),
        Err(CargoError::OutsideWorktree { .. })
    ));
    assert!(matches!(
        plan(&request(temp.path(), "crates/api/Cargo.toml", "--bin", &[])),
        Err(CargoError::InvalidName { .. })
    ));
}

/// The core adapter matches Cargo.toml and renders the plan.
#[tokio::test]
async fn adapter_resolves_through_core_trait() {
    let temp = tempfile::tempdir().expect("tempdir");
    fixture(temp.path());
    let runner = CargoBins;
    assert_eq!(runner.id(), "cargo");
    assert!(runner.matches(Path::new("crates/api/Cargo.toml")));
    assert!(!runner.matches(Path::new("Cargo.lock")));
    let args = vec!["--port".to_owned(), "1".to_owned()];
    let request = ScriptRequest {
        worktree: temp.path(),
        source: Path::new("crates/api/Cargo.toml"),
        name: "server",
        extra_args: &args,
    };
    let plan = runner.resolve(&request).await.expect("resolve");
    assert_eq!(
        plan.argv,
        ["cargo", "run", "--bin", "server", "--", "--port", "1"]
    );
    assert_eq!(plan.cwd, temp.path().join("crates/api"));
    let missing = ScriptRequest {
        name: "worker",
        ..request
    };
    let error = runner.resolve(&missing).await.expect_err("missing bin");
    assert!(
        matches!(error, devlanes_core::Error::InvalidInput(_)),
        "{error}"
    );
}

/// `--port` reaches the binary through a real `cargo run`.
#[test]
#[ignore = "needs cargo"]
fn real_cargo_forwards_port() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            (
                "Cargo.toml",
                "[package]\nname = \"argv\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
            ),
            (
                "src/main.rs",
                "fn main() { println!(\"{:?}\", std::env::args().skip(1).collect::<Vec<_>>()); }\n",
            ),
        ],
    );
    let args = vec!["--port".to_owned(), "4321".to_owned()];
    let plan = plan(&request(temp.path(), "Cargo.toml", "argv", &args)).expect("plan");
    let mut child = Command::new(&plan.argv[0])
        .args(&plan.argv[1..])
        .current_dir(&plan.cwd)
        .env("CARGO_TARGET_DIR", temp.path().join("target"))
        // An argument would land after `--` and reach the binary instead.
        .env("CARGO_TERM_QUIET", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn cargo");
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll cargo") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("cargo timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output).expect("read stdout");
    }
    assert!(status.success(), "cargo failed: {status}");
    assert!(output.contains(r#"["--port", "4321"]"#), "{output}");
}

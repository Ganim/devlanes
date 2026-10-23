use devlanes_core::contrib::{self, ScriptRunner};
use devlanes_run_script_python::{PythonError, PythonScripts, ScriptRequest, plan};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const PACKAGED: &str = r#"
[project]
name = "acme"
version = "0.1.0"
requires-python = ">=3.9"

[project.scripts]
serve = "acme:main"

[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"
"#;

/// Creates `files` (path, contents) under `root`, with parent directories.
fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write file");
    }
}

/// `text` as is: the snapshots show Unix separators.
#[cfg(unix)]
fn slashes(text: String) -> String {
    text
}

/// `text` with `/` for `\`, so the Unix snapshots hold on Windows.
#[cfg(windows)]
fn slashes(text: String) -> String {
    text.replace('\\', "/")
}

fn request<'a>(
    worktree: &'a Path,
    source: &'a str,
    name: &'a str,
    extra_args: &'a [String],
) -> ScriptRequest<'a> {
    ScriptRequest {
        worktree,
        source: Path::new(source),
        name,
        extra_args,
    }
}

/// Runner choice, entry points and notes for each project layout.
#[test]
fn runner_selection_table() {
    let uv_only = "[project]\nname = \"acme\"\n[project.scripts]\nserve = \"acme:main\"\n[tool.uv]\ndev-dependencies = []\n";
    let uv_packaged = "[project]\nname = \"acme\"\n[project.scripts]\nserve = \"acme:main\"\n[tool.uv]\npackage = true\n";
    let poetry = "[tool.poetry]\nname = \"acme\"\n[tool.poetry.scripts]\nserve = { reference = \"acme.cli:serve\", type = \"console\" }\n";
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("uv.lock", &[("pyproject.toml", PACKAGED), ("uv.lock", "")]),
        ("[tool.uv], not packaged", &[("pyproject.toml", uv_only)]),
        (
            "[tool.uv] package = true",
            &[("pyproject.toml", uv_packaged)],
        ),
        (
            "poetry.lock",
            &[("pyproject.toml", poetry), ("poetry.lock", "")],
        ),
        (
            "uv and poetry",
            &[
                ("pyproject.toml", PACKAGED),
                ("uv.lock", ""),
                ("poetry.lock", ""),
            ],
        ),
        ("neither", &[("pyproject.toml", PACKAGED)]),
    ];
    let args = vec!["--port".to_owned(), "20010".to_owned()];
    let mut rendered = String::new();
    for (label, files) in cases {
        let temp = tempfile::tempdir().expect("tempdir");
        write_files(temp.path(), files);
        let line = match plan(&request(temp.path(), "pyproject.toml", "serve", &args)) {
            Ok(plan) => {
                let mut line = format!(
                    "{:?}: {} (entry {})",
                    plan.runner,
                    plan.argv.join(" "),
                    plan.entry_point.unwrap_or_default()
                );
                for note in plan.notes {
                    line.push_str(&format!("\n    note: {note}"));
                }
                line
            }
            Err(error) => format!(
                "error: {}",
                slashes(
                    error
                        .to_string()
                        .replace(&temp.path().display().to_string(), "<lane>")
                )
            ),
        };
        rendered.push_str(&format!("{label}: {line}\n"));
    }
    insta::assert_snapshot!(rendered);
}

/// A workspace member uses the uv.lock at the workspace root and runs in its own directory.
#[test]
fn uv_workspace_member() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            (
                "pyproject.toml",
                "[tool.uv.workspace]\nmembers = [\"packages/*\"]\n",
            ),
            ("uv.lock", ""),
            ("packages/api/pyproject.toml", PACKAGED),
        ],
    );
    let plan = plan(&request(
        temp.path(),
        "packages/api/pyproject.toml",
        "serve",
        &[],
    ))
    .expect("plan");
    assert_eq!(plan.argv, ["uv", "run", "serve"]);
    assert_eq!(plan.cwd, temp.path().join("packages/api"));
}

/// Missing scripts list both tables; bad sources and names are rejected.
#[test]
fn errors() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[(
            "pyproject.toml",
            "[project.scripts]\nserve = \"a:b\"\n[tool.poetry.scripts]\nworker = \"a:c\"\n",
        )],
    );
    let error = plan(&request(temp.path(), "pyproject.toml", "migrate", &[])).expect_err("missing");
    let message = slashes(
        error
            .to_string()
            .replace(&temp.path().display().to_string(), "<lane>"),
    );
    insta::assert_snapshot!(message, @"`<lane>/pyproject.toml` has no script `migrate` in [project.scripts] or [tool.poetry.scripts] (scripts: serve, worker)");
    assert!(matches!(
        plan(&request(temp.path(), "../pyproject.toml", "serve", &[])),
        Err(PythonError::OutsideWorktree { .. })
    ));
    assert!(matches!(
        plan(&request(temp.path(), "setup.py", "serve", &[])),
        Err(PythonError::NotPyproject { .. })
    ));
    assert!(matches!(
        plan(&request(temp.path(), "pyproject.toml", "-m", &[])),
        Err(PythonError::InvalidName { .. })
    ));
}

/// The core adapter matches pyproject.toml and renders the plan.
#[tokio::test]
async fn adapter_resolves_through_core_trait() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[("svc/pyproject.toml", PACKAGED), ("uv.lock", "")],
    );
    let runner = PythonScripts;
    assert_eq!(runner.id(), "python");
    assert!(runner.matches(Path::new("svc/pyproject.toml")));
    assert!(!runner.matches(Path::new("svc/setup.cfg")));
    let args = vec!["--port".to_owned(), "1".to_owned()];
    let request = contrib::ScriptRequest {
        worktree: temp.path(),
        source: Path::new("svc/pyproject.toml"),
        name: "serve",
        extra_args: &args,
    };
    let plan = runner.resolve(&request).await.expect("resolve");
    assert_eq!(plan.argv, ["uv", "run", "serve", "--port", "1"]);
    assert_eq!(plan.cwd, temp.path().join("svc"));
    assert_eq!(
        plan.detail.as_deref(),
        Some("uv (uv.lock), entry point acme:main")
    );
    assert!(plan.notes.is_empty(), "{:?}", plan.notes);
    let missing = contrib::ScriptRequest {
        name: "nope",
        ..request
    };
    let error = runner.resolve(&missing).await.expect_err("missing script");
    assert!(
        matches!(error, devlanes_core::Error::InvalidInput(_)),
        "{error}"
    );
}

/// Runs the plan with a deadline and returns stdout.
fn run(argv: &[String], dir: &Path) -> String {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .env("UV_NO_PROGRESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn runner");
    let deadline = Instant::now() + Duration::from_secs(300);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll runner") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{argv:?} timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output).expect("read stdout");
    }
    assert!(status.success(), "{argv:?} failed: {status}");
    output
}

/// Writes a packaged project whose `serve` script prints its arguments.
fn argv_project(root: &Path, extra: &[(&str, &str)]) {
    write_files(
        root,
        &[(
            "src/acme/__init__.py",
            "import sys\n\ndef main():\n    print(sys.argv[1:])\n",
        )],
    );
    write_files(root, extra);
}

/// `--port` reaches the script through `uv run`.
#[test]
#[ignore = "needs uv"]
fn real_uv_forwards_port() {
    let temp = tempfile::tempdir().expect("tempdir");
    // `[tool.uv]` selects uv without a hand-made lockfile, which uv would reject.
    let pyproject = format!("{PACKAGED}\n[tool.uv]\n");
    argv_project(temp.path(), &[("pyproject.toml", &pyproject)]);
    let args = vec!["--port".to_owned(), "4321".to_owned()];
    let plan = plan(&request(temp.path(), "pyproject.toml", "serve", &args)).expect("plan");
    assert!(run(&plan.argv, &plan.cwd).contains("['--port', '4321']"));
}

/// `--port` reaches the script through `poetry run`.
#[test]
#[ignore = "needs poetry"]
fn real_poetry_forwards_port() {
    let temp = tempfile::tempdir().expect("tempdir");
    argv_project(
        temp.path(),
        &[("pyproject.toml", PACKAGED), ("poetry.lock", "")],
    );
    let args = vec!["--port".to_owned(), "4321".to_owned()];
    let plan = plan(&request(temp.path(), "pyproject.toml", "serve", &args)).expect("plan");
    assert!(run(&plan.argv, &plan.cwd).contains("['--port', '4321']"));
}

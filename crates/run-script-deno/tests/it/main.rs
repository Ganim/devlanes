use devlanes_core::contrib::{ScriptRequest, ScriptRunner};
use devlanes_run_script_deno::{DenoError, DenoTasks, TaskRequest, plan};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CONFIG: &str = r#"{
  // JSONC: comments and trailing commas are allowed.
  "tasks": {
    "dev": "deno run --allow-net main.ts",
    "build": { "description": "Bundle", "command": "deno bundle main.ts", },
    "ci": { "dependencies": ["build"] },
  },
}
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
) -> TaskRequest<'a> {
    TaskRequest {
        worktree,
        source: Path::new(source),
        name,
        extra_args,
    }
}

/// `deno task <name> <args>` in the config's directory, with the task body.
#[test]
fn renders_deno_task() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            ("apps/api/deno.jsonc", CONFIG),
            ("both/deno.json", r#"{"tasks":{"dev":"a"}}"#),
            ("both/deno.jsonc", r#"{"tasks":{"dev":"b"}}"#),
        ],
    );
    let args = ["--port".to_owned(), "20010".to_owned()];
    let mut rendered = String::new();
    for (source, name, args) in [
        ("apps/api/deno.jsonc", "dev", &args[..]),
        ("apps/api/deno.jsonc", "build", &[][..]),
        ("apps/api/deno.jsonc", "ci", &[][..]),
        ("both/deno.jsonc", "dev", &[][..]),
        ("both/deno.json", "dev", &[][..]),
    ] {
        let plan = plan(&request(temp.path(), source, name, args)).expect("plan");
        let cwd = slashes(
            plan.cwd
                .strip_prefix(temp.path())
                .expect("inside worktree")
                .display()
                .to_string(),
        );
        rendered.push_str(&format!(
            "{source}#{name}: cwd={cwd} argv={} body={:?}\n",
            plan.argv.join(" "),
            plan.body
        ));
    }
    insta::assert_snapshot!(rendered, @r#"
    apps/api/deno.jsonc#dev: cwd=apps/api argv=deno task dev --port 20010 body=Some("deno run --allow-net main.ts")
    apps/api/deno.jsonc#build: cwd=apps/api argv=deno task build body=Some("deno bundle main.ts")
    apps/api/deno.jsonc#ci: cwd=apps/api argv=deno task ci body=None
    both/deno.jsonc#dev: cwd=both argv=deno task --config deno.jsonc dev body=Some("b")
    both/deno.json#dev: cwd=both argv=deno task dev body=Some("a")
    "#);
}

/// Missing tasks list the defined ones; bad sources and names are rejected.
#[test]
fn errors() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[("deno.json", CONFIG), ("bad/deno.json", "{ tasks: }")],
    );
    let error = plan(&request(temp.path(), "deno.json", "start", &[])).expect_err("missing task");
    let message = slashes(
        error
            .to_string()
            .replace(&temp.path().display().to_string(), "<lane>"),
    );
    insta::assert_snapshot!(message, @"`<lane>/deno.json` has no task `start` (tasks: build, ci, dev)");
    assert!(matches!(
        plan(&request(temp.path(), "bad/deno.json", "dev", &[])),
        Err(DenoError::Parse { .. })
    ));
    assert!(matches!(
        plan(&request(temp.path(), "../deno.json", "dev", &[])),
        Err(DenoError::OutsideWorktree { .. })
    ));
    assert!(matches!(
        plan(&request(temp.path(), "package.json", "dev", &[])),
        Err(DenoError::NotConfig { .. })
    ));
    assert!(matches!(
        plan(&request(temp.path(), "deno.json", "-h", &[])),
        Err(DenoError::InvalidName { .. })
    ));
}

/// The core adapter matches Deno configs and returns the body.
#[tokio::test]
async fn adapter_resolves_through_core_trait() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("deno.json", CONFIG)]);
    let runner = DenoTasks;
    assert_eq!(runner.id(), "deno");
    assert!(runner.matches(Path::new("apps/x/deno.jsonc")));
    assert!(!runner.matches(Path::new("package.json")));
    let request = ScriptRequest {
        worktree: temp.path(),
        source: Path::new("deno.json"),
        name: "dev",
        extra_args: &[],
    };
    let plan = runner.resolve(&request).await.expect("resolve");
    assert_eq!(plan.argv, ["deno", "task", "dev"]);
    assert_eq!(plan.body.as_deref(), Some("deno run --allow-net main.ts"));
    let missing = ScriptRequest {
        name: "nope",
        ..request
    };
    let error = runner.resolve(&missing).await.expect_err("missing task");
    assert!(
        matches!(error, devlanes_core::Error::InvalidInput(_)),
        "{error}"
    );
}

/// `--port` reaches the task command unchanged through the real binary.
#[test]
#[ignore = "needs deno"]
fn real_deno_forwards_port() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            ("argv.ts", "console.log(JSON.stringify(Deno.args));\n"),
            (
                "deno.json",
                r#"{"tasks":{"dev":"deno run argv.ts --fixed"}}"#,
            ),
        ],
    );
    let args = vec!["--port".to_owned(), "4321".to_owned()];
    let plan = plan(&request(temp.path(), "deno.json", "dev", &args)).expect("plan");
    let mut child = Command::new(&plan.argv[0])
        .args(&plan.argv[1..])
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn deno");
    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll deno") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("deno timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output).expect("read stdout");
    }
    assert!(status.success(), "deno failed: {status}");
    assert!(
        output.contains(r#"["--fixed","--port","4321"]"#),
        "{output}"
    );
}

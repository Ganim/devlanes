use devlanes_core::contrib::{ScriptRequest, ScriptRunner};
use devlanes_preset_next as next;
use devlanes_preset_vite as vite;
use devlanes_run_script_just::{JustError, JustRecipes, RecipeCheck, RecipeRequest, plan};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const JUSTFILE: &str = "\
dev *ARGS:
    @echo \"args=[{{ARGS}}]\"

fixtures:
    ./load.sh

serve +FLAGS:
    echo {{FLAGS}}

deploy env:
    echo {{env}}
";

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
) -> RecipeRequest<'a> {
    RecipeRequest {
        worktree,
        source: Path::new(source),
        name,
        extra_args,
    }
}

/// `just <recipe> <preset args>` with no `--`; `--justfile` for `*.just` files.
#[test]
fn preset_arg_rendering() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[("justfile", JUSTFILE), ("tools/web.just", JUSTFILE)],
    );
    let presets = [
        ("none", Vec::new()),
        (
            "vite",
            vite::contribution(20012, &vite::ViteOptions::default()).args,
        ),
        (
            "next",
            next::contribution(20013, &next::NextOptions::default()).args,
        ),
    ];
    let mut rendered = String::new();
    for source in ["justfile", "tools/web.just"] {
        for (preset, args) in &presets {
            let plan = plan(&request(temp.path(), source, "dev", args)).expect("plan");
            rendered.push_str(&format!("{source} x {preset}: {}\n", plan.argv.join(" ")));
        }
    }
    insta::assert_snapshot!(rendered, @r"
    justfile x none: just dev
    justfile x vite: just dev --port 20012 --strictPort --host 127.0.0.1
    justfile x next: just dev --port 20013 --hostname 127.0.0.1
    tools/web.just x none: just --justfile web.just dev
    tools/web.just x vite: just --justfile web.just dev --port 20012 --strictPort --host 127.0.0.1
    tools/web.just x next: just --justfile web.just dev --port 20013 --hostname 127.0.0.1
    ");
}

/// Arity errors that just would report only after starting.
#[test]
fn argument_errors() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("justfile", JUSTFILE)]);
    let args = vec!["--port".to_owned(), "1".to_owned()];
    let messages: Vec<String> = [
        plan(&request(temp.path(), "justfile", "fixtures", &args)),
        plan(&request(temp.path(), "justfile", "serve", &[])),
        plan(&request(temp.path(), "justfile", "deploy", &[])),
    ]
    .into_iter()
    .map(|result| result.expect_err("argument error").to_string())
    .collect();
    insta::assert_snapshot!(messages.join("\n"), @r"
    recipe `fixtures` takes no extra arguments (got --port 1); declare a variadic last parameter, e.g. `fixtures *ARGS:`, and use `{{ARGS}}` in its body
    recipe `serve` needs 1 positional argument(s), which run_script cannot supply; give the parameters defaults
    recipe `deploy` needs 1 positional argument(s), which run_script cannot supply; give the parameters defaults
    ");
    assert!(plan(&request(temp.path(), "justfile", "serve", &args)).is_ok());
}

/// Missing recipes are errors unless an import could provide them.
#[test]
fn missing_and_unverifiable_recipes() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            ("justfile", JUSTFILE),
            ("imports/justfile", "import 'shared.just'\n"),
        ],
    );
    let error = plan(&request(temp.path(), "justfile", "seed", &[])).expect_err("missing recipe");
    let message = slashes(
        error
            .to_string()
            .replace(&temp.path().display().to_string(), "<lane>"),
    );
    insta::assert_snapshot!(message, @"`<lane>/justfile` has no recipe `seed` (recipes: deploy, dev, fixtures, serve)");
    let plan = plan(&request(temp.path(), "imports/justfile", "seed", &[])).expect("plan");
    assert_eq!(
        plan.check,
        RecipeCheck::Unverifiable {
            reason: "it imports other justfiles"
        }
    );
    assert_eq!(plan.cwd, temp.path().join("imports"));
}

/// The core adapter matches justfiles, forwards args and maps errors.
#[tokio::test]
async fn adapter_resolves_through_core_trait() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("Justfile", JUSTFILE)]);
    let runner = JustRecipes;
    assert_eq!(runner.id(), "just");
    assert!(runner.matches(Path::new("Justfile")));
    assert!(runner.matches(Path::new("tools/web.just")));
    assert!(!runner.matches(Path::new("Makefile")));
    let args = vec!["--port".to_owned(), "1".to_owned()];
    let request = ScriptRequest {
        worktree: temp.path(),
        source: Path::new("Justfile"),
        name: "dev",
        extra_args: &args,
    };
    let plan = runner.resolve(&request).await.expect("resolve");
    assert_eq!(plan.argv, ["just", "dev", "--port", "1"]);
    assert_eq!(plan.cwd, temp.path());
    let fixtures = ScriptRequest {
        name: "fixtures",
        ..request
    };
    let error = runner
        .resolve(&fixtures)
        .await
        .expect_err("no variadic parameter");
    assert!(
        matches!(error, devlanes_core::Error::InvalidInput(_)),
        "{error}"
    );
}

/// Sources must stay inside the worktree.
#[test]
fn rejects_sources_outside_worktree() {
    let temp = tempfile::tempdir().expect("tempdir");
    assert!(matches!(
        plan(&request(temp.path(), "../justfile", "dev", &[])),
        Err(JustError::OutsideWorktree { .. })
    ));
}

/// Preset flags reach a `*ARGS` recipe through the real binary.
#[test]
#[ignore = "needs just"]
fn real_just_forwards_port() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("justfile", JUSTFILE)]);
    let args = vec!["--port".to_owned(), "4321".to_owned()];
    let plan = plan(&request(temp.path(), "justfile", "dev", &args)).expect("plan");
    let mut child = Command::new(&plan.argv[0])
        .args(&plan.argv[1..])
        .current_dir(&plan.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn just");
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll just") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("just timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output).expect("read stdout");
    }
    assert!(status.success(), "just failed: {status}");
    assert_eq!(output.trim(), "args=[--port 4321]");
}

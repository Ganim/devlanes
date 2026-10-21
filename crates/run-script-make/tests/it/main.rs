use devlanes_core::contrib::{ScriptRequest, ScriptRunner};
use devlanes_run_script_make::{MakeError, MakeTargets, TargetCheck, TargetRequest, plan};
use std::path::Path;

/// The core adapter matches Makefiles, renders `make <target>` and maps errors.
#[tokio::test]
async fn adapter_resolves_through_core_trait() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("infra/Makefile", "certs:\n\t./gen.sh\n")]);
    let runner = MakeTargets;
    assert_eq!(runner.id(), "make");
    assert!(runner.matches(Path::new("infra/Makefile")));
    assert!(runner.matches(Path::new("tools/build.mk")));
    assert!(!runner.matches(Path::new("package.json")));
    let request = ScriptRequest {
        worktree: temp.path(),
        source: Path::new("infra/Makefile"),
        name: "certs",
        extra_args: &[],
    };
    let plan = runner.resolve(&request).await.expect("resolve");
    assert_eq!(plan.argv, ["make", "certs"]);
    assert_eq!(plan.cwd, temp.path().join("infra"));
    assert_eq!(plan.body, None);
    let args = vec!["--port".to_owned()];
    let with_args = ScriptRequest {
        extra_args: &args,
        ..request
    };
    let error = runner.resolve(&with_args).await.expect_err("extra args");
    assert!(
        matches!(error, devlanes_core::Error::InvalidInput(_)),
        "{error}"
    );
}

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
) -> TargetRequest<'a> {
    TargetRequest {
        worktree,
        source: Path::new(source),
        name,
        extra_args,
    }
}

/// `make <target>` in the Makefile's directory; `-f` only when make would
/// otherwise read a different file.
#[test]
fn renders_make_argv() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            ("Makefile", "certs:\n\t./gen.sh\n"),
            ("tools/build.mk", "dist:\n\ttrue\n"),
            ("both/GNUmakefile", "other:\n"),
            ("both/Makefile", "certs:\n"),
        ],
    );
    let mut rendered = String::new();
    for (source, name) in [
        ("Makefile", "certs"),
        ("tools/build.mk", "dist"),
        ("both/Makefile", "certs"),
    ] {
        let plan = plan(&request(temp.path(), source, name, &[])).expect("plan");
        let cwd = plan
            .cwd
            .strip_prefix(temp.path())
            .expect("cwd inside worktree")
            .display()
            .to_string();
        rendered.push_str(&format!(
            "{source}#{name}: cwd={cwd:?} argv={}\n",
            plan.argv.join(" ")
        ));
    }
    insta::assert_snapshot!(rendered, @r#"
    Makefile#certs: cwd="" argv=make certs
    tools/build.mk#dist: cwd="tools" argv=make -f build.mk dist
    both/Makefile#certs: cwd="both" argv=make -f Makefile certs
    "#);
}

/// A certainly missing target is an error listing the known ones.
#[test]
fn missing_target_is_an_error() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[("Makefile", ".PHONY: certs\nbuild:\n\ttrue\n")],
    );
    let error = plan(&request(temp.path(), "Makefile", "certs", &[])).expect_err("missing target");
    let message = error
        .to_string()
        .replace(&temp.path().display().to_string(), "<lane>");
    insta::assert_snapshot!(message, @"`<lane>/Makefile` has no target `certs` (targets: build)");
}

/// When includes could define the target, make gets to decide.
#[test]
fn unverifiable_targets_still_run() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("Makefile", "include common.mk\n")]);
    let plan = plan(&request(temp.path(), "Makefile", "certs", &[])).expect("plan");
    assert_eq!(
        plan.check,
        TargetCheck::Unverifiable {
            reason: "it includes other makefiles"
        }
    );
    assert_eq!(plan.argv, ["make", "certs"]);
}

/// Preset arguments cannot reach a make recipe, so they are refused.
#[test]
fn extra_args_are_refused() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(temp.path(), &[("Makefile", "dev:\n\tvite\n")]);
    let args = vec!["--port".to_owned(), "1".to_owned()];
    let error = plan(&request(temp.path(), "Makefile", "dev", &args)).expect_err("extra args");
    insta::assert_snapshot!(error.to_string(), @"make cannot pass arguments to target `dev` (got --port 1); use a package.json script or a justfile recipe with a `*ARGS` parameter, or `run`");
}

/// Names make would treat as options or assignments, and escaping sources.
#[test]
fn rejects_bad_requests() {
    let temp = tempfile::tempdir().expect("tempdir");
    for name in ["", "-n", "A=1", "a b"] {
        assert!(matches!(
            plan(&request(temp.path(), "Makefile", name, &[])),
            Err(MakeError::InvalidName { .. })
        ));
    }
    assert!(matches!(
        plan(&request(temp.path(), "../Makefile", "x", &[])),
        Err(MakeError::OutsideWorktree { .. })
    ));
}

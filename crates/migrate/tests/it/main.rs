//! Each fixture directory is a project with one documented setup. The tests copy it to
//! `<home>/code/<name>`, so `~` and absolute roots resolve the same on every machine, then
//! snapshot the generated YAML and notes. `migrate` validates its output itself; the tests
//! check it again so a snapshot can never hold an invalid file.

use std::path::{Path, PathBuf};

use devlanes_migrate::{Detected, MigrateError, Migration, Options, SourceKind, check, detect};

/// A copied fixture and the options that point `~` at its parent home.
struct Project {
    _home: tempfile::TempDir,
    dir: PathBuf,
    options: Options,
}

/// Copies `tests/fixtures/<name>` to `<tmp>/code/<name>`.
fn project(name: &str) -> Project {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    let home = tempfile::tempdir().expect("temp dir");
    let dir = home.path().join("code").join(name);
    copy(&source, &dir);
    Project {
        options: Options {
            os: "macos".into(),
            home: Some(home.path().to_path_buf()),
            project: None,
        },
        dir,
        _home: home,
    }
}

/// Recursive copy that keeps permission bits (mise file tasks must stay executable).
fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).expect("create dir");
    for entry in std::fs::read_dir(from).expect("read fixture") {
        let entry = entry.expect("entry");
        let target = to.join(entry.file_name());
        if entry.file_type().expect("type").is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).expect("copy");
        }
    }
}

/// Detects and migrates a fixture.
fn run(name: &str) -> (Vec<Detected>, Result<Migration, MigrateError>) {
    let project = project(name);
    let found = detect(&project.dir);
    let result = devlanes_migrate::migrate(&project.dir, &found, &project.options);
    (found, result)
}

/// The YAML followed by the notes, one per line.
fn report(migration: &Migration) -> String {
    check(&migration.yaml).expect("generated YAML validates");
    let mut out = migration.yaml.clone();
    out.push_str("\n--- notes ---\n");
    for note in &migration.notes {
        match &note.unit {
            Some(unit) => out.push_str(&format!("{} [{unit}]: {}\n", note.source, note.message)),
            None => out.push_str(&format!("{}: {}\n", note.source, note.message)),
        }
    }
    out
}

/// Snapshot test for a fixture that holds exactly `kinds`.
fn snapshot(name: &str, kinds: &[SourceKind]) {
    let (found, result) = run(name);
    let found_kinds: Vec<SourceKind> = found.iter().map(|d| d.kind).collect();
    assert_eq!(found_kinds, kinds, "{name}: detected sources");
    let migration = result.unwrap_or_else(|error| panic!("{name}: {error}"));
    insta::assert_snapshot!(name, report(&migration));
}

#[test]
fn mprocs_yaml() {
    snapshot("mprocs-yaml", &[SourceKind::Mprocs]);
}

#[test]
fn mprocs_json() {
    snapshot("mprocs-json", &[SourceKind::Mprocs]);
}

#[test]
fn dekit() {
    snapshot("dekit", &[SourceKind::Dekit]);
}

#[test]
fn process_compose() {
    let (found, _) = run("process-compose");
    assert_eq!(
        found[0].files,
        ["process-compose.yaml", "process-compose.override.yaml"]
    );
    snapshot("process-compose", &[SourceKind::ProcessCompose]);
}

#[test]
fn procfile_overmind() {
    let (found, _) = run("procfile-overmind");
    assert_eq!(found[0].files, ["Procfile.dev", ".overmind.env"]);
    snapshot("procfile-overmind", &[SourceKind::Procfile]);
}

#[test]
fn procfile_foreman_with_compose() {
    snapshot("procfile-foreman", &[SourceKind::Procfile]);
}

#[test]
fn pm2_json() {
    snapshot("pm2-json", &[SourceKind::Pm2]);
}

#[test]
fn pm2_static_js() {
    snapshot("pm2-js", &[SourceKind::Pm2]);
}

#[test]
fn pm2_yaml() {
    snapshot("pm2-yaml", &[SourceKind::Pm2]);
}

#[test]
fn pm2_computed_js_asks_for_json() {
    let (found, result) = run("pm2-js-dynamic");
    assert_eq!(found.len(), 1);
    match result {
        Err(MigrateError::Empty { notes, .. }) => {
            assert!(
                notes[0]
                    .message
                    .contains("JSON.stringify(require('./ecosystem.config.js')"),
                "{notes:?}"
            );
        }
        other => panic!("expected Empty, got {other:?}"),
    }
}

#[test]
#[cfg_attr(
    windows,
    ignore = "POSIX absolute paths in fixtures are relative on Windows, ported in the Windows path step"
)]
fn tmuxinator() {
    snapshot("tmuxinator", &[SourceKind::Tmuxinator]);
}

#[test]
fn tmuxp_yaml() {
    snapshot("tmuxp-yaml", &[SourceKind::Tmuxp]);
}

#[test]
fn tmuxp_json() {
    snapshot("tmuxp-json", &[SourceKind::Tmuxp]);
}

#[test]
fn concurrently() {
    snapshot("concurrently", &[SourceKind::PackageJson]);
}

#[test]
fn concurrently_uses_installed_binaries() {
    let project = project("concurrently");
    let bin = project.dir.join("node_modules/.bin");
    std::fs::create_dir_all(&bin).expect("bin dir");
    std::fs::write(bin.join("tailwindcss"), "").expect("write");
    let migration =
        devlanes_migrate::migrate(&project.dir, &detect(&project.dir), &project.options)
            .expect("migrates");
    assert!(
        migration
            .yaml
            .contains("run: pnpm exec tailwindcss -w -i src/app.css\n"),
        "{}",
        migration.yaml
    );
    assert!(
        migration
            .yaml
            .contains("run: mkdocs serve -a localhost:${{ self.port }}\n"),
        "{}",
        migration.yaml
    );
    assert!(
        !migration
            .notes
            .iter()
            .any(|n| n.message.contains("assuming"))
    );
}

#[test]
fn npm_run_all() {
    snapshot("npm-run-all", &[SourceKind::PackageJson]);
}

#[test]
fn run_p() {
    snapshot("run-p", &[SourceKind::PackageJson]);
}

#[test]
fn mise() {
    snapshot("mise", &[SourceKind::Mise]);
}

#[test]
fn merges_sources_without_conflicts() {
    snapshot("merge", &[SourceKind::Mprocs, SourceKind::Procfile]);
}

#[test]
fn reports_conflicting_sources() {
    let (found, result) = run("conflict");
    assert_eq!(found.len(), 2);
    match result {
        Err(error @ MigrateError::Conflict { .. }) => {
            assert_eq!(
                error.to_string(),
                "mprocs and procfile conflict: web (mprocs, procfile)"
            );
        }
        other => panic!("expected a conflict, got {other:?}"),
    }
    // One source at a time works.
    let project = project("conflict");
    let only: Vec<Detected> = detect(&project.dir)
        .into_iter()
        .filter(|d| d.kind == SourceKind::Procfile)
        .collect();
    devlanes_migrate::migrate(&project.dir, &only, &project.options).expect("procfile alone");
}

#[test]
fn finds_nothing_in_an_unrelated_directory() {
    let dir = tempfile::tempdir().expect("temp dir");
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"dev":"vite"}}"#,
    )
    .expect("write");
    std::fs::write(
        dir.path().join("compose.yaml"),
        "services: {db: {image: postgres}}\n",
    )
    .expect("write");
    assert!(detect(dir.path()).is_empty());
}

#[test]
fn every_source_has_a_name_and_files() {
    for kind in SourceKind::ALL {
        assert_eq!(SourceKind::parse(kind.name()), Some(kind));
        assert!(!kind.file_names().is_empty(), "{kind}");
    }
    assert_eq!(SourceKind::parse("overmind"), Some(SourceKind::Procfile));
    assert_eq!(SourceKind::parse("auto"), None);
}

#[test]
fn project_name_follows_the_directory() {
    let home = tempfile::tempdir().expect("temp dir");
    let dir = home.path().join("Acme Web_App");
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("Procfile"), "web: ./server\n").expect("write");
    let migration =
        devlanes_migrate::migrate(&dir, &detect(&dir), &Options::default()).expect("migrates");
    assert_eq!(migration.project, "acme-web_app");
}

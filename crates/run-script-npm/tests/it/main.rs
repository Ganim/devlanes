use devlanes_core::contrib::{self, ScriptRunner};
use devlanes_run_script_npm::{
    Detection, DetectionSource, NpmError, NpmScripts, PackageManager, ScriptRequest,
    detect_package_manager, plan,
};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ALL: [PackageManager; 5] = [
    PackageManager::Npm,
    PackageManager::Pnpm,
    PackageManager::YarnClassic,
    PackageManager::YarnBerry,
    PackageManager::Bun,
];

/// Creates `files` (path, contents) under `root`, with parent directories.
fn write_files(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write file");
    }
}

/// Renders a detection with paths relative to `root` for stable snapshots.
fn describe(root: &Path, result: Result<Detection, NpmError>) -> String {
    let relative = |path: &Path| match path
        .strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
    {
        empty if empty.is_empty() => ".".to_owned(),
        other => other,
    };
    match result {
        Ok(detection) => {
            let source = match &detection.source {
                DetectionSource::PackageManagerField(path) => {
                    format!("packageManager in {}", relative(path))
                }
                DetectionSource::DevEngines(path) => format!("devEngines in {}", relative(path)),
                DetectionSource::Lockfile(path) => format!("lockfile {}", relative(path)),
                DetectionSource::Default => "default".to_owned(),
            };
            let mut line = format!("{:?} ({source})", detection.manager);
            if let Some(version) = &detection.version {
                line.push_str(&format!(" version={version}"));
            }
            if let Some(linker) = detection.yarn_linker {
                line.push_str(&format!(" linker={linker:?}"));
            }
            for note in &detection.notes {
                line.push_str(&format!("\n    note: {note}"));
            }
            line
        }
        Err(NpmError::AmbiguousLockfiles { dir, lockfiles }) => {
            format!(
                "error: ambiguous in `{}`: {}",
                relative(&dir),
                lockfiles.join(", ")
            )
        }
        Err(other) => format!("error: {other}"),
    }
}

/// Runs detection for `worktree/packages/app` over each case's files.
fn table(cases: &[(&str, &[(&str, &str)])]) -> String {
    let mut table = String::new();
    for (label, files) in cases {
        let temp = tempfile::tempdir().expect("tempdir");
        let worktree = temp.path().join("lane");
        write_files(
            &worktree,
            &[("packages/app/package.json", r#"{"name":"app"}"#)],
        );
        write_files(&worktree, files);
        let result = detect_package_manager(&worktree.join("packages/app"), &worktree);
        table.push_str(&format!("{label}: {}\n", describe(&worktree, result)));
    }
    slashes(table)
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

/// Declared fields: `packageManager` beats `devEngines`, nearest manifest
/// wins up to the workspace root, and the field beats any lockfile.
#[test]
fn declared_field_precedence() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        (
            "packageManager in the script's package.json",
            &[
                (
                    "packages/app/package.json",
                    r#"{"packageManager":"pnpm@10.4.1+sha512.abc"}"#,
                ),
                ("pnpm-lock.yaml", ""),
            ],
        ),
        (
            "packageManager at the workspace root",
            &[
                ("package.json", r#"{"packageManager":"pnpm@10.4.1"}"#),
                ("pnpm-lock.yaml", ""),
            ],
        ),
        (
            "nearest packageManager wins",
            &[
                (
                    "packages/app/package.json",
                    r#"{"packageManager":"bun@1.3.0"}"#,
                ),
                ("package.json", r#"{"packageManager":"pnpm@10.4.1"}"#),
            ],
        ),
        (
            "devEngines object",
            &[(
                "package.json",
                r#"{"devEngines":{"packageManager":{"name":"npm","version":"^11.0.0","onFail":"error"}}}"#,
            )],
        ),
        (
            "devEngines array, first supported entry",
            &[(
                "package.json",
                r#"{"devEngines":{"packageManager":[{"name":"deno"},{"name":"pnpm","version":">=10"}]}}"#,
            )],
        ),
        (
            "devEngines array, several supported",
            &[(
                "package.json",
                r#"{"devEngines":{"packageManager":[{"name":"pnpm"},{"name":"npm"}]}}"#,
            )],
        ),
        (
            "packageManager and devEngines disagree",
            &[(
                "package.json",
                r#"{"packageManager":"pnpm@10.4.1","devEngines":{"packageManager":{"name":"npm"}}}"#,
            )],
        ),
        (
            "field contradicts the lockfile",
            &[
                ("package.json", r#"{"packageManager":"pnpm@10.4.1"}"#),
                ("yarn.lock", ""),
            ],
        ),
        (
            "field resolves lockfiles of two managers",
            &[
                ("package.json", r#"{"packageManager":"pnpm@10.4.1"}"#),
                ("pnpm-lock.yaml", ""),
                ("package-lock.json", "{}"),
            ],
        ),
        (
            "unsupported packageManager",
            &[
                ("package.json", r#"{"packageManager":"deno@2.5.0"}"#),
                ("pnpm-lock.yaml", ""),
            ],
        ),
        (
            "yarn 4, Plug'n'Play without .pnp.cjs",
            &[
                ("package.json", r#"{"packageManager":"yarn@4.5.0"}"#),
                ("yarn.lock", ""),
            ],
        ),
        (
            "yarn 4, Plug'n'Play installed",
            &[
                ("package.json", r#"{"packageManager":"yarn@4.5.0"}"#),
                ("yarn.lock", ""),
                (".pnp.cjs", ""),
            ],
        ),
        (
            "yarn 4, node-modules linker",
            &[
                ("package.json", r#"{"packageManager":"yarn@4.5.0"}"#),
                ("yarn.lock", ""),
                (".yarnrc.yml", "nodeLinker: \"node-modules\"\n"),
            ],
        ),
        (
            "yarn 1 by version over a 2+ lockfile header",
            &[
                ("package.json", r#"{"packageManager":"yarn@1.22.22"}"#),
                ("yarn.lock", "__metadata:\n  version: 8\n"),
            ],
        ),
        (
            "field above the workspace root is not used",
            &[
                ("package.json", r#"{"packageManager":"bun@1.3.0"}"#),
                ("packages/pnpm-workspace.yaml", "packages: ['*']\n"),
            ],
        ),
    ];
    insta::assert_snapshot!(table(cases));
}

/// Without a declared field, the nearest lockfile decides and a note asks for the field.
#[test]
fn lockfile_fallback_table() {
    let cases: &[(&str, &[(&str, &str)])] = &[
        ("pnpm", &[("pnpm-lock.yaml", "")]),
        ("npm", &[("package-lock.json", "{}")]),
        ("npm shrinkwrap", &[("npm-shrinkwrap.json", "{}")]),
        (
            "yarn 1 header",
            &[(
                "yarn.lock",
                "# THIS IS AN AUTOGENERATED FILE.\n# yarn lockfile v1\n",
            )],
        ),
        (
            "yarn 2+ header",
            &[
                (
                    "yarn.lock",
                    "# This file is generated\n\n__metadata:\n  version: 8\n",
                ),
                (".pnp.cjs", ""),
            ],
        ),
        (
            "empty yarn.lock, .yarnrc.yml",
            &[("yarn.lock", ""), (".yarnrc.yml", "nodeLinker: pnpm\n")],
        ),
        ("empty yarn.lock alone", &[("yarn.lock", "")]),
        ("bun text lockfile", &[("bun.lock", "{}")]),
        ("bun binary lockfile", &[("bun.lockb", "")]),
        (
            "nearest lockfile wins",
            &[
                ("pnpm-lock.yaml", ""),
                ("packages/app/package-lock.json", "{}"),
            ],
        ),
        (
            "two managers",
            &[("pnpm-lock.yaml", ""), ("bun.lock", "{}")],
        ),
        ("nothing", &[]),
    ];
    insta::assert_snapshot!(table(cases));
}

/// Lockfiles above the lane worktree, such as a parent checkout's, are ignored.
#[test]
fn detection_stops_at_worktree() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[("pnpm-lock.yaml", ""), ("lane/package.json", "{}")],
    );
    let worktree = temp.path().join("lane");
    let detection = detect_package_manager(&worktree, &worktree).expect("detection");
    assert_eq!(detection.manager, PackageManager::Npm);
    assert_eq!(detection.source, DetectionSource::Default);
}

/// argv and `--` placement for every manager, with and without arguments.
#[test]
fn per_manager_argv() {
    let args = vec!["--port".to_owned(), "20012".to_owned()];
    let mut rendered = String::new();
    for manager in ALL {
        rendered.push_str(&format!(
            "{manager:?}: {} | {}\n",
            manager.run_argv("dev", &[]).join(" "),
            manager.run_argv("dev", &args).join(" ")
        ));
    }
    insta::assert_snapshot!(rendered, @r"
    Npm: npm run dev | npm run dev -- --port 20012
    Pnpm: pnpm run dev | pnpm run dev --port 20012
    YarnClassic: yarn run dev | yarn run dev --port 20012
    YarnBerry: yarn run dev | yarn run dev --port 20012
    Bun: bun run dev | bun run dev --port 20012
    ");
}

/// The plan runs in the manifest's directory and carries the body, version and notes.
#[test]
fn plan_uses_manifest_dir_and_exposes_body() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            ("package.json", r#"{"packageManager":"pnpm@10.4.1"}"#),
            ("pnpm-lock.yaml", ""),
            (
                "apps/api/package.json",
                r#"{"scripts":{"dev":"wrangler dev --persist-to .state"}}"#,
            ),
        ],
    );
    let args = vec!["--port".to_owned(), "20010".to_owned()];
    let request = ScriptRequest {
        worktree: temp.path(),
        source: Path::new("apps/api/package.json"),
        name: "dev",
        extra_args: &args,
    };
    let declared = plan(&request).expect("plan");
    assert_eq!(declared.argv, ["pnpm", "run", "dev", "--port", "20010"]);
    assert_eq!(declared.cwd, temp.path().join("apps/api"));
    assert_eq!(declared.manager, PackageManager::Pnpm);
    assert_eq!(declared.version.as_deref(), Some("10.4.1"));
    assert_eq!(
        declared.detail(temp.path()),
        "pnpm 10.4.1 (packageManager in package.json)"
    );
    assert!(declared.notes.is_empty(), "{:?}", declared.notes);
    assert_eq!(declared.script_body, "wrangler dev --persist-to .state");

    write_files(temp.path(), &[("package.json", "{}")]);
    let detected = plan(&request).expect("plan");
    assert_eq!(detected.manager, PackageManager::Pnpm);
    assert_eq!(
        detected.notes,
        [
            r#"no packageManager field; detected pnpm from pnpm-lock.yaml. Add "packageManager": "pnpm@<version>" to package.json"#
        ]
    );
}

/// A missing script names the file and lists the scripts that exist.
#[test]
fn missing_script_lists_available() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[(
            "package.json",
            r#"{"scripts":{"build":"tsc","start":"node ."}}"#,
        )],
    );
    let error = plan(&ScriptRequest {
        worktree: temp.path(),
        source: Path::new("package.json"),
        name: "dev",
        extra_args: &[],
    })
    .expect_err("missing script");
    let message = slashes(
        error
            .to_string()
            .replace(&temp.path().display().to_string(), "<lane>"),
    );
    insta::assert_snapshot!(message, @"`<lane>/package.json` has no script `dev` (available: build, start)");
}

/// Sources must stay inside the worktree and be package.json files.
#[test]
fn rejects_bad_sources() {
    let temp = tempfile::tempdir().expect("tempdir");
    let request = |source: &'static str, name: &'static str| ScriptRequest {
        worktree: temp.path(),
        source: Path::new(source),
        name,
        extra_args: &[],
    };
    assert!(matches!(
        plan(&request("../other/package.json", "dev")),
        Err(NpmError::OutsideWorktree { .. })
    ));
    assert!(matches!(
        plan(&request("Makefile", "dev")),
        Err(NpmError::NotManifest { .. })
    ));
    assert!(matches!(
        plan(&request("package.json", "--help")),
        Err(NpmError::InvalidName { .. })
    ));
    assert!(matches!(
        plan(&request("package.json", "dev")),
        Err(NpmError::Read { .. })
    ));
}

/// The core adapter matches package.json, places `--` for npm and returns the body.
#[tokio::test]
async fn adapter_resolves_through_core_trait() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            ("package-lock.json", "{}"),
            ("web/package.json", r#"{"scripts":{"dev":"vite"}}"#),
        ],
    );
    let runner = NpmScripts;
    assert_eq!(runner.id(), "npm");
    assert!(runner.matches(Path::new("web/package.json")));
    assert!(!runner.matches(Path::new("web/Makefile")));
    let args = vec!["--port".to_owned(), "1".to_owned()];
    let request = contrib::ScriptRequest {
        worktree: temp.path(),
        source: Path::new("web/package.json"),
        name: "dev",
        extra_args: &args,
    };
    let plan = runner.resolve(&request).await.expect("resolve");
    assert_eq!(plan.argv, ["npm", "run", "dev", "--", "--port", "1"]);
    assert_eq!(plan.cwd, temp.path().join("web"));
    assert_eq!(plan.body.as_deref(), Some("vite"));
    assert_eq!(
        plan.detail.as_deref(),
        Some("npm (lockfile package-lock.json)")
    );
    assert_eq!(
        plan.notes,
        [
            r#"no packageManager field; detected npm from package-lock.json. Add "packageManager": "npm@<version>" to package.json"#
        ]
    );
    let missing = contrib::ScriptRequest {
        name: "start",
        ..request
    };
    let error = runner.resolve(&missing).await.expect_err("missing script");
    assert!(
        matches!(error, devlanes_core::Error::InvalidInput(_)),
        "{error}"
    );
}

/// Runs `argv` in `dir` with a deadline, returning stdout on success.
fn run_with_timeout(argv: &[String], dir: &Path, timeout: Duration) -> String {
    let mut child = Command::new(&argv[0])
        .args(&argv[1..])
        .current_dir(dir)
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .env("npm_config_update_notifier", "false")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn package manager");
    let deadline = Instant::now() + timeout;
    let status = loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("{argv:?} timed out");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut output = String::new();
    if let Some(mut stdout) = child.stdout.take() {
        std::io::Read::read_to_string(&mut stdout, &mut output).expect("read stdout");
    }
    assert!(status.success(), "{argv:?} failed: {status}\n{output}");
    output
}

/// Returns the manager behind `program` if it is installed.
fn installed(program: &str) -> Option<PackageManager> {
    let output = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let version = String::from_utf8_lossy(&output.stdout);
    Some(match program {
        "npm" => PackageManager::Npm,
        "pnpm" => PackageManager::Pnpm,
        "bun" => PackageManager::Bun,
        "yarn" if version.trim_start().starts_with("1.") => PackageManager::YarnClassic,
        "yarn" => PackageManager::YarnBerry,
        _ => return None,
    })
}

/// `--port` reaches the script unchanged through every installed manager.
#[test]
#[ignore = "needs node"]
fn real_managers_forward_port() {
    let temp = tempfile::tempdir().expect("tempdir");
    write_files(
        temp.path(),
        &[
            (
                "argv.js",
                "console.log(JSON.stringify(process.argv.slice(2)))\n",
            ),
            (
                "package.json",
                r#"{"name":"forwarding","version":"1.0.0","license":"MIT","scripts":{"dev":"node argv.js --fixed"}}"#,
            ),
        ],
    );
    let managers: Vec<PackageManager> = ["npm", "pnpm", "yarn", "bun"]
        .into_iter()
        .filter_map(installed)
        .collect();
    assert!(!managers.is_empty(), "no package manager on PATH");
    let args = vec!["--port".to_owned(), "4321".to_owned()];
    for manager in managers {
        let output = run_with_timeout(
            &manager.run_argv("dev", &args),
            temp.path(),
            Duration::from_secs(60),
        );
        assert!(
            output.contains(r#"["--fixed","--port","4321"]"#),
            "{manager:?} did not forward --port:\n{output}"
        );
    }
}

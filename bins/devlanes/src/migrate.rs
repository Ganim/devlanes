//! `devlanes migrate`: finds an existing process-manager setup, converts it with
//! `devlanes-migrate` and writes `devlanes.yaml`. Runs in the client only: nothing here
//! needs the daemon.

use std::path::{Path, PathBuf};

use devlanes_cli::{ChoiceOption, MigratePlan, OutputMode, Picker, Rendered, Runtime, exit};
use devlanes_migrate::{Detected, MigrateError, Migration, Note, Options, SourceKind};
use serde_json::{Value, json};

use crate::output::emit;

/// Runs the command and returns the exit status.
pub fn run(runtime: &Runtime, plan: &MigratePlan) -> u8 {
    match execute(runtime, plan) {
        Ok(code) => code,
        Err(failure) => {
            let rendered = failure.render(runtime);
            emit(&rendered);
            rendered.exit_code
        }
    }
}

/// A refusal or error, with its JSON code and exit status.
struct Failure {
    code: &'static str,
    exit: u8,
    message: String,
    /// Indented lines under the message.
    context: Vec<String>,
    hint: Option<String>,
    data: Value,
}

impl Failure {
    /// A failure without extra lines or data.
    fn new(code: &'static str, exit: u8, message: String) -> Box<Self> {
        Box::new(Self {
            code,
            exit,
            message,
            context: Vec::new(),
            hint: None,
            data: json!({}),
        })
    }

    /// `error: ...` text, or `{"error": {...}}` in JSON mode, on stderr.
    fn render(&self, runtime: &Runtime) -> Rendered {
        let stderr = if runtime.mode == OutputMode::Json {
            let mut data = self.data.clone();
            data["message"] = json!(self.message);
            if let Some(hint) = &self.hint {
                data["hint"] = json!(hint);
            }
            format!(
                "{}\n",
                json!({"error": {"code": self.code, "exit_code": self.exit, "data": data}})
            )
        } else {
            let mut text = format!("error: {}\n", self.message);
            for line in &self.context {
                text.push_str(&format!("  {line}\n"));
            }
            if let Some(hint) = &self.hint {
                text.push_str(&format!("hint: {hint}\n"));
            }
            text
        };
        Rendered {
            stderr,
            exit_code: self.exit,
            ..Rendered::default()
        }
    }
}

/// The whole command.
fn execute(runtime: &Runtime, plan: &MigratePlan) -> Result<u8, Box<Failure>> {
    let kind =
        match plan.from.as_deref() {
            None | Some("auto") => None,
            Some(name) => Some(SourceKind::parse(name).ok_or_else(|| {
                Failure::new("usage", exit::USAGE, format!("unknown source {name}"))
            })?),
        };
    let root = git_root(&plan.cwd).unwrap_or_else(|| plan.cwd.clone());
    let (dir, found) = find(&root, &plan.cwd, kind).ok_or_else(|| nothing_found(&root, kind))?;
    let output = match &plan.to {
        Some(path) if path.is_absolute() => path.clone(),
        Some(path) => plan.cwd.join(path),
        None => dir.join("devlanes.yaml"),
    };
    if !plan.dry_run && !plan.force {
        refuse_overwrite(&output, &dir, plan.to.is_none(), &plan.cwd)?;
    }
    let options = Options {
        home: devlanes_client::user_home(),
        ..Options::default()
    };
    let migration = match devlanes_migrate::migrate(&dir, &found, &options) {
        Ok(migration) => migration,
        Err(MigrateError::Conflict { units, .. }) => {
            let chosen = choose_source(runtime, &found, &units, plan)?;
            devlanes_migrate::migrate(&dir, &[chosen], &options).map_err(failure)?
        }
        Err(error) => return Err(failure(error)),
    };
    if !plan.dry_run {
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent).map_err(|e| write_failed(&output, &e))?;
        }
        std::fs::write(&output, &migration.yaml).map_err(|e| write_failed(&output, &e))?;
    }
    emit(&report(runtime, plan, &output, &migration));
    Ok(exit::OK)
}

/// The Git worktree root above `cwd` (a `.git` directory or worktree file).
fn git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Setups in the repository root, else in the current directory.
fn find(root: &Path, cwd: &Path, kind: Option<SourceKind>) -> Option<(PathBuf, Vec<Detected>)> {
    let mut dirs = vec![root.to_path_buf()];
    if cwd != root {
        dirs.push(cwd.to_path_buf());
    }
    dirs.into_iter().find_map(|dir| {
        let found = match kind {
            Some(kind) => devlanes_migrate::detect_kind(&dir, kind)
                .into_iter()
                .collect(),
            None => devlanes_migrate::detect(&dir),
        };
        (!found.is_empty()).then_some((dir, found))
    })
}

/// Exit 3 with every file name that was looked for.
fn nothing_found(root: &Path, kind: Option<SourceKind>) -> Box<Failure> {
    let kinds: Vec<SourceKind> = match kind {
        Some(kind) => vec![kind],
        None => SourceKind::ALL.to_vec(),
    };
    let mut names: Vec<&str> = Vec::new();
    for kind in &kinds {
        for name in kind.file_names() {
            if !names.contains(name) {
                names.push(name);
            }
        }
    }
    let what = match kind {
        Some(kind) => format!("no {kind} setup"),
        None => "no setup to migrate".to_owned(),
    };
    let mut failure = Failure::new(
        "no_source",
        exit::NOT_FOUND,
        format!(
            "{what} in {}; looked for {}",
            root.display(),
            names.join(", ")
        ),
    );
    if kind.is_none() {
        failure.context.push(
            "package.json counts when a script runs concurrently, npm-run-all --parallel or run-p; mise.toml when it has [tasks]"
                .into(),
        );
    }
    failure.data = json!({"dir": root, "searched": names});
    failure
}

/// Exit 2 when the target exists (or a `.devlanes/devlanes.yaml` would shadow it).
fn refuse_overwrite(
    output: &Path,
    dir: &Path,
    default: bool,
    cwd: &Path,
) -> Result<(), Box<Failure>> {
    let nested = dir.join(".devlanes/devlanes.yaml");
    let existing = if output.exists() {
        output.to_path_buf()
    } else if default && nested.is_file() {
        nested
    } else {
        return Ok(());
    };
    let mut failure = Failure::new(
        "config_exists",
        exit::USAGE,
        format!("{} already exists; nothing written", shown(&existing, cwd)),
    );
    failure.hint = Some(rerun_with("--force"));
    failure.context.push(format!(
        "see the result first with {}",
        rerun_with("--dry-run")
    ));
    failure.data = json!({"path": existing});
    Err(failure)
}

/// This invocation again with `flag` added.
fn rerun_with(flag: &str) -> String {
    devlanes_cli::Hint::rerun_with(std::env::args().skip(1), flag).0
}

/// Sources that define the same names: pick one on a terminal, else exit 2 with the
/// command for each.
fn choose_source(
    runtime: &Runtime,
    found: &[Detected],
    units: &[(String, Vec<SourceKind>)],
    plan: &MigratePlan,
) -> Result<Detected, Box<Failure>> {
    let clashes: Vec<String> = units
        .iter()
        .map(|(name, kinds)| {
            let kinds: Vec<&str> = kinds.iter().map(|k| k.name()).collect();
            format!("{name} ({})", kinds.join(", "))
        })
        .collect();
    let options: Vec<ChoiceOption> = found
        .iter()
        .map(|d| ChoiceOption {
            value: d.kind.name().to_owned(),
            detail: d.files.join(", "),
        })
        .collect();
    if crate::choose::interactive(runtime) {
        let title = format!(
            "devlanes migrate: these setups define the same names ({}); choose one",
            clashes.join(", ")
        );
        let picked = crate::choose::pick(Picker::new(title, options, false)).map_err(|e| {
            Failure::new("io", exit::FAILED, format!("cannot show the picker: {e}"))
        })?;
        let Some(name) = picked.and_then(|values| values.into_iter().next()) else {
            return Err(Failure::new("cancelled", 130, "cancelled".into()));
        };
        return found
            .iter()
            .find(|d| d.kind.name() == name)
            .cloned()
            .ok_or_else(|| Failure::new("cancelled", 130, "cancelled".into()));
    }
    let command = |kind: SourceKind| {
        let mut args = vec![
            "migrate".to_owned(),
            "--from".to_owned(),
            kind.name().to_owned(),
        ];
        if plan.dry_run {
            args.push("--dry-run".into());
        }
        if plan.force {
            args.push("--force".into());
        }
        if let Some(to) = &plan.to {
            args.push("--to".into());
            args.push(to.display().to_string());
        }
        devlanes_cli::Hint::command(args).0
    };
    let mut failure = Failure::new(
        "sources_conflict",
        exit::USAGE,
        format!(
            "{} define the same names: {}; migrate one of them",
            found
                .iter()
                .map(|d| d.kind.name())
                .collect::<Vec<_>>()
                .join(" and "),
            clashes.join(", ")
        ),
    );
    failure.context = found
        .iter()
        .map(|d| format!("{}  ({})", command(d.kind), d.files.join(", ")))
        .collect();
    failure.hint = found.first().map(|d| command(d.kind));
    failure.data = json!({
        "sources": found,
        "conflicts": units.iter().map(|(name, kinds)| json!({"name": name, "sources": kinds})).collect::<Vec<_>>(),
        "commands": found.iter().map(|d| command(d.kind)).collect::<Vec<_>>(),
    });
    Err(failure)
}

/// A conversion error as a failure.
fn failure(error: MigrateError) -> Box<Failure> {
    let message = error.to_string();
    match error {
        MigrateError::Empty { sources, notes } => {
            let mut failure = Failure::new("nothing_to_migrate", exit::FAILED, message);
            failure.context = notes.iter().map(note_line).collect();
            failure.data = json!({"sources": sources, "notes": notes});
            failure
        }
        MigrateError::Invalid { message, yaml } => {
            let mut failure = Failure::new(
                "invalid_output",
                exit::FAILED,
                format!("the generated devlanes.yaml is invalid ({message}); nothing written"),
            );
            failure
                .context
                .push("this is a devlanes bug; please report it with the source file".into());
            failure.data = json!({"yaml": yaml});
            failure
        }
        _ => Failure::new("migrate_failed", exit::FAILED, message),
    }
}

/// The write error.
fn write_failed(path: &Path, error: &std::io::Error) -> Box<Failure> {
    Failure::new(
        "write_failed",
        exit::FAILED,
        format!("cannot write {}: {error}", path.display()),
    )
}

/// `Procfile.dev [web]: message`.
fn note_line(note: &Note) -> String {
    match &note.unit {
        Some(unit) => format!("{} [{unit}]: {}", note.source, note.message),
        None => format!("{}: {}", note.source, note.message),
    }
}

/// `path` relative to `cwd` when it is inside it.
fn shown(path: &Path, cwd: &Path) -> String {
    path.strip_prefix(cwd)
        .map_or_else(|_| path.display().to_string(), |p| p.display().to_string())
}

/// The result: YAML for `--dry-run`, a summary after writing, the document in JSON mode.
fn report(runtime: &Runtime, plan: &MigratePlan, output: &Path, migration: &Migration) -> Rendered {
    if runtime.mode == OutputMode::Json {
        let data = json!({
            "path": output,
            "written": !plan.dry_run,
            "sources": migration.sources,
            "yaml": migration.yaml,
            "notes": migration.notes,
            "services": migration.services,
            "tasks": migration.tasks,
        });
        return Rendered {
            stdout: format!("{data}\n"),
            ..Rendered::default()
        };
    }
    let notes: String = migration
        .notes
        .iter()
        .map(|note| format!("note: {}\n", note_line(note)))
        .collect();
    if plan.dry_run {
        // The YAML alone on stdout, so it can be piped; notes go to stderr.
        return Rendered {
            stdout: migration.yaml.clone(),
            stderr: if runtime.quiet { String::new() } else { notes },
            ..Rendered::default()
        };
    }
    if runtime.quiet {
        return Rendered::default();
    }
    let files: Vec<String> = migration
        .sources
        .iter()
        .flat_map(|d| d.files.clone())
        .collect();
    let count = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let mut stdout = format!(
        "wrote {} from {} ({}, {})\n",
        shown(output, &plan.cwd),
        files.join(", "),
        count(migration.services.len(), "service"),
        count(migration.tasks.len(), "task"),
    );
    stdout.push_str(&notes);
    stdout.push_str(if migration.notes.is_empty() {
        "next: devlanes start\n"
    } else {
        "next: review the notes (also comments in the file), then devlanes start\n"
    });
    Rendered {
        stdout,
        ..Rendered::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--from` offers exactly the names and aliases the migrate crate knows.
    #[test]
    fn from_values_match_the_migrate_crate() {
        let mut expected = vec!["auto"];
        expected.extend(SourceKind::ALL.iter().map(|kind| kind.name()));
        expected.extend(SourceKind::ALIASES.iter().map(|(alias, _)| *alias));
        assert_eq!(devlanes_cli::MIGRATE_SOURCES.to_vec(), expected);
    }

    /// The Git root is the nearest directory with `.git`, file or directory.
    #[test]
    fn finds_the_git_root() {
        let temp = tempfile::tempdir().expect("tempdir");
        let nested = temp.path().join("a/b");
        std::fs::create_dir_all(&nested).expect("dirs");
        assert_eq!(git_root(&nested), None);
        std::fs::write(temp.path().join(".git"), "gitdir: elsewhere\n").expect("write");
        assert_eq!(git_root(&nested).as_deref(), Some(temp.path()));
    }
}

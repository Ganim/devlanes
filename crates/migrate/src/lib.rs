//! Converts other process-manager setups into a validated `devlanes.yaml`.
//!
//! Pure and synchronous: [`detect`] looks for known files in one directory, [`migrate`]
//! reads them, converts each into services and tasks, merges them and checks the result with
//! `devlanes-config`. Nothing is written; the caller decides where the text goes.
//!
//! ```
//! let dir = tempfile::tempdir()?;
//! std::fs::write(dir.path().join("Procfile"), "web: bin/rails server -p $PORT\n")?;
//! let found = devlanes_migrate::detect(dir.path());
//! let migration = devlanes_migrate::migrate(dir.path(), &found, &Default::default())?;
//! assert!(migration.yaml.contains("run: bin/rails server -p $PORT"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod draft;
mod emit;
mod error;
mod jsobject;
mod ports;
mod sources;
mod words;

use std::path::{Path, PathBuf};

use serde::Serialize;

pub use error::MigrateError;

use draft::{Draft, Unit};

/// A tool whose configuration can be migrated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SourceKind {
    Mprocs,
    Dekit,
    ProcessCompose,
    Procfile,
    Pm2,
    Tmuxinator,
    Tmuxp,
    PackageJson,
    Mise,
}

impl SourceKind {
    /// Every source, in detection and merge order.
    pub const ALL: [Self; 9] = [
        Self::Mprocs,
        Self::Dekit,
        Self::ProcessCompose,
        Self::Procfile,
        Self::Pm2,
        Self::Tmuxinator,
        Self::Tmuxp,
        Self::PackageJson,
        Self::Mise,
    ];

    /// Other names `--from` accepts, with the source they mean.
    pub const ALIASES: [(&'static str, Self); 6] = [
        ("foreman", Self::Procfile),
        ("overmind", Self::Procfile),
        ("hivemind", Self::Procfile),
        ("honcho", Self::Procfile),
        ("concurrently", Self::PackageJson),
        ("npm-run-all", Self::PackageJson),
    ];

    /// The name used by `--from` and in JSON output.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mprocs => "mprocs",
            Self::Dekit => "dekit",
            Self::ProcessCompose => "process-compose",
            Self::Procfile => "procfile",
            Self::Pm2 => "pm2",
            Self::Tmuxinator => "tmuxinator",
            Self::Tmuxp => "tmuxp",
            Self::PackageJson => "package-json",
            Self::Mise => "mise",
        }
    }

    /// A source by name or alias.
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.name() == name)
            .or_else(|| {
                Self::ALIASES
                    .into_iter()
                    .find_map(|(alias, kind)| (alias == name).then_some(kind))
            })
    }

    /// The file names detection looks for, for messages when nothing is found.
    pub fn file_names(self) -> &'static [&'static str] {
        sources::get(self).file_names()
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// A source found in a directory: its kind and the files that will be read, relative to the
/// directory, main file first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Detected {
    #[serde(rename = "source")]
    pub kind: SourceKind,
    pub files: Vec<String>,
}

/// Something to review in the generated file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Note {
    /// The source file the note is about.
    pub source: String,
    /// The service or task it concerns, by its devlanes name.
    pub unit: Option<String>,
    pub message: String,
}

/// Settings that are not in the source files.
#[derive(Debug, Clone)]
pub struct Options {
    /// Operating system for mprocs `$select: os` values, as in `std::env::consts::OS`.
    pub os: String,
    /// Home directory, for `~` in tmux session roots.
    pub home: Option<PathBuf>,
    /// Project name; defaults to one made from the directory name.
    pub project: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            home: None,
            project: None,
        }
    }
}

/// A validated `devlanes.yaml` and what to review in it.
#[derive(Debug, Clone, Serialize)]
pub struct Migration {
    pub project: String,
    pub sources: Vec<Detected>,
    pub yaml: String,
    pub notes: Vec<Note>,
    pub services: Vec<String>,
    pub tasks: Vec<String>,
}

/// Finds every supported setup in `dir`, in [`SourceKind::ALL`] order.
pub fn detect(dir: &Path) -> Vec<Detected> {
    SourceKind::ALL
        .into_iter()
        .filter_map(|kind| detect_kind(dir, kind))
        .collect()
}

/// Finds one kind of setup in `dir`.
pub fn detect_kind(dir: &Path, kind: SourceKind) -> Option<Detected> {
    sources::get(kind)
        .detect(dir)
        .map(|files| Detected { kind, files })
}

/// Converts `found` (from [`detect`]) into one validated configuration. Units from several
/// sources are merged when their names do not clash; identical duplicates are kept once.
pub fn migrate(
    dir: &Path,
    found: &[Detected],
    options: &Options,
) -> Result<Migration, MigrateError> {
    let cx = sources::Context { dir, options };
    let mut drafts = Vec::new();
    for detected in found {
        drafts.push(sources::get(detected.kind).convert(&cx, &detected.files)?);
    }
    let kinds: Vec<SourceKind> = found.iter().map(|d| d.kind).collect();
    let mut merged = merge(&kinds, drafts)?;
    if merged.is_empty() {
        return Err(MigrateError::Empty {
            sources: kinds,
            notes: merged.notes,
        });
    }
    ports::adapt(&mut merged.services, &mut merged.tasks);
    drop_global_port(&mut merged);
    for (_, task) in &mut merged.tasks {
        strip_service_fields(task);
    }
    let project = options
        .project
        .clone()
        .unwrap_or_else(|| draft::project_name(dir));
    let files: Vec<String> = found.iter().flat_map(|d| d.files.clone()).collect();
    let compose = sources::compose_file(dir);
    let yaml = emit::render(&emit::Document {
        project: &project,
        files: &files,
        global_env: &merged.global_env,
        services: &merged.services,
        tasks: &merged.tasks,
        notes: &merged.notes,
        compose: compose.as_deref(),
    });
    check(&yaml)?;
    let mut notes = merged.notes.clone();
    for (name, unit) in merged.services.iter().chain(&merged.tasks) {
        notes.extend(unit.notes.iter().map(|message| Note {
            source: unit.file.clone(),
            unit: Some(name.clone()),
            message: message.clone(),
        }));
    }
    if let Some(compose) = &compose {
        notes.push(Note {
            source: compose.clone(),
            unit: None,
            message: "not migrated; the file ends with a commented infra.compose to run its containers as shared infra".into(),
        });
    }
    Ok(Migration {
        project,
        sources: found.to_vec(),
        yaml,
        notes,
        services: merged.services.iter().map(|(n, _)| n.clone()).collect(),
        tasks: merged.tasks.iter().map(|(n, _)| n.clone()).collect(),
    })
}

/// Parses and validates `yaml` as a project configuration.
pub fn check(yaml: &str) -> Result<(), MigrateError> {
    let invalid = |message: String| MigrateError::Invalid {
        message,
        yaml: yaml.to_owned(),
    };
    let project = devlanes_config::parse_project(yaml, None).map_err(|e| invalid(e.message))?;
    devlanes_config::validate(&project).map_err(|e| invalid(e.message))?;
    Ok(())
}

/// Merges drafts in order. A name defined twice is a conflict unless both definitions are
/// the same unit in the same section.
fn merge(kinds: &[SourceKind], drafts: Vec<Draft>) -> Result<Draft, MigrateError> {
    let mut merged = Draft::new(kinds.first().copied().unwrap_or(SourceKind::Mprocs));
    let mut owners: Vec<(String, SourceKind)> = Vec::new();
    let mut conflicts: Vec<(String, Vec<SourceKind>)> = Vec::new();
    let mut conflict = |name: &str, first: SourceKind, second: SourceKind| match conflicts
        .iter_mut()
        .find(|(n, _)| n == name)
    {
        Some((_, list)) => {
            if !list.contains(&second) {
                list.push(second);
            }
        }
        None => conflicts.push((name.to_owned(), vec![first, second])),
    };
    for draft in drafts {
        merged.notes.extend(draft.notes);
        for (section, units) in [(true, draft.services), (false, draft.tasks)] {
            for (name, unit) in units {
                let alias = name.replace('_', "-");
                let existing = owners
                    .iter()
                    .find(|(n, _)| n.replace('_', "-") == alias)
                    .map(|(_, kind)| *kind);
                match existing {
                    None => {
                        owners.push((name.clone(), draft.kind));
                        target(&mut merged, section).push((name, unit));
                    }
                    Some(first) => {
                        let same = target(&mut merged, section)
                            .iter_mut()
                            .find(|(n, _)| *n == name)
                            .filter(|(_, kept)| kept.same_settings(&unit));
                        match same {
                            Some((_, kept)) => kept.note(format!(
                                "{} defines the same {}; kept once",
                                unit.file,
                                if section { "service" } else { "task" }
                            )),
                            None => conflict(&name, first, draft.kind),
                        }
                    }
                }
            }
        }
        for (key, value) in draft.global_env {
            let path = format!("global_env.{key}");
            match merged.global_env.iter().find(|(k, _)| *k == key) {
                Some((_, kept)) if *kept == value => {}
                Some(_) => {
                    let first = owners
                        .iter()
                        .find(|(n, _)| *n == path)
                        .map_or(draft.kind, |(_, kind)| *kind);
                    conflict(&path, first, draft.kind);
                }
                None => {
                    owners.push((path, draft.kind));
                    merged.global_env.push((key, value));
                }
            }
        }
    }
    if conflicts.is_empty() {
        Ok(merged)
    } else {
        Err(MigrateError::Conflict {
            sources: kinds.to_vec(),
            units: conflicts,
        })
    }
}

/// The services or tasks of a draft.
fn target(draft: &mut Draft, services: bool) -> &mut Vec<(String, Unit)> {
    if services {
        &mut draft.services
    } else {
        &mut draft.tasks
    }
}

/// Tasks run once and have no stop policy, readiness, replicas or tags in devlanes.
fn strip_service_fields(task: &mut Unit) {
    if task.stop_signal.take().is_some() | task.stop_timeout.take().is_some() {
        task.note("stop is not migrated: a task runs until it exits");
    }
    if task.ready.take().is_some() {
        task.note("ready is not migrated: a task has no readiness check");
    }
    if task.replicas.take().is_some() {
        task.note("replicas are not migrated for a task");
    }
    if !std::mem::take(&mut task.tags).is_empty() {
        task.note("tags are not migrated for a task");
    }
    task.autostart = None;
}

/// A shared `PORT` would give every service the same port; devlanes sets it per service.
fn drop_global_port(draft: &mut Draft) {
    if let Some(index) = draft.global_env.iter().position(|(k, _)| k == "PORT") {
        let (_, value) = draft.global_env.remove(index);
        let file = draft
            .services
            .first()
            .map(|(_, unit)| unit.file.clone())
            .unwrap_or_default();
        draft.note(
            &file,
            format!("dropped the shared PORT={value:?}: devlanes gives each service its own PORT"),
        );
    }
}

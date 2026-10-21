//! One module per tool. Each detects its files, parses them into typed models with serde and
//! converts them into a [`Draft`].

mod dekit;
mod mise;
mod mprocs;
mod package_json;
mod pm2;
mod process_compose;
mod procfile;
mod tmuxinator;
mod tmuxp;

use std::path::Path;

use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::{
    MigrateError, Options, SourceKind,
    draft::{self, Check, Command, Draft, HttpProbe, Unit},
};

/// A tool whose setup can be migrated.
pub(crate) trait Source: Sync {
    /// The documented file names, in lookup order.
    fn file_names(&self) -> &'static [&'static str];
    /// The files to read when the tool's setup is in `dir`, main file first.
    fn detect(&self, dir: &Path) -> Option<Vec<String>>;
    /// Reads and converts the files [`Source::detect`] returned.
    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError>;
}

/// The implementation for `kind`.
pub(crate) fn get(kind: SourceKind) -> &'static dyn Source {
    match kind {
        SourceKind::Mprocs => &mprocs::Mprocs,
        SourceKind::Dekit => &dekit::Dekit,
        SourceKind::ProcessCompose => &process_compose::ProcessCompose,
        SourceKind::Procfile => &procfile::Procfile,
        SourceKind::Pm2 => &pm2::Pm2,
        SourceKind::Tmuxinator => &tmuxinator::Tmuxinator,
        SourceKind::Tmuxp => &tmuxp::Tmuxp,
        SourceKind::PackageJson => &package_json::PackageJson,
        SourceKind::Mise => &mise::Mise,
    }
}

/// The directory being migrated and the caller's options.
pub(crate) struct Context<'a> {
    pub dir: &'a Path,
    pub options: &'a Options,
}

impl Context<'_> {
    /// Reads a file relative to the directory.
    pub fn read(&self, file: &str) -> Result<String, MigrateError> {
        let path = self.dir.join(file);
        std::fs::read_to_string(&path).map_err(|source| MigrateError::Read { path, source })
    }

    /// Reads a YAML or JSON file into a typed model.
    pub fn yaml<T: DeserializeOwned>(&self, file: &str) -> Result<T, MigrateError> {
        let value = self.yaml_value(file)?;
        typed(file, value)
    }

    /// Reads a YAML or JSON file as a plain value.
    pub fn yaml_value(&self, file: &str) -> Result<Value, MigrateError> {
        let text = self.read(file)?;
        parse_yaml(file, &text)
    }

    /// A working directory for a unit, relative to the project, or a note on the unit when the
    /// source's directory cannot be one.
    pub fn cwd(&self, unit: &mut Unit, base: &str, raw: &str) -> Option<String> {
        match draft::relative_dir(self.dir, base, raw, self.options.home.as_deref()) {
            Ok(dir) => dir,
            Err(why) => {
                unit.note(format!("working directory not migrated: {why}"));
                None
            }
        }
    }
}

/// YAML (and so JSON) text into a value.
pub(crate) fn parse_yaml(file: &str, text: &str) -> Result<Value, MigrateError> {
    if text.trim().is_empty() {
        return Ok(Value::Null);
    }
    serde_saphyr::from_str::<Value>(text).map_err(|error| MigrateError::Parse {
        file: file.to_owned(),
        message: error.to_string(),
    })
}

/// A value into a typed model, naming the file on failure.
pub(crate) fn typed<T: DeserializeOwned>(file: &str, value: Value) -> Result<T, MigrateError> {
    serde_json::from_value(value).map_err(|error| MigrateError::Parse {
        file: file.to_owned(),
        message: error.to_string(),
    })
}

/// The files among `names` that exist in `dir`.
pub(crate) fn existing(dir: &Path, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|name| dir.join(name).is_file())
        .map(|name| (*name).to_owned())
        .collect()
}

/// A note for each field the converter does not know, under `path`.
pub(crate) fn unknown_fields(
    notes: &mut Vec<String>,
    path: &str,
    fields: &IndexMap<String, Value>,
) {
    for key in fields.keys() {
        let at = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        notes.push(format!("{at} is not migrated"));
    }
}

/// Adds notes for unknown fields to the draft.
pub(crate) fn unknown_top(draft: &mut Draft, file: &str, fields: &IndexMap<String, Value>) {
    let mut notes = Vec::new();
    unknown_fields(&mut notes, "", fields);
    for note in notes {
        draft.note(file, note);
    }
}

/// An environment map from a source: scalars become strings, `null` (unset) becomes a note.
pub(crate) fn env_entries(unit: &mut Unit, env: &IndexMap<String, Value>) {
    for (key, value) in env {
        match draft::scalar_text(value) {
            Some(text) => unit.set_env(key, draft::escape(&text)),
            None if value.is_null() => unit.note(format!(
                "env {key} is unset in the source; devlanes cannot remove an inherited variable"
            )),
            None => unit.note(format!("env {key} is not a string and was skipped")),
        }
    }
}

/// A string or a list of strings.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
pub(crate) enum OneOrMany {
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    /// The items in order.
    pub fn items(&self) -> Vec<String> {
        match self {
            Self::One(one) => vec![one.clone()],
            Self::Many(many) => many.clone(),
        }
    }
}

/// An HTTP readiness URL: a probe of a local port, settled once ports are known, or a `curl`
/// command for any other URL.
pub(crate) fn http_check(unit: &mut Unit, url: &str, status: Option<u16>) -> Option<Check> {
    match HttpProbe::parse(url, status) {
        Some((port, http)) => Some(Check::Probe {
            port,
            http: Some(http),
        }),
        None => {
            unit.note(format!(
                "ready http {url} is not a local http:// URL; it became a curl command"
            ));
            Some(Check::Cmd(Command::Shell(format!(
                "curl -fsS -o /dev/null {}",
                crate::words::quote(url)
            ))))
        }
    }
}

/// For tools that load `.env` into every process: devlanes reads `.env` only for
/// `${{ env.NAME }}` expressions. Names the variables, never their values. `tool` is the
/// subject and verb, such as `process-compose loads`.
pub(crate) fn dotenv_note(dir: &Path, tool: &str) -> Option<String> {
    let text = std::fs::read_to_string(dir.join(".env")).ok()?;
    let names: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("export ").unwrap_or(line).split_once('='))
        .map(|(key, _)| key.trim())
        .filter(|key| !key.is_empty())
        .collect();
    Some(format!(
        "{tool} .env into every process; devlanes reads it only for ${{{{ env.NAME }}}} expressions, so add what processes need to global_env as NAME: ${{{{ env.NAME }}}}, or `dotenv` to .envrc (.env defines {})",
        if names.is_empty() {
            "nothing".to_owned()
        } else {
            names.join(", ")
        }
    ))
}

/// Docker Compose file names, in Compose's lookup order.
const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// A Docker Compose file to suggest as `infra.compose`: one with `services` and without
/// process-compose's `processes`.
pub(crate) fn compose_file(dir: &Path) -> Option<String> {
    COMPOSE_FILES.into_iter().find_map(|name| {
        let text = std::fs::read_to_string(dir.join(name)).ok()?;
        let value = parse_yaml(name, &text).ok()?;
        (value.get("services").is_some() && value.get("processes").is_none())
            .then(|| name.to_owned())
    })
}

/// Marks services that autostarted services depend on as autostarted too, since devlanes
/// has no dependencies that would start them. `deps` maps a unit to the units it needs.
pub(crate) fn autostart_closure(
    started: &mut std::collections::BTreeSet<String>,
    deps: &IndexMap<String, Vec<String>>,
) {
    let mut queue: Vec<String> = started.iter().cloned().collect();
    while let Some(name) = queue.pop() {
        for dep in deps.get(&name).into_iter().flatten() {
            if started.insert(dep.clone()) {
                queue.push(dep.clone());
            }
        }
    }
}

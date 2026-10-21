//! Procfile runners (foreman, overmind, hivemind, honcho): `Procfile.dev` or `Procfile`, and
//! the options overmind, foreman and honcho read from `.overmind.env`, `.foreman` and `.env`.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::LazyLock,
};

use regex::Regex;

use super::{Context, Source};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit, When, escape},
};

/// The Procfile source.
pub(crate) struct Procfile;

/// Procfile names, the development one first.
const FILES: [&str; 2] = ["Procfile.dev", "Procfile"];

/// Option files that name the Procfile or change how processes run.
const OPTION_FILES: [&str; 3] = [".overmind.env", ".foreman", ".env"];

/// `name: command`, the format foreman defined and Heroku documents.
static LINE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_-]+):\s*(.*)$").expect("valid regex"));

/// overmind's variable for another process's port.
static OVERMIND_PORT_REF: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{?OVERMIND_PROCESS_([A-Za-z0-9_]+)_PORT\}?").expect("valid regex")
});

impl Source for Procfile {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        let options = Options::read(dir);
        let procfile = options
            .procfile
            .clone()
            .filter(|name| dir.join(name).is_file())
            .or_else(|| {
                FILES
                    .into_iter()
                    .find(|name| dir.join(name).is_file())
                    .map(str::to_owned)
            })?;
        let mut files = vec![procfile];
        files.extend(
            OPTION_FILES
                .into_iter()
                .filter(|name| *name != ".env" && dir.join(name).is_file())
                .map(str::to_owned),
        );
        Some(files)
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::Procfile);
        let Some(file) = files.first() else {
            return Ok(draft);
        };
        let options = Options::read(cx.dir);
        if file == FILES[0] && cx.dir.join(FILES[1]).is_file() {
            draft.note(
                FILES[1],
                "skipped: Procfile.dev is the development one; Procfile is usually for production",
            );
        }
        if let Some(note) = super::dotenv_note(cx.dir, "foreman, honcho and overmind load") {
            draft.note(file, note);
        }
        for (key, value, why) in &options.ignored {
            draft.note(
                &options.origin(key),
                format!("{key}={value} is not migrated: {why}"),
            );
        }
        let text = cx.read(file)?;
        let mut entries: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match LINE.captures(line) {
                Some(captures) => entries.push((captures[1].to_owned(), captures[2].to_owned())),
                None => draft.note(file, format!("line {line:?} is not a process; skipped")),
            }
        }
        let mut names = Names::default();
        let claimed: BTreeMap<String, (String, Option<String>)> = entries
            .iter()
            .map(|(raw, _)| (raw.clone(), names.claim(raw)))
            .collect();
        for (raw, command) in &entries {
            let (name, renamed) = claimed[raw].clone();
            let escaped = escape(command);
            let mut linked = Vec::new();
            let run = OVERMIND_PORT_REF
                .replace_all(&escaped, |captures: &regex::Captures<'_>| {
                    match claimed.get(&captures[1]) {
                        Some((target, _)) => {
                            linked.push(target.clone());
                            format!("${{{{ svc.{}.port }}}}", target.replace('-', "_"))
                        }
                        None => captures[0].to_owned(),
                    }
                })
                .into_owned();
            let mut unit = Unit::new(file, Command::Shell(run));
            if let Some(renamed) = renamed {
                unit.note(renamed);
            }
            if !linked.is_empty() {
                unit.note(format!(
                    "OVERMIND_PROCESS_*_PORT now reads the lane port of {}",
                    linked.join(", ")
                ));
            }
            if options.can_die.contains(raw) {
                unit.when = Some(When::Start);
                unit.note("OVERMIND_CAN_DIE: it runs once on every lane start");
                draft.tasks.push((name, unit));
                continue;
            }
            if raw == "release" {
                unit.note("Heroku runs release when deploying; it became an on-demand task");
                draft.tasks.push((name, unit));
                continue;
            }
            match options.formation(raw) {
                Some(0) => unit.autostart = Some(false),
                Some(count) if count > 1 => unit.replicas = Some(count),
                _ => {}
            }
            if !options.enabled(raw) {
                unit.autostart = Some(false);
            }
            draft.services.push((name, unit));
        }
        Ok(draft)
    }
}

/// The runner options that change what the Procfile means.
#[derive(Debug, Default)]
struct Options {
    /// From `OVERMIND_PROCFILE`, `.foreman`'s `procfile:` or honcho's `PROCFILE`.
    procfile: Option<String>,
    /// `name=count`; `all` applies to every process.
    formation: BTreeMap<String, u32>,
    /// `OVERMIND_PROCESSES`: only these start.
    only: Option<BTreeSet<String>>,
    /// `OVERMIND_IGNORED_PROCESSES`.
    ignored_processes: BTreeSet<String>,
    /// `OVERMIND_CAN_DIE`: processes expected to exit.
    can_die: BTreeSet<String>,
    /// Settings with no devlanes equivalent, with their values and why.
    ignored: Vec<(String, String, &'static str)>,
    /// Which file each key came from.
    origins: BTreeMap<String, String>,
}

impl Options {
    /// Reads `.overmind.env`, `.foreman` and the runner keys of `.env`. Values of other
    /// `.env` keys are never read into the result: they may be secrets.
    fn read(dir: &Path) -> Self {
        let mut options = Self::default();
        for file in [".env", ".foreman", ".overmind.env"] {
            let Ok(text) = std::fs::read_to_string(dir.join(file)) else {
                continue;
            };
            for line in text.lines() {
                let line = line.trim();
                let line = line.strip_prefix("export ").unwrap_or(line);
                // `.foreman` is YAML (`procfile: Procfile.dev`); the others are dotenv files.
                let separator = if file == ".foreman" { ':' } else { '=' };
                let Some((key, value)) = line.split_once(separator) else {
                    continue;
                };
                let key = key.trim();
                let value = value.trim().trim_matches(['"', '\'']).to_owned();
                if file == ".env" && !key.starts_with("OVERMIND_") && key != "PROCFILE" {
                    continue;
                }
                options.set(file, key, value);
            }
        }
        options
    }

    /// Applies one setting; later files win.
    fn set(&mut self, file: &str, key: &str, value: String) {
        let list = |value: &str| -> BTreeSet<String> {
            value
                .split(',')
                .map(|item| item.trim().to_owned())
                .filter(|item| !item.is_empty())
                .collect()
        };
        self.origins.insert(key.to_owned(), file.to_owned());
        match key {
            "OVERMIND_PROCFILE" | "PROCFILE" | "procfile" => self.procfile = Some(value),
            "OVERMIND_FORMATION" | "formation" => {
                for item in value.split(',') {
                    if let Some((name, count)) = item.split_once('=')
                        && let Ok(count) = count.trim().parse()
                    {
                        self.formation.insert(name.trim().to_owned(), count);
                    }
                }
            }
            "OVERMIND_PROCESSES" => self.only = Some(list(&value)),
            "OVERMIND_IGNORED_PROCESSES" => self.ignored_processes = list(&value),
            "OVERMIND_CAN_DIE" => self.can_die = list(&value),
            "OVERMIND_AUTO_RESTART" => self.ignored.push((
                key.to_owned(),
                value,
                "devlanes does not restart processes that exit",
            )),
            "OVERMIND_ANY_CAN_DIE" => self.ignored.push((
                key.to_owned(),
                value,
                "devlanes keeps other services running when one exits",
            )),
            "OVERMIND_PORT" | "OVERMIND_PORT_STEP" | "OVERMIND_NO_PORT" | "port" => {
                self.ignored.push((
                    key.to_owned(),
                    value,
                    "devlanes gives each service a port per lane",
                ))
            }
            _ => {}
        }
    }

    /// The file a key was read from.
    fn origin(&self, key: &str) -> String {
        self.origins.get(key).cloned().unwrap_or_default()
    }

    /// The instance count for a process.
    fn formation(&self, name: &str) -> Option<u32> {
        self.formation
            .get(name)
            .or_else(|| self.formation.get("all"))
            .copied()
    }

    /// Whether the runner starts this process.
    fn enabled(&self, name: &str) -> bool {
        !self.ignored_processes.contains(name)
            && self.only.as_ref().is_none_or(|only| only.contains(name))
    }
}

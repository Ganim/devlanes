//! process-compose: the first of `compose.yml`, `compose.yaml`, `process-compose.yml`,
//! `process-compose.yaml` that has `processes`, plus the matching override file.

use std::{collections::BTreeSet, path::Path, sync::LazyLock};

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::{
    Context, OneOrMany, Source, http_check, parse_yaml, typed, unknown_fields, unknown_top,
};
use crate::{
    MigrateError, SourceKind,
    draft::{Check, Command, Draft, Names, Ready, Unit, When, escape, scalar_text, signal_number},
};

/// The process-compose source.
pub(crate) struct ProcessCompose;

/// process-compose's discovery order; `compose.y*ml` counts only with `processes`.
const FILES: [&str; 4] = [
    "compose.yml",
    "compose.yaml",
    "process-compose.yml",
    "process-compose.yaml",
];

/// Override files, merged over the main file in this order of preference.
const OVERRIDES: [&str; 4] = [
    "compose.override.yml",
    "compose.override.yaml",
    "process-compose.override.yml",
    "process-compose.override.yaml",
];

/// Top-level settings that only affect process-compose itself.
const SILENT_KEYS: [&str; 2] = ["version", "is_strict"];

/// `$NAME` or `${NAME}` in an environment value.
static ENV_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)").expect("valid regex")
});

/// `${NAME...}` with an envsubst function such as `${NAME^^}` or `${NAME:-default}`.
static COMPLEX_REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$\{[A-Za-z_][A-Za-z0-9_]*[^A-Za-z0-9_}][^}]*\}").expect("valid regex")
});

/// A whole file.
#[derive(Debug, Deserialize)]
struct File {
    #[serde(default)]
    processes: IndexMap<String, Process>,
    environment: Option<Environment>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `environment:` as `KEY=value` lines or a mapping.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum Environment {
    Lines(Vec<String>),
    Map(IndexMap<String, Value>),
}

/// One process.
#[derive(Debug, Clone, Deserialize)]
struct Process {
    command: Option<String>,
    entrypoint: Option<Vec<String>>,
    working_dir: Option<String>,
    environment: Option<Environment>,
    env_file: Option<String>,
    availability: Option<Availability>,
    #[serde(default)]
    depends_on: IndexMap<String, DependsOn>,
    readiness_probe: Option<Probe>,
    liveness_probe: Option<Value>,
    ready_log_line: Option<String>,
    shutdown: Option<Shutdown>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    is_daemon: bool,
    #[serde(default)]
    is_foreground: bool,
    replicas: Option<u32>,
    namespace: Option<OneOrMany>,
    description: Option<String>,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `depends_on.<name>`.
#[derive(Debug, Clone, Deserialize)]
struct DependsOn {
    condition: Option<String>,
}

/// `availability:`.
#[derive(Debug, Clone, Deserialize)]
struct Availability {
    restart: Option<String>,
    #[serde(default)]
    exit_on_end: bool,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

/// `readiness_probe:`.
#[derive(Debug, Clone, Deserialize)]
struct Probe {
    exec: Option<ProbeExec>,
    http_get: Option<HttpGet>,
    #[serde(flatten)]
    timing: IndexMap<String, Value>,
}

/// `exec:` probe.
#[derive(Debug, Clone, Deserialize)]
struct ProbeExec {
    command: String,
    working_dir: Option<String>,
}

/// `http_get:` probe.
#[derive(Debug, Clone, Deserialize)]
struct HttpGet {
    host: Option<String>,
    scheme: Option<String>,
    path: Option<String>,
    port: Option<Value>,
    status_code: Option<u16>,
    headers: Option<Value>,
}

/// `shutdown:`.
#[derive(Debug, Clone, Deserialize)]
struct Shutdown {
    command: Option<String>,
    signal: Option<i64>,
    timeout_seconds: Option<u64>,
    #[serde(default)]
    parent_only: bool,
    send_keys: Option<String>,
}

impl Source for ProcessCompose {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        let main = FILES.into_iter().find(|name| has_processes(dir, name))?;
        let mut files = vec![main.to_owned()];
        if let Some(over) = OVERRIDES.into_iter().find(|name| has_processes(dir, name)) {
            files.push(over.to_owned());
        }
        Some(files)
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::ProcessCompose);
        let Some(main) = files.first() else {
            return Ok(draft);
        };
        let mut value = cx.yaml_value(main)?;
        for over in &files[1..] {
            merge(&mut value, cx.yaml_value(over)?);
            draft.note(over, format!("merged over {main}"));
        }
        let parsed: File = typed(main, value)?;
        let mut top = parsed.other.clone();
        for key in SILENT_KEYS {
            top.shift_remove(key);
        }
        unknown_top(&mut draft, main, &top);
        if let Some(note) = super::dotenv_note(cx.dir, "process-compose loads") {
            draft.note(main, note);
        }
        if let Some(environment) = &parsed.environment {
            let mut holder = Unit::new(main, Command::Shell(String::new()));
            for (key, value) in env_pairs(&mut holder, environment) {
                draft.global_env.push((key, value));
            }
            for note in holder.notes {
                draft.note(main, note);
            }
        }
        convert_processes(cx, &mut draft, main, &parsed.processes);
        Ok(draft)
    }
}

/// Whether `name` exists and has a `processes` mapping.
fn has_processes(dir: &Path, name: &str) -> bool {
    std::fs::read_to_string(dir.join(name))
        .ok()
        .and_then(|text| parse_yaml(name, &text).ok())
        .is_some_and(|value| value.get("processes").is_some_and(Value::is_object))
}

/// Merges an override: mappings merge by key, everything else replaces.
fn merge(base: &mut Value, over: Value) {
    match (base, over) {
        (Value::Object(base), Value::Object(over)) => {
            for (key, value) in over {
                match base.get_mut(&key) {
                    Some(existing) => merge(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, over) => *base = over,
    }
}

/// Converts the processes: a process others wait to complete is a task that runs on every
/// lane start; a foreground process is an interactive task; the rest are services.
fn convert_processes(
    cx: &Context<'_>,
    draft: &mut Draft,
    file: &str,
    processes: &IndexMap<String, Process>,
) {
    let completed: BTreeSet<&str> = processes
        .values()
        .flat_map(|p| p.depends_on.iter())
        .filter(|(_, dep)| {
            matches!(
                dep.condition.as_deref(),
                Some("process_completed" | "process_completed_successfully")
            )
        })
        .map(|(name, _)| name.as_str())
        .collect();
    let is_task = |name: &str, process: &Process| {
        completed.contains(name)
            || process.is_foreground
            || process.availability.as_ref().is_some_and(|a| a.exit_on_end)
    };
    let mut names = Names::default();
    let claimed: IndexMap<&str, (String, Option<String>)> = processes
        .keys()
        .map(|raw| (raw.as_str(), names.claim(raw)))
        .collect();
    for (raw, process) in processes {
        let (name, renamed) = claimed[raw.as_str()].clone();
        let Some(run) = command(process) else {
            draft.note(file, format!("process {raw} has no command; not migrated"));
            continue;
        };
        let mut unit = Unit::new(file, run);
        if let Some(renamed) = renamed {
            unit.note(renamed);
        }
        unit.description = process.description.clone();
        if let Some(dir) = &process.working_dir {
            unit.cwd = cx.cwd(&mut unit, "", dir);
        }
        if let Some(environment) = &process.environment {
            for (key, value) in env_pairs(&mut unit, environment) {
                unit.set_env(&key, value);
            }
        }
        if let Some(env_file) = &process.env_file {
            unit.note(format!(
                "env_file {env_file} is not migrated: devlanes does not read env files"
            ));
        }
        if let Some(namespace) = &process.namespace {
            unit.tags = namespace
                .items()
                .into_iter()
                .filter(|ns| ns != "default")
                .collect();
        }
        if let Some(availability) = &process.availability {
            if let Some(restart) = availability.restart.as_deref().filter(|r| *r != "no") {
                unit.note(format!(
                    "availability.restart: {restart} is not migrated: devlanes does not restart processes that exit"
                ));
            }
            // Backoff and restart counts only mean something with a restart policy.
            let mut other = availability.other.clone();
            for key in ["backoff_seconds", "max_restarts"] {
                other.shift_remove(key);
            }
            unknown_fields(&mut unit.notes, "availability", &other);
        }
        if let Some(shutdown) = &process.shutdown {
            convert_shutdown(&mut unit, shutdown);
        }
        if process.is_daemon {
            unit.note("is_daemon is not migrated: devlanes runs the command in the foreground and stops it with a signal");
        }
        if process.liveness_probe.is_some() {
            unit.note(
                "liveness_probe is not migrated: devlanes checks readiness only while starting",
            );
        }
        if command_text(process).is_some_and(|text| text.contains("{{")) {
            unit.note(
                "the command uses process-compose template variables ({{...}}); fill them in",
            );
        }
        unknown_fields(&mut unit.notes, &format!("processes.{raw}"), &process.other);
        let dep_names = |filter: &dyn Fn(&str) -> bool| -> Vec<String> {
            process
                .depends_on
                .keys()
                .filter(|dep| filter(dep))
                .map(|dep| {
                    claimed
                        .get(dep.as_str())
                        .map_or_else(|| dep.clone(), |(name, _)| name.clone())
                })
                .collect()
        };
        let service_deps =
            dep_names(&|dep| processes.get(dep).is_some_and(|other| !is_task(dep, other)));
        if is_task(raw, process) {
            if process.is_foreground {
                unit.interactive = true;
                unit.note("is_foreground became an interactive task: run it with devlanes run");
            } else if process.availability.as_ref().is_some_and(|a| a.exit_on_end) {
                unit.note("exit_on_end became an on-demand task: run it with devlanes run, or devlanes with");
            } else if !process.disabled {
                unit.when = Some(When::Start);
                if !service_deps.is_empty() {
                    unit.after_services = true;
                    unit.note(format!(
                        "needs {}; it runs after the lane's services are ready",
                        service_deps.join(", ")
                    ));
                    let waiting: Vec<&str> = processes
                        .iter()
                        .filter(|(other, p)| !is_task(other, p) && p.depends_on.contains_key(raw))
                        .map(|(other, _)| claimed[other.as_str()].0.as_str())
                        .collect();
                    if !waiting.is_empty() {
                        unit.note(super::dekit::order_note(&waiting, &service_deps));
                    }
                }
            }
            if process.readiness_probe.is_some() || process.ready_log_line.is_some() {
                unit.note("readiness is not migrated: a task has no readiness check");
            }
            if let Some(replicas) = process.replicas.filter(|r| *r > 1) {
                unit.note(format!("replicas: {replicas} is not migrated for a task"));
            }
            draft.tasks.push((name, unit));
        } else {
            if process.disabled {
                unit.autostart = Some(false);
            }
            if let Some(replicas) = process.replicas.filter(|r| *r > 1) {
                unit.replicas = Some(replicas);
                unit.note("replicas: PC_REPLICA_NUM counted from 0; devlanes has ${{ replica.index }} from 1");
            }
            if !service_deps.is_empty() {
                unit.note(format!(
                    "depends_on ({}) is not migrated: devlanes starts services together",
                    service_deps.join(", ")
                ));
            }
            unit.ready = readiness(&mut unit, process);
            draft.services.push((name, unit));
        }
    }
}

/// The text of the command, for checks.
fn command_text(process: &Process) -> Option<String> {
    match (&process.entrypoint, &process.command) {
        (Some(entry), Some(command)) => Some(format!("{} {command}", entry.join(" "))),
        (Some(entry), None) => Some(entry.join(" ")),
        (None, Some(command)) => Some(command.clone()),
        (None, None) => None,
    }
}

/// `command` is shell text (process-compose runs it with bash); `entrypoint` is an argv that
/// takes `command` as its last argument. `$$` is process-compose's escape for `$`.
fn command(process: &Process) -> Option<Command> {
    let unescape = |text: &str| text.replace("$$", "$");
    match (&process.entrypoint, &process.command) {
        (Some(entry), command) if !entry.is_empty() => {
            let mut args: Vec<String> = entry.iter().map(|arg| unescape(arg)).collect();
            if let Some(command) = command {
                args.push(unescape(command));
            }
            Some(Command::argv(&args))
        }
        (_, Some(command)) if !command.trim().is_empty() => {
            Some(Command::shell(&unescape(command)))
        }
        _ => None,
    }
}

/// `KEY=value` pairs, with `$NAME` and `${NAME}` turned into `${{ env.NAME }}`.
fn env_pairs(unit: &mut Unit, environment: &Environment) -> Vec<(String, String)> {
    let pairs: Vec<(String, Option<String>)> = match environment {
        Environment::Lines(lines) => lines
            .iter()
            .map(|line| match line.split_once('=') {
                Some((key, value)) => (key.to_owned(), Some(value.to_owned())),
                None => (line.clone(), None),
            })
            .collect(),
        Environment::Map(map) => map
            .iter()
            .map(|(key, value)| (key.clone(), scalar_text(value)))
            .collect(),
    };
    let mut out = Vec::new();
    for (key, value) in pairs {
        let Some(value) = value else {
            unit.note(format!(
                "environment entry {key} has no value; not migrated"
            ));
            continue;
        };
        let escaped = escape(&value);
        let replaced = ENV_REFERENCE.replace_all(&escaped, |captures: &regex::Captures<'_>| {
            let name = captures
                .get(1)
                .or_else(|| captures.get(2))
                .map_or("", |m| m.as_str());
            format!("${{{{ env.{name} }}}}")
        });
        if COMPLEX_REFERENCE.is_match(&value) {
            unit.note(format!(
                "environment {key} uses an expansion function devlanes does not have; fill it in"
            ));
        }
        if replaced != escaped {
            unit.note(format!(
                "environment {key}: $VAR became ${{{{ env.VAR }}}}, read from the shell that runs devlanes or from .env"
            ));
        }
        out.push((key, replaced.into_owned()));
    }
    out
}

/// `readiness_probe` or `ready_log_line` in devlanes terms.
fn readiness(unit: &mut Unit, process: &Process) -> Option<Ready> {
    if let Some(line) = &process.ready_log_line {
        // process-compose matches this as a regular expression already.
        return Some(Ready {
            check: Check::Log(escape(line)),
            timeout: None,
        });
    }
    let probe = process.readiness_probe.as_ref()?;
    if !probe.timing.is_empty() {
        let keys: Vec<&str> = probe.timing.keys().map(String::as_str).collect();
        unit.note(format!(
            "readiness_probe timing ({}) is not migrated; devlanes retries until ready.timeout",
            keys.join(", ")
        ));
    }
    let check = if let Some(exec) = &probe.exec {
        if exec.working_dir.is_some() {
            unit.note("readiness_probe.exec.working_dir is not migrated");
        }
        Check::Cmd(Command::shell(&exec.command.replace("$$", "$")))
    } else if let Some(http) = &probe.http_get {
        if http.headers.is_some() {
            unit.note("readiness_probe.http_get.headers are not migrated");
        }
        let scheme = http
            .scheme
            .as_deref()
            .unwrap_or("http")
            .to_ascii_lowercase();
        let host = http.host.as_deref().unwrap_or("127.0.0.1");
        let port = http
            .port
            .as_ref()
            .and_then(scalar_text)
            .unwrap_or_else(|| "80".into());
        let path = http.path.as_deref().unwrap_or("/");
        let url = format!("{scheme}://{host}:{port}{path}");
        http_check(unit, &url, http.status_code)?
    } else {
        return None;
    };
    Some(Ready {
        check,
        timeout: None,
    })
}

/// `shutdown:` in devlanes terms.
fn convert_shutdown(unit: &mut Unit, shutdown: &Shutdown) {
    if let Some(command) = &shutdown.command {
        unit.note(format!(
            "shutdown.command {command:?} is not migrated: devlanes stops a service with a signal"
        ));
    }
    if let Some(keys) = &shutdown.send_keys {
        if keys == r"\x03" || keys == "\u{3}" {
            unit.stop_signal = Some("SIGINT".into());
            unit.note("shutdown sends SIGINT instead of typing Ctrl-C");
        } else {
            unit.note(format!("shutdown.send_keys {keys:?} is not migrated"));
        }
    }
    if let Some(signal) = shutdown.signal {
        match signal_number(signal) {
            Some("SIGTERM") => {}
            Some(name) => unit.stop_signal = Some(name.into()),
            None => unit.note(format!("shutdown.signal {signal} is not migrated")),
        }
    }
    if let Some(seconds) = shutdown.timeout_seconds {
        unit.stop_timeout = Some(format!("{seconds}s"));
    }
    if shutdown.parent_only {
        unit.note("shutdown.parent_only is not migrated: devlanes stops the whole process tree");
    }
}

//! pm2: an `ecosystem` file in JSON, YAML, or JavaScript when it exports a plain literal.

use std::path::Path;

use indexmap::IndexMap;
use serde::Deserialize;
use serde_json::Value;

use super::{Context, OneOrMany, Source, env_entries, existing, parse_yaml, typed, unknown_fields};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit, duration_ms},
    words,
};

/// The pm2 source.
pub(crate) struct Pm2;

/// Ecosystem files; static formats first, since they need no evaluation.
const FILES: [&str; 9] = [
    "ecosystem.config.json",
    "ecosystem.json",
    "ecosystem.config.yaml",
    "ecosystem.config.yml",
    "ecosystem.yaml",
    "ecosystem.yml",
    "ecosystem.config.js",
    "ecosystem.config.cjs",
    "ecosystem.config.mjs",
];

/// Interpreters pm2 picks by extension (pm2's `lib/API/interpreter.json`).
const INTERPRETERS: [(&str, &str); 12] = [
    (".js", "node"),
    (".cjs", "node"),
    (".mjs", "node"),
    (".ts", "bun"),
    (".tsx", "bun"),
    (".sh", "bash"),
    (".py", "python"),
    (".rb", "ruby"),
    (".php", "php"),
    (".pl", "perl"),
    (".coffee", "coffee"),
    (".ls", "lsc"),
];

/// Log settings: devlanes keeps every unit's output itself.
const LOG_KEYS: [&str; 8] = [
    "out_file",
    "error_file",
    "log_file",
    "log_date_format",
    "merge_logs",
    "combine_logs",
    "time",
    "pid_file",
];

/// Restart policies: devlanes does not restart processes.
const RESTART_KEYS: [&str; 5] = [
    "max_memory_restart",
    "cron_restart",
    "max_restarts",
    "min_uptime",
    "restart_delay",
];

/// One app.
#[derive(Debug, Deserialize)]
struct App {
    name: Option<String>,
    script: Option<String>,
    args: Option<OneOrMany>,
    cwd: Option<String>,
    interpreter: Option<String>,
    interpreter_args: Option<OneOrMany>,
    node_args: Option<OneOrMany>,
    #[serde(default)]
    env: IndexMap<String, Value>,
    instances: Option<Value>,
    exec_mode: Option<String>,
    watch: Option<Value>,
    autorestart: Option<bool>,
    kill_timeout: Option<u64>,
    #[serde(default)]
    wait_ready: bool,
    listen_timeout: Option<Value>,
    #[serde(default)]
    shutdown_with_message: bool,
    #[serde(flatten)]
    other: IndexMap<String, Value>,
}

impl Source for Pm2 {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        existing(dir, &FILES)
            .into_iter()
            .next()
            .map(|file| vec![file])
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::Pm2);
        let Some(file) = files.first() else {
            return Ok(draft);
        };
        let text = cx.read(file)?;
        let value = if file.ends_with("js") {
            match crate::jsobject::exported(&text) {
                Ok(value) => value,
                Err(why) => {
                    draft.note(
                        file,
                        format!(
                            "cannot be read without running it ({why}); export it with \
                             node -e \"console.log(JSON.stringify(require('./{file}'), null, 2))\" > ecosystem.json \
                             and run devlanes migrate --from pm2 again"
                        ),
                    );
                    return Ok(draft);
                }
            }
        } else {
            parse_yaml(file, &text)?
        };
        let apps = match value {
            Value::Array(apps) => apps,
            Value::Object(mut map) => {
                let apps = match map.shift_remove("apps") {
                    Some(Value::Array(apps)) => apps,
                    Some(one @ Value::Object(_)) => vec![one],
                    _ => Vec::new(),
                };
                for key in map.keys() {
                    let why = if key == "deploy" {
                        "deploy is for servers; not migrated".to_owned()
                    } else {
                        format!("{key} is not migrated")
                    };
                    draft.note(file, why);
                }
                apps
            }
            _ => Vec::new(),
        };
        let mut names = Names::default();
        for (index, app) in apps.into_iter().enumerate() {
            let app: App = typed(file, app)?;
            let Some(script) = app.script.clone() else {
                draft.note(file, format!("app {index} has no script; not migrated"));
                continue;
            };
            let raw = app.name.clone().unwrap_or_else(|| stem(&script));
            let (name, renamed) = names.claim(&raw);
            let mut unit = Unit::new(file, Command::shell(&command_line(&app, &script)));
            if let Some(renamed) = renamed {
                unit.note(renamed);
            }
            convert_app(cx, &mut unit, &app);
            draft.services.push((name, unit));
        }
        Ok(draft)
    }
}

/// `interpreter [interpreter args] script [args]`, as pm2 builds it.
fn command_line(app: &App, script: &str) -> String {
    let interpreter = match app.interpreter.as_deref() {
        Some("none") => None,
        Some(explicit) => Some(explicit.to_owned()),
        None => INTERPRETERS
            .iter()
            .find(|(ext, _)| script.ends_with(ext))
            .map(|(_, interpreter)| (*interpreter).to_owned()),
    };
    let mut parts = Vec::new();
    if let Some(interpreter) = interpreter {
        parts.push(words::quote(&interpreter));
        if let Some(args) = app.interpreter_args.as_ref().or(app.node_args.as_ref()) {
            parts.push(arguments(args));
        }
    }
    parts.push(words::quote(script));
    if let Some(args) = &app.args {
        parts.push(arguments(args));
    }
    parts.retain(|part| !part.is_empty());
    parts.join(" ")
}

/// A string of arguments is shell text already; a list is quoted word by word.
fn arguments(args: &OneOrMany) -> String {
    match args {
        OneOrMany::One(text) => text.trim().to_owned(),
        OneOrMany::Many(list) => words::join(list),
    }
}

/// The file name without directories or extension, pm2's default app name.
fn stem(script: &str) -> String {
    Path::new(script)
        .file_stem()
        .map_or_else(|| script.to_owned(), |s| s.to_string_lossy().into_owned())
}

/// Everything except the command.
fn convert_app(cx: &Context<'_>, unit: &mut Unit, app: &App) {
    if let Some(cwd) = &app.cwd {
        unit.cwd = cx.cwd(unit, "", cwd);
    }
    env_entries(unit, &app.env);
    match &app.instances {
        Some(Value::Number(count)) => match count.as_i64() {
            Some(count) if count > 1 => unit.replicas = u32::try_from(count).ok(),
            Some(1) | None => {}
            Some(other) => unit.note(format!(
                "instances: {other} (one per CPU) is not migrated; set replicas"
            )),
        },
        Some(Value::String(text)) => {
            unit.note(format!("instances: {text} is not migrated; set replicas"))
        }
        _ => {}
    }
    if app
        .exec_mode
        .as_deref()
        .is_some_and(|mode| mode.starts_with("cluster"))
    {
        unit.note("cluster mode shared one port between instances; devlanes gives each replica its own port");
    }
    if app
        .watch
        .as_ref()
        .is_some_and(|watch| !matches!(watch, Value::Bool(false) | Value::Null))
    {
        unit.note("watch is not migrated: devlanes does not restart on file changes");
    }
    if app.autorestart == Some(true) {
        unit.note("autorestart is not migrated: devlanes does not restart processes that exit");
    }
    // pm2 stops apps with SIGINT, then SIGKILL after kill_timeout (1600 ms by default).
    unit.stop_signal = Some("SIGINT".into());
    if let Some(timeout) = app.kill_timeout {
        unit.stop_timeout = Some(duration_ms(timeout));
    }
    if app.shutdown_with_message {
        unit.note("shutdown_with_message is not migrated: devlanes stops a service with a signal");
    }
    if app.wait_ready || app.listen_timeout.is_some() {
        unit.note("wait_ready/listen_timeout are not migrated; add a ready check");
    }
    let mut other = app.other.clone();
    let env_sets: Vec<String> = other
        .keys()
        .filter(|key| key.starts_with("env_"))
        .cloned()
        .collect();
    for key in &env_sets {
        other.shift_remove(key);
    }
    if !env_sets.is_empty() {
        unit.note(format!(
            "{} (pm2 --env) not migrated; use a devlanes profile with env",
            env_sets.join(", ")
        ));
    }
    let logs: Vec<&str> = LOG_KEYS
        .into_iter()
        .filter(|key| other.shift_remove(*key).is_some())
        .collect();
    if !logs.is_empty() {
        unit.note(format!(
            "{} not migrated: devlanes keeps every service's output",
            logs.join(", ")
        ));
    }
    let restarts: Vec<&str> = RESTART_KEYS
        .into_iter()
        .filter(|key| other.shift_remove(*key).is_some())
        .collect();
    if !restarts.is_empty() {
        unit.note(format!(
            "{} not migrated: devlanes does not restart processes",
            restarts.join(", ")
        ));
    }
    unknown_fields(&mut unit.notes, "apps", &other);
}

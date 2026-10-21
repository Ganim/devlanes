//! package.json scripts that start several dev processes at once with `concurrently`,
//! `npm-run-all --parallel` or `run-p`. Each process becomes a service, by script reference
//! where the command runs another script.

use std::path::Path;

use indexmap::IndexMap;
use regex::Regex;
use serde::Deserialize;
use serde_json::Value;

use super::{Context, Source, typed};
use crate::{
    MigrateError, SourceKind,
    draft::{Command, Draft, Names, Unit, When},
    words,
};

/// The package.json source.
pub(crate) struct PackageJson;

/// Only the root manifest.
const FILES: [&str; 1] = ["package.json"];

/// Scripts checked first for a parallel runner, in this order.
const PREFERRED: [&str; 4] = ["dev", "start", "develop", "serve"];

/// concurrently options that take a value.
const CONCURRENTLY_VALUES: [&str; 22] = [
    "-n",
    "--names",
    "-c",
    "--prefix-colors",
    "-p",
    "--prefix",
    "-l",
    "--prefix-length",
    "-t",
    "--timestamp-format",
    "--name-separator",
    "-s",
    "--success",
    "--restart-tries",
    "--restart-after",
    "-m",
    "--max-processes",
    "-ks",
    "--kill-signal",
    "--default-input-target",
    "--hide",
    "--teardown",
];

/// Programs that are on PATH outside a package's `node_modules/.bin`.
const GLOBAL_PROGRAMS: [&str; 22] = [
    "node", "npm", "npx", "pnpm", "yarn", "bun", "bunx", "deno", "corepack", "python", "python3",
    "go", "cargo", "docker", "make", "just", "sh", "bash", "env", "cd", "echo", "mise",
];

/// The parts of package.json this source reads.
#[derive(Debug, Deserialize)]
struct Manifest {
    #[serde(default)]
    scripts: IndexMap<String, String>,
    #[serde(rename = "packageManager")]
    package_manager: Option<String>,
}

/// A process the runner starts.
struct Process {
    name: String,
    command: Command,
    note: Option<String>,
}

impl Source for PackageJson {
    fn file_names(&self) -> &'static [&'static str] {
        &FILES
    }

    fn detect(&self, dir: &Path) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(dir.join(FILES[0])).ok()?;
        let manifest: Manifest = serde_json::from_str(&text).ok()?;
        parallel_scripts(&manifest).first()?;
        Some(vec![FILES[0].to_owned()])
    }

    fn convert(&self, cx: &Context<'_>, files: &[String]) -> Result<Draft, MigrateError> {
        let mut draft = Draft::new(SourceKind::PackageJson);
        let Some(file) = files.first() else {
            return Ok(draft);
        };
        let value: Value =
            serde_json::from_str(&cx.read(file)?).map_err(|e| MigrateError::Parse {
                file: file.clone(),
                message: e.to_string(),
            })?;
        let manifest: Manifest = typed(file, value)?;
        let candidates = parallel_scripts(&manifest);
        let Some(chosen) = candidates.first().cloned() else {
            return Ok(draft);
        };
        for other in &candidates[1..] {
            draft.note(
                file,
                format!(
                    "script {other} also runs processes in parallel; only {chosen} is migrated"
                ),
            );
        }
        let bin = cx.dir.join("node_modules/.bin");
        let exec = Exec {
            prefix: exec_prefix(cx.dir, manifest.package_manager.as_deref()),
            bin: bin.is_dir().then_some(bin),
        };
        let line = &manifest.scripts[&chosen];
        let Some(argv) = words::split(line) else {
            draft.note(
                file,
                format!("script {chosen} has unbalanced quotes; not migrated"),
            );
            return Ok(draft);
        };
        let argv = until_operator(&argv);
        let runner = argv.first().map(String::as_str).unwrap_or_default();
        let (processes, before) = if runner == "concurrently" {
            (
                concurrently(&manifest, &argv[1..], &exec, &mut draft, file),
                Vec::new(),
            )
        } else {
            npm_run_all(
                &manifest,
                runner,
                &argv[1..],
                &exec.prefix,
                &mut draft,
                file,
            )
        };
        draft.note(file, format!("services come from script {chosen}: {line}"));
        let mut names = Names::default();
        for process in before {
            let (name, _) = names.claim(&process.name);
            let mut unit = Unit::new(file, process.command);
            unit.when = Some(When::Start);
            unit.note(format!(
                "runs before the parallel group of {chosen} on every lane start"
            ));
            if let Some(note) = process.note {
                unit.note(note);
            }
            draft.tasks.push((name, unit));
        }
        for process in processes {
            let (name, renamed) = names.claim(&process.name);
            let mut unit = Unit::new(file, process.command);
            if let Some(renamed) = renamed {
                unit.note(renamed);
            }
            if let Some(note) = process.note {
                unit.note(note);
            }
            draft.services.push((name, unit));
        }
        Ok(draft)
    }
}

/// Scripts whose command starts processes in parallel, preferred names first.
fn parallel_scripts(manifest: &Manifest) -> Vec<String> {
    let mut found: Vec<String> = manifest
        .scripts
        .iter()
        .filter(|(_, line)| is_parallel(line))
        .map(|(name, _)| name.clone())
        .collect();
    found.sort_by_key(|name| {
        PREFERRED
            .iter()
            .position(|p| p == name)
            .unwrap_or(PREFERRED.len())
    });
    found
}

/// Whether a script line runs a parallel runner.
fn is_parallel(line: &str) -> bool {
    let Some(argv) = words::split(line) else {
        return false;
    };
    match argv.first().map(String::as_str) {
        Some("concurrently" | "run-p") => true,
        Some("npm-run-all") => argv.iter().any(|a| a == "-p" || a == "--parallel"),
        _ => false,
    }
}

/// The words before the first `&&`, `||`, `;` or `|`.
fn until_operator(argv: &[String]) -> Vec<String> {
    argv.iter()
        .take_while(|w| !matches!(w.as_str(), "&&" | "||" | ";" | "|" | "&"))
        .cloned()
        .collect()
}

/// How to run a package binary from outside a script, by package manager.
fn exec_prefix(dir: &Path, declared: Option<&str>) -> String {
    let manager = declared
        .and_then(|d| d.split('@').next())
        .map(str::to_owned)
        .or_else(|| {
            [
                ("pnpm-lock.yaml", "pnpm"),
                ("yarn.lock", "yarn"),
                ("bun.lock", "bun"),
                ("bun.lockb", "bun"),
            ]
            .into_iter()
            .find(|(lock, _)| dir.join(lock).is_file())
            .map(|(_, manager)| manager.to_owned())
        })
        .unwrap_or_else(|| "npm".to_owned());
    match manager.as_str() {
        "pnpm" => "pnpm exec",
        "yarn" => "yarn run",
        "bun" => "bun x",
        _ => "npx",
    }
    .to_owned()
}

/// How to run a package binary outside a script.
struct Exec {
    /// `pnpm exec`, `npx`, ...
    prefix: String,
    /// `node_modules/.bin` when installed.
    bin: Option<std::path::PathBuf>,
}

/// The concurrently command list.
fn concurrently(
    manifest: &Manifest,
    args: &[String],
    exec: &Exec,
    draft: &mut Draft,
    file: &str,
) -> Vec<Process> {
    let mut names: Vec<String> = Vec::new();
    let mut separator = ",".to_owned();
    let mut commands = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        let (flag, inline) = match arg.split_once('=') {
            Some((flag, value)) if arg.starts_with('-') => (flag, Some(value.to_owned())),
            _ => (arg.as_str(), None),
        };
        if CONCURRENTLY_VALUES.contains(&flag) {
            let value = match inline {
                Some(value) => value,
                None => {
                    index += 1;
                    args.get(index).cloned().unwrap_or_default()
                }
            };
            match flag {
                "-n" | "--names" => names = vec![value],
                "--name-separator" => separator = value,
                _ => {}
            }
        } else if !arg.starts_with('-') || arg == "-" {
            commands.push(arg.clone());
        }
        index += 1;
    }
    let names: Vec<String> = names
        .first()
        .map(|n| n.split(separator.as_str()).map(str::to_owned).collect())
        .unwrap_or_default();
    let mut processes = Vec::new();
    for (position, command) in commands.iter().enumerate() {
        let given = names.get(position).filter(|n| !n.is_empty()).cloned();
        if let Some((manager, pattern)) = shortcut(command) {
            if manager == "deno" {
                processes.push(Process {
                    name: given.unwrap_or_else(|| pattern.to_owned()),
                    command: Command::shell(&format!("deno task {pattern}")),
                    note: None,
                });
                continue;
            }
            let matched = expand_shortcut(manifest, pattern);
            if matched.is_empty() {
                draft.note(file, format!("{command} matches no script; not migrated"));
            }
            for (script, wildcard) in matched {
                let name = match (&given, wildcard) {
                    (Some(prefix), Some(part)) => format!("{prefix}{part}"),
                    (Some(given), None) => given.clone(),
                    (None, Some(part)) => part,
                    (None, None) => script.clone(),
                };
                processes.push(script_process(name, &script));
            }
            continue;
        }
        processes.push(command_process(manifest, given, command, position, exec));
    }
    processes
}

/// `npm:dev:*` into (`npm`, `dev:*`).
fn shortcut(command: &str) -> Option<(&str, &str)> {
    let (manager, pattern) = command.split_once(':')?;
    matches!(manager, "npm" | "pnpm" | "yarn" | "bun" | "node" | "deno")
        .then_some((manager, pattern))
        .filter(|_| !command.contains(' '))
}

/// Scripts a concurrently shortcut pattern names, with what `*` matched. `(!x)` excludes
/// matches containing `x`.
fn expand_shortcut(manifest: &Manifest, pattern: &str) -> Vec<(String, Option<String>)> {
    let (pattern, exclude) = match pattern.find("(!") {
        Some(at) if pattern.ends_with(')') => {
            (&pattern[..at], Some(&pattern[at + 2..pattern.len() - 1]))
        }
        _ => (pattern, None),
    };
    if !pattern.contains('*') {
        return if manifest.scripts.contains_key(pattern) {
            vec![(pattern.to_owned(), None)]
        } else {
            Vec::new()
        };
    }
    let regex = format!("^{}$", regex::escape(pattern).replace(r"\*", "(.*)"));
    let Ok(regex) = Regex::new(&regex) else {
        return Vec::new();
    };
    manifest
        .scripts
        .keys()
        .filter_map(|script| {
            let captures = regex.captures(script)?;
            let part = captures.get(1).map(|m| m.as_str().to_owned());
            if exclude.is_some_and(|ex| part.as_deref().unwrap_or(script).contains(ex)) {
                return None;
            }
            Some((script.clone(), part))
        })
        .collect()
}

/// A service that runs another script of the same package.json.
fn script_process(name: String, script: &str) -> Process {
    Process {
        name,
        command: Command::Script {
            source: FILES[0].to_owned(),
            name: script.to_owned(),
        },
        note: None,
    }
}

/// A command line: a script reference when it is `<pm> run <script>`, else shell text.
fn command_process(
    manifest: &Manifest,
    given: Option<String>,
    command: &str,
    position: usize,
    exec: &Exec,
) -> Process {
    let argv = words::split(command).unwrap_or_default();
    let script = match argv
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [
            "npm" | "pnpm" | "yarn" | "bun",
            "run" | "run-script",
            script,
        ]
        | ["pnpm" | "yarn", script]
        | ["node", "--run", script] => manifest
            .scripts
            .contains_key(*script)
            .then(|| (*script).to_owned()),
        _ => None,
    };
    if let Some(script) = script {
        return script_process(given.unwrap_or_else(|| script.clone()), &script);
    }
    let program = argv.first().map(String::as_str).unwrap_or_default();
    let name = given.unwrap_or_else(|| {
        let base = program.rsplit('/').next().unwrap_or(program);
        if base.is_empty() {
            format!("process-{}", position + 1)
        } else {
            base.to_owned()
        }
    });
    // Inside a script, node_modules/.bin is on PATH; a devlanes `run` line has no such PATH.
    let local = match &exec.bin {
        _ if GLOBAL_PROGRAMS.contains(&program) || program.contains(['/', '=']) => Some(false),
        Some(bin) => Some(bin.join(program).exists()),
        None => None,
    };
    let exec = exec.prefix.as_str();
    match local {
        Some(false) => Process {
            name,
            command: Command::shell(command),
            note: None,
        },
        Some(true) => Process {
            name,
            command: Command::shell(&format!("{exec} {command}")),
            note: None,
        },
        None => Process {
            name,
            command: Command::shell(&format!("{exec} {command}")),
            note: Some(format!(
                "runs through `{exec}`, assuming {program} is a package binary (node_modules is not installed); drop the prefix if it is a global program"
            )),
        },
    }
}

/// npm-run-all and run-p groups: parallel groups become services, the sequential steps
/// before them become lane-start tasks.
fn npm_run_all(
    manifest: &Manifest,
    runner: &str,
    args: &[String],
    exec: &str,
    draft: &mut Draft,
    file: &str,
) -> (Vec<Process>, Vec<Process>) {
    let mut parallel = runner == "run-p";
    let mut services = Vec::new();
    let mut before = Vec::new();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "-p" | "--parallel" => parallel = true,
            "-s" | "--sequential" | "--serial" => parallel = false,
            "--max-parallel" | "--npm-path" => index += 1,
            _ if arg.starts_with('-') => {}
            pattern => {
                let (pattern, extra) = match pattern.split_once(' ') {
                    Some((p, rest)) => (p, Some(rest.trim())),
                    None => (pattern, None),
                };
                let matched = glob_scripts(manifest, pattern);
                if matched.is_empty() {
                    draft.note(file, format!("{pattern} matches no script; not migrated"));
                }
                for script in matched {
                    let process = match extra {
                        Some(extra) => {
                            let extra = extra.strip_prefix("--").unwrap_or(extra).trim();
                            Process {
                                name: script.clone(),
                                command: Command::shell(&format!(
                                    "{} run {} -- {extra}",
                                    exec_runner(exec),
                                    words::quote(&script)
                                )),
                                note: Some("script arguments kept as a command line".into()),
                            }
                        }
                        None => script_process(script.clone(), &script),
                    };
                    if parallel {
                        services.push(process);
                    } else if services.is_empty() {
                        before.push(process);
                    } else {
                        draft.note(
                            file,
                            format!("{script} runs after the parallel group; not migrated"),
                        );
                    }
                }
            }
        }
        index += 1;
    }
    (services, before)
}

/// The package manager command behind an exec prefix.
fn exec_runner(exec: &str) -> &str {
    match exec {
        "pnpm exec" => "pnpm",
        "yarn run" => "yarn",
        "bun x" => "bun",
        _ => "npm",
    }
}

/// Scripts an npm-run-all pattern names: `*` matches within a `:` segment, `**` across.
fn glob_scripts(manifest: &Manifest, pattern: &str) -> Vec<String> {
    if !pattern.contains('*') {
        return if manifest.scripts.contains_key(pattern) {
            vec![pattern.to_owned()]
        } else {
            Vec::new()
        };
    }
    let regex = format!(
        "^{}$",
        regex::escape(pattern)
            .replace(r"\*\*", ".*")
            .replace(r"\*", "[^:]*")
    );
    let Ok(regex) = Regex::new(&regex) else {
        return Vec::new();
    };
    manifest
        .scripts
        .keys()
        .filter(|script| regex.is_match(script))
        .cloned()
        .collect()
}

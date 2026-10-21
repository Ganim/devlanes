//! Core adapters: the `package.json` script runner and argument placement for
//! presets.

use crate::{NpmError, ScriptRequest, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{self, DraftCommand, LaunchDraft, ScriptPlan, ScriptRunner},
};
use std::path::Path;

/// `run_script` runner for `package.json` files.
#[derive(Debug, Default, Clone, Copy)]
pub struct NpmScripts;

#[async_trait]
impl ScriptRunner for NpmScripts {
    fn id(&self) -> &'static str {
        "npm"
    }

    /// Matches files named `package.json`.
    fn matches(&self, source: &Path) -> bool {
        handles(source)
    }

    /// Runs [`plan`] on a blocking thread: it reads the manifest and walks up
    /// to the worktree looking for lockfiles.
    async fn resolve(&self, request: &contrib::ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let worktree = request.worktree.to_path_buf();
        let source = request.source.to_path_buf();
        let name = request.name.to_owned();
        let extra_args = request.extra_args.to_vec();
        let planned = tokio::task::spawn_blocking(move || {
            plan(&ScriptRequest {
                worktree: &worktree,
                source: &source,
                name: &name,
                extra_args: &extra_args,
            })
        })
        .await
        .map_err(|e| Error::Integration(format!("package.json script resolution failed: {e}")))?
        .map_err(into_core)?;
        Ok(ScriptPlan {
            argv: planned.argv,
            cwd: planned.cwd,
            body: Some(planned.script_body),
        })
    }
}

/// Maps I/O failures to [`Error::Integration`] and config mistakes (missing
/// script, ambiguous lockfiles, bad paths) to [`Error::InvalidInput`].
fn into_core(error: NpmError) -> Error {
    match error {
        NpmError::Read { .. } => Error::Integration(error.to_string()),
        other => Error::InvalidInput(other.to_string()),
    }
}

/// Appends preset arguments to a draft.
///
/// `run_script` drafts get the arguments as-is: the script runner adds the
/// separator its package manager needs. A plain `run` that invokes an npm
/// script (`npm run dev`, `npm start`) gets a `--` first, because npm parses
/// flags after the script name itself unless they follow `--`. Other plain
/// commands get the arguments appended unchanged.
pub fn forward_args(draft: &mut LaunchDraft, args: Vec<String>) {
    if args.is_empty() {
        return;
    }
    if needs_npm_separator(&draft.command, &draft.args) {
        draft.args.push("--".to_owned());
    }
    draft.args.extend(args);
}

/// npm subcommands that run a script and forward arguments after `--`.
const RUN_COMMANDS: &[&str] = &["run", "run-script", "rum", "urn"];
const LIFECYCLE_COMMANDS: &[&str] = &["start", "test", "t", "tst", "restart", "stop"];

/// npm options whose value is the next token, so it is not the subcommand.
const VALUE_FLAGS: &[&str] = &["-w", "--workspace", "--prefix", "-C", "--script-shell"];

/// Returns whether arguments appended to `command` need a `--` first: true for
/// a plain command whose last simple command is an npm script run without a
/// `--` yet (in the command or in `pending` draft args).
pub fn needs_npm_separator(command: &DraftCommand, pending: &[String]) -> bool {
    if pending.iter().any(|arg| arg == "--") {
        return false;
    }
    match command {
        DraftCommand::Shell(text) => {
            // Appended args land on the last line, after its last simple command.
            let last_line = text
                .lines()
                .rev()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default();
            let mut segment = Vec::new();
            for token in last_line.split_whitespace() {
                if matches!(token, "&&" | "||" | ";" | "|" | "&") || token.ends_with(';') {
                    segment.clear();
                } else {
                    segment.push(token);
                }
            }
            is_npm_script_run(&segment)
        }
        DraftCommand::Argv(argv) => {
            let tokens: Vec<&str> = argv.iter().map(String::as_str).collect();
            is_npm_script_run(&tokens)
        }
        DraftCommand::Script { .. } => false,
    }
}

/// Recognises `[VAR=x ...] npm [flags] (run <name> | start | test ...)` with
/// no `--` after the subcommand.
fn is_npm_script_run(tokens: &[&str]) -> bool {
    let unquote = |token: &str| token.trim_matches(|c| c == '"' || c == '\'').to_owned();
    let mut rest = tokens
        .iter()
        .map(|token| unquote(token))
        .skip_while(|token| !token.starts_with('-') && token.contains('='));
    let Some(program) = rest.next() else {
        return false;
    };
    if Path::new(&program)
        .file_name()
        .is_none_or(|name| name != "npm")
    {
        return false;
    }
    let mut subcommand = None;
    let mut skip_value = false;
    for token in rest.by_ref() {
        if std::mem::take(&mut skip_value) {
            continue;
        }
        if token.starts_with('-') {
            skip_value = VALUE_FLAGS.contains(&token.as_str());
            continue;
        }
        subcommand = Some(token);
        break;
    }
    let Some(subcommand) = subcommand else {
        return false;
    };
    let after: Vec<String> = rest.collect();
    if after.iter().any(|token| token == "--") {
        return false;
    }
    if RUN_COMMANDS.contains(&subcommand.as_str()) {
        // Without a script name npm lists scripts and runs nothing.
        after.iter().any(|token| !token.starts_with('-'))
    } else {
        LIFECYCLE_COMMANDS.contains(&subcommand.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;

    fn draft(command: DraftCommand) -> LaunchDraft {
        LaunchDraft {
            command,
            args: Vec::new(),
            cwd: PathBuf::from("/w"),
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn shell(text: &str) -> DraftCommand {
        DraftCommand::Shell(text.to_owned())
    }

    /// Plain commands that run an npm script need `--`; everything else does not.
    #[test]
    fn separator_detection_table() {
        let cases = [
            ("npm run dev", true),
            ("npm run --silent dev", true),
            ("npm -w apps/web run dev", true),
            ("NODE_ENV=development npm run dev", true),
            ("npm start", true),
            ("cd web && npm run dev", true),
            ("npm run build &&\nnpm run dev", true),
            ("/usr/local/bin/npm run dev", true),
            ("npm run dev --", false),
            ("npm run", false),
            ("npm install", false),
            ("pnpm run dev", false),
            ("pnpm --silent -F @acme/web dev", false),
            ("yarn dev", false),
            ("npm run dev | tee log && vite", false),
            ("npx vite", false),
        ];
        for (text, expected) in cases {
            assert_eq!(needs_npm_separator(&shell(text), &[]), expected, "{text:?}");
        }
        let argv = DraftCommand::Argv(vec!["npm".into(), "run".into(), "dev".into()]);
        assert!(needs_npm_separator(&argv, &[]));
        assert!(!needs_npm_separator(&argv, &["--".to_owned()]));
        let script = DraftCommand::Script {
            source: "package.json".into(),
            name: "dev".into(),
        };
        assert!(!needs_npm_separator(&script, &[]));
    }

    /// The separator is added once, before the first forwarded argument.
    #[test]
    fn forward_args_places_separator_once() {
        let mut draft = draft(shell("npm run dev"));
        forward_args(&mut draft, vec!["--port".into(), "1".into()]);
        forward_args(&mut draft, vec!["--host".into()]);
        forward_args(&mut draft, Vec::new());
        assert_eq!(draft.args, ["--", "--port", "1", "--host"]);
        let mut draft = self::draft(shell("vite"));
        forward_args(&mut draft, vec!["--port".into(), "1".into()]);
        assert_eq!(draft.args, ["--port", "1"]);
    }
}

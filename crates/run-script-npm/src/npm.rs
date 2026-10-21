//! npm: `npm run <script> -- <args>`.
//!
//! "Use `--` to pass `-`-prefixed flags and options which would otherwise be
//! parsed by npm" (<https://docs.npmjs.com/cli/v11/commands/npm-run>). Without
//! it npm 11.17 consumes `--port` itself and passes only `3000` on.

use crate::manager::Rules;
use std::path::Path;

/// npm's argv rules.
pub(crate) struct Npm;

impl Rules for Npm {
    fn program(&self) -> &'static str {
        "npm"
    }

    fn needs_separator(&self) -> bool {
        true
    }
}

/// npm subcommands that run a script and forward arguments after `--`.
const RUN_COMMANDS: &[&str] = &["run", "run-script", "rum", "urn"];
const LIFECYCLE_COMMANDS: &[&str] = &["start", "test", "t", "tst", "restart", "stop"];

/// npm options whose value is the next token, so it is not the subcommand.
const VALUE_FLAGS: &[&str] = &["-w", "--workspace", "--prefix", "-C", "--script-shell"];

/// Recognises `[VAR=x ...] npm [flags] (run <name> | start | test ...)` with
/// no `--` after the subcommand, i.e. a plain command whose appended
/// arguments npm would otherwise swallow.
pub(crate) fn is_script_run(tokens: &[&str]) -> bool {
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

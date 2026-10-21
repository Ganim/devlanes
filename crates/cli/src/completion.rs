use std::{ffi::OsStr, sync::Arc};

use clap::CommandFactory;
use clap_complete::{ArgValueCompleter, CompleteEnv, CompletionCandidate};

use crate::Cli;

/// Candidate categories use daemon-owned names, never local filesystem inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionKind {
    Unit,
    Lane,
    Task,
    Project,
    Host,
}

/// Inject a short-timeout, existing-connection-only client at the composition root.
pub trait CompletionSource: Send + Sync {
    /// Must never auto-spawn a daemon; an unavailable peer returns an empty list.
    fn existing_daemon_candidates(&self, kind: CompletionKind, prefix: &str) -> Vec<String>;
}

/// Attaches lazy candidates; parsing normal commands never queries the source.
pub fn completion_command(source: Arc<dyn CompletionSource>) -> clap::Command {
    add_candidates(Cli::command(), &source)
}

/// Runs before argument parsing and before spawning threads, as required by CompleteEnv.
pub fn complete_environment(source: Arc<dyn CompletionSource>) {
    CompleteEnv::with_factory(move || completion_command(source.clone())).complete();
}

/// Installs dynamic completion hooks without connecting to or spawning a daemon.
pub fn write_completions(
    shell: clap_complete::Shell,
    writer: &mut dyn std::io::Write,
) -> std::io::Result<()> {
    let shells = clap_complete::env::Shells::builtins();
    let completer = shells
        .completer(&shell.to_string())
        .ok_or_else(|| std::io::Error::other("unsupported completion shell"))?;
    completer.write_registration("COMPLETE", "devlanes", "devlanes", "devlanes", writer)
}

/// Global selectors and nested command operands share the same no-spawn contract.
fn add_candidates(mut command: clap::Command, source: &Arc<dyn CompletionSource>) -> clap::Command {
    let args: Vec<_> = command
        .get_arguments()
        .filter_map(|arg| {
            let name = arg.get_id().as_str();
            let kind = match name {
                "unit" | "units" | "target" => CompletionKind::Unit,
                "lane" => CompletionKind::Lane,
                "task" => CompletionKind::Task,
                "project" => CompletionKind::Project,
                "host" => CompletionKind::Host,
                _ => return None,
            };
            Some((name.to_owned(), kind))
        })
        .collect();
    for (name, kind) in args {
        let source = source.clone();
        command = command.mut_arg(name, |arg| {
            arg.add(ArgValueCompleter::new(move |current: &OsStr| {
                let Some(prefix) = current.to_str() else {
                    return Vec::new();
                };
                source
                    .existing_daemon_candidates(kind, prefix)
                    .into_iter()
                    .filter(|v| v.starts_with(prefix))
                    .map(CompletionCandidate::new)
                    .collect::<Vec<_>>()
            }))
        });
    }
    command.mut_subcommands(|subcommand| add_candidates(subcommand, source))
}

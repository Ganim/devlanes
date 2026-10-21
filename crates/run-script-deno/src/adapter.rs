//! Core adapter: the Deno task script runner.

use crate::{DenoError, TaskRequest, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ScriptPlan, ScriptRequest, ScriptRunner},
};
use std::path::Path;

/// `run_script` runner for `deno.json` and `deno.jsonc`.
#[derive(Debug, Default, Clone, Copy)]
pub struct DenoTasks;

#[async_trait]
impl ScriptRunner for DenoTasks {
    fn id(&self) -> &'static str {
        "deno"
    }

    /// Matches `deno.json` and `deno.jsonc`.
    fn matches(&self, source: &Path) -> bool {
        handles(source)
    }

    /// Runs [`plan`] on a blocking thread because it reads the config file.
    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let worktree = request.worktree.to_path_buf();
        let source = request.source.to_path_buf();
        let name = request.name.to_owned();
        let extra_args = request.extra_args.to_vec();
        let planned = tokio::task::spawn_blocking(move || {
            plan(&TaskRequest {
                worktree: &worktree,
                source: &source,
                name: &name,
                extra_args: &extra_args,
            })
        })
        .await
        .map_err(|e| Error::Integration(format!("deno task resolution failed: {e}")))?
        .map_err(|error| match error {
            DenoError::Read { .. } => Error::Integration(error.to_string()),
            other => Error::InvalidInput(other.to_string()),
        })?;
        Ok(ScriptPlan {
            argv: planned.argv,
            cwd: planned.cwd,
            body: planned.body,
        })
    }
}

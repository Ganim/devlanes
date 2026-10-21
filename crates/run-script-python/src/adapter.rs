//! Core adapter: the `pyproject.toml` script runner.

use crate::{PythonError, ScriptRequest, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{self, ScriptPlan, ScriptRunner},
};
use std::path::Path;

/// `run_script` runner for `pyproject.toml` scripts.
#[derive(Debug, Default, Clone, Copy)]
pub struct PythonScripts;

#[async_trait]
impl ScriptRunner for PythonScripts {
    fn id(&self) -> &'static str {
        "python"
    }

    /// Matches files named `pyproject.toml`.
    fn matches(&self, source: &Path) -> bool {
        handles(source)
    }

    /// Runs [`plan`] on a blocking thread because it reads the project file
    /// and looks for lockfiles. The plan has no shell body to inspect.
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
        .map_err(|e| Error::Integration(format!("pyproject script resolution failed: {e}")))?
        .map_err(|error| match error {
            PythonError::Read { .. } => Error::Integration(error.to_string()),
            other => Error::InvalidInput(other.to_string()),
        })?;
        Ok(ScriptPlan {
            detail: Some(planned.detail()),
            argv: planned.argv,
            cwd: planned.cwd,
            body: None,
            notes: planned.notes,
        })
    }
}

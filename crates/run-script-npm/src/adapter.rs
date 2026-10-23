//! Core adapter: the `package.json` script runner.

use crate::{NpmError, ScriptRequest, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{self, ScriptPlan, ScriptRunner},
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
            detail: Some(planned.detail(request.worktree)),
            argv: planned.argv,
            cwd: planned.cwd,
            body: Some(planned.script_body),
            notes: planned.notes,
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

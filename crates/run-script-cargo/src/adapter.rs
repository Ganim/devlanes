//! Core adapter: the Cargo binary script runner.

use crate::{BinRequest, CargoError, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ScriptPlan, ScriptRequest, ScriptRunner},
};
use std::path::Path;

/// `run_script` runner for `Cargo.toml` binaries.
#[derive(Debug, Default, Clone, Copy)]
pub struct CargoBins;

#[async_trait]
impl ScriptRunner for CargoBins {
    fn id(&self) -> &'static str {
        "cargo"
    }

    /// Matches files named `Cargo.toml`.
    fn matches(&self, source: &Path) -> bool {
        handles(source)
    }

    /// Runs [`plan`] on a blocking thread: it reads manifests and lists
    /// `src/bin` directories. The plan has no body to inspect.
    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let worktree = request.worktree.to_path_buf();
        let source = request.source.to_path_buf();
        let name = request.name.to_owned();
        let extra_args = request.extra_args.to_vec();
        let planned = tokio::task::spawn_blocking(move || {
            plan(&BinRequest {
                worktree: &worktree,
                source: &source,
                name: &name,
                extra_args: &extra_args,
            })
        })
        .await
        .map_err(|e| Error::Integration(format!("cargo binary resolution failed: {e}")))?
        .map_err(|error| match error {
            CargoError::Read { .. } => Error::Integration(error.to_string()),
            other => Error::InvalidInput(other.to_string()),
        })?;
        Ok(ScriptPlan {
            argv: planned.argv,
            cwd: planned.cwd,
            body: None,
            notes: Vec::new(),
            detail: None,
        })
    }
}

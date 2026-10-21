//! Core adapter: the Makefile script runner.

use crate::{MakeError, TargetRequest, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ScriptPlan, ScriptRequest, ScriptRunner},
};
use std::path::Path;

/// `run_script` runner for Makefiles (`Makefile`, `makefile`, `GNUmakefile`,
/// `*.mk`, `*.mak`).
#[derive(Debug, Default, Clone, Copy)]
pub struct MakeTargets;

#[async_trait]
impl ScriptRunner for MakeTargets {
    fn id(&self) -> &'static str {
        "make"
    }

    /// Matches Makefile names and `.mk`/`.mak` files.
    fn matches(&self, source: &Path) -> bool {
        handles(source)
    }

    /// Runs [`plan`] on a blocking thread: it reads the Makefile and lists its
    /// directory. The plan has no body; recipes are not shell text presets inspect.
    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let worktree = request.worktree.to_path_buf();
        let source = request.source.to_path_buf();
        let name = request.name.to_owned();
        let extra_args = request.extra_args.to_vec();
        let planned = tokio::task::spawn_blocking(move || {
            plan(&TargetRequest {
                worktree: &worktree,
                source: &source,
                name: &name,
                extra_args: &extra_args,
            })
        })
        .await
        .map_err(|e| Error::Integration(format!("Makefile target resolution failed: {e}")))?
        .map_err(|error| match error {
            MakeError::Read { .. } => Error::Integration(error.to_string()),
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

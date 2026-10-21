//! Core adapter: the justfile script runner.

use crate::{JustError, RecipeRequest, handles, plan};
use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{ScriptPlan, ScriptRequest, ScriptRunner},
};
use std::path::Path;

/// `run_script` runner for justfiles (`justfile`, `.justfile` in any case,
/// `*.just`).
#[derive(Debug, Default, Clone, Copy)]
pub struct JustRecipes;

#[async_trait]
impl ScriptRunner for JustRecipes {
    fn id(&self) -> &'static str {
        "just"
    }

    /// Matches justfile names and `.just` files.
    fn matches(&self, source: &Path) -> bool {
        handles(source)
    }

    /// Runs [`plan`] on a blocking thread because it reads the justfile. The
    /// plan has no body; recipe bodies are not inspected by presets.
    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let worktree = request.worktree.to_path_buf();
        let source = request.source.to_path_buf();
        let name = request.name.to_owned();
        let extra_args = request.extra_args.to_vec();
        let planned = tokio::task::spawn_blocking(move || {
            plan(&RecipeRequest {
                worktree: &worktree,
                source: &source,
                name: &name,
                extra_args: &extra_args,
            })
        })
        .await
        .map_err(|e| Error::Integration(format!("justfile recipe resolution failed: {e}")))?
        .map_err(|error| match error {
            JustError::Read { .. } => Error::Integration(error.to_string()),
            other => Error::InvalidInput(other.to_string()),
        })?;
        Ok(ScriptPlan {
            argv: planned.argv,
            cwd: planned.cwd,
            body: None,
        })
    }
}

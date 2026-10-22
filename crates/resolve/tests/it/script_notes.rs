//! Script runner notes and detail reach the launch notes.

use std::sync::Arc;

use async_trait::async_trait;
use devlanes_core::{
    Error,
    contrib::{Contributor, ScriptPlan, ScriptRequest, ScriptRunner},
};
use devlanes_resolve::{ResolveInput, resolve_lane};

use crate::support::*;

/// Runner that reports how it picked the tool, like the npm runner does.
struct NotingRunner;

#[async_trait]
impl ScriptRunner for NotingRunner {
    fn id(&self) -> &'static str {
        "npm"
    }

    fn matches(&self, _source: &std::path::Path) -> bool {
        true
    }

    async fn resolve(&self, request: &ScriptRequest<'_>) -> Result<ScriptPlan, Error> {
        let mut argv = vec!["pnpm".to_owned(), "run".to_owned(), request.name.to_owned()];
        argv.extend(request.extra_args.iter().cloned());
        Ok(ScriptPlan {
            argv,
            cwd: request.worktree.to_path_buf(),
            body: None,
            notes: vec!["no packageManager field; detected pnpm from pnpm-lock.yaml".to_owned()],
            detail: Some("pnpm (lockfile pnpm-lock.yaml)".to_owned()),
        })
    }
}

#[tokio::test]
/// Notes survive both the first render and the re-render with contributor args.
async fn runner_notes_reach_launch() {
    let config = parse(
        r#"
project: demo
services:
  plain:
    run_script: web/package.json#dev
  preset:
    preset: vite
    run_script: web/package.json#dev
"#,
    );
    let facts = facts("demo", 1, None);
    let flags: Arc<dyn Contributor> = Arc::new(FakeContributor {
        port_flag_for: Some("vite"),
        ..FakeContributor::env("vite", 100, vec![])
    });
    let contributors = [flags];
    let (block, offsets) = allocation(&config, 21_000, &contributors);
    let runners: [Arc<dyn ScriptRunner>; 1] = [Arc::new(NotingRunner)];
    let mut input = ResolveInput::new(&config, &facts, block, &offsets);
    input.contributors = &contributors;
    input.script_runners = &runners;
    let lane = resolve_lane(input).await.expect("resolves");
    let expected = [
        "npm: pnpm (lockfile pnpm-lock.yaml)",
        "no packageManager field; detected pnpm from pnpm-lock.yaml",
    ];
    for unit in ["plain", "preset"] {
        assert_eq!(service_notes(&lane.services[unit]), expected, "{unit}");
    }
    assert_eq!(lane.services["preset"].launch.args[2], "--port");
}

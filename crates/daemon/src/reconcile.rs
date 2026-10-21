//! Startup reconciliation (plan 3.7, 7.6). Runs before the daemon accepts
//! clients. Unfinished `unit_runs` whose recorded identity still verifies belong
//! to a crashed or restarted daemon: they keep running as `orphaned` units
//! (decision 16 overrides plan 4.3, see `orphans`); the rest are marked ended.
//! Containers are only re-adopted by label, never stopped.

use std::sync::Arc;

use devlanes_proto::RpcError;
use serde_json::json;

use crate::{
    engine::{Inner, once},
    registry::store_error,
};

impl Inner {
    /// Reloads known projects, records surviving units as orphans and starts
    /// background loops. Never terminates anything.
    pub(crate) async fn reconcile(self: &Arc<Self>) -> Result<(), RpcError> {
        if !once(&self.background) {
            return Ok(());
        }
        let runs = self.store.active_unit_runs().await.map_err(store_error)?;
        self.load_known_projects().await?;
        self.track_orphans(runs).await;
        let names: Vec<String> = self.all_projects().iter().map(|p| p.name()).collect();
        match self.integ.infra.adopt(&names).await {
            Ok(adopted) => tracing::info!(%adopted, "re-adopted containers"),
            Err(error) => tracing::debug!(code = %error.code, "container adoption skipped"),
        }
        self.start_dns().await;
        self.start_background();
        Ok(())
    }

    /// Rebuilds projects and lanes from SQLite so `status` shows them before any
    /// client cwd is seen. Lanes without a client env show `needs_env` when wanted.
    async fn load_known_projects(self: &Arc<Self>) -> Result<(), RpcError> {
        let projects = self.store.projects().await.map_err(store_error)?;
        let dirs = self
            .store
            .project_config_dirs()
            .await
            .map_err(store_error)?;
        for record in projects {
            let Some((_, config_dir)) = dirs.iter().find(|(id, _)| *id == record.id) else {
                continue;
            };
            let project = self
                .register_project(
                    record.id,
                    record.name.clone(),
                    record.root_path.clone(),
                    record.common_dir.clone(),
                    record.primary_branch.clone(),
                    config_dir.clone(),
                )
                .await?;
            for lane in project.lanes() {
                if lane.st().present {
                    self.reload_config(&project, &lane).await;
                }
            }
            self.refresh_views(&project).await;
        }
        Ok(())
    }

    /// Re-lists every project, then starts watchers, the sampler and retention.
    fn start_background(self: &Arc<Self>) {
        let engine = self.clone();
        tokio::spawn(async move {
            for project in engine.all_projects() {
                let (root, common_dir, primary) = {
                    let st = project.st();
                    (
                        st.root.clone(),
                        st.common_dir.clone(),
                        st.primary_branch.clone(),
                    )
                };
                let repository = match common_dir {
                    Some(common_dir) => match devlanes_git::list(&root, &common_dir).await {
                        Ok(worktrees) => Some(devlanes_git::Repository {
                            common_dir,
                            root,
                            primary_branch: primary,
                            worktrees,
                        }),
                        Err(error) => {
                            tracing::debug!(%error, "project re-list failed");
                            None
                        }
                    },
                    None => None,
                };
                if repository.is_some() {
                    let _ = engine.relist(&project).await;
                }
                engine.watch_project(&project, repository);
            }
        });
        tokio::spawn(self.clone().sampler_loop());
        tokio::spawn(self.clone().state_maintenance_loop());
        tokio::spawn(self.clone().log_maintenance_loop());
        tokio::spawn(self.clone().orphan_watch_loop());
        if let Some(mut events) = self.integ.infra.events() {
            let engine = self.clone();
            tokio::spawn(async move {
                loop {
                    let event = tokio::select! {
                        () = engine.shutdown.cancelled() => return,
                        event = events.recv() => event,
                    };
                    match event {
                        Ok(event) => engine.bus.emit("infra.changed", json!(event)),
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                    }
                }
            });
        }
    }
}

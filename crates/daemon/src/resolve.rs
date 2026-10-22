//! Full lane resolution (launch plans with env) and the evaluated infra spec.

use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use devlanes_config::{ComposeFiles, Run};
use devlanes_core::ports::{PortBlock, PortName};
use devlanes_proto::RpcError;
use devlanes_resolve::{ResolveError, ResolveInput, ResolvedLane};
use devlanes_state::BlockOwner;
use serde_json::{Value, json};

use crate::{
    engine::Inner,
    integrations::{ContainerDecl, InfraPortNeed, InfraSpec, PortScope},
    model::{ConfigState, EnvMap, LaneRt, ProjectRt},
    registry::{lane_context, read_dotenv, store_error},
};

/// Shapes resolution failures as machine codes with data.
pub(crate) fn resolve_error(error: &ResolveError) -> RpcError {
    let (code, data, exit) = match error {
        ResolveError::Expression { path, message } | ResolveError::Invalid { path, message } => {
            ("config_error", json!({"path": path, "message": message}), 1)
        }
        ResolveError::UnknownProfile { name, available } => (
            "unknown_profile",
            json!({"profile": name, "candidates": available}),
            2,
        ),
        ResolveError::NoMatch {
            selector,
            candidates,
        } => (
            "not_found",
            json!({"reference": selector, "candidates": candidates}),
            3,
        ),
        ResolveError::InvalidSelector { selector, message } => (
            "invalid_reference",
            json!({"reference": selector, "message": message}),
            2,
        ),
        ResolveError::Cycle { chain } => ("config_error", json!({"cycle": chain}), 1),
        ResolveError::MissingOffset { unit, name } => {
            ("port_error", json!({"unit": unit, "port": name}), 1)
        }
        ResolveError::Port { unit, message } => {
            ("port_error", json!({"unit": unit, "message": message}), 1)
        }
        ResolveError::NoScriptRunner { unit, file } => {
            ("script_error", json!({"unit": unit, "file": file}), 1)
        }
        ResolveError::Script { unit, message } => {
            ("script_error", json!({"unit": unit, "message": message}), 1)
        }
        ResolveError::Contributor { unit, id, message } => (
            "contributor_failed",
            json!({"unit": unit, "contributor": id, "message": message}),
            1,
        ),
        ResolveError::SecretsLocked { profile, message } => (
            "secrets_locked",
            json!({"profile": profile, "message": message}),
            1,
        ),
        ResolveError::Secrets { profile, message } => (
            "secrets_failed",
            json!({"profile": profile, "message": message}),
            1,
        ),
    };
    RpcError::new(code, data, exit)
}

impl Inner {
    /// Plans every unit of a lane with its stored client env. Fails with
    /// `needs_env` when the lane has none (decision 7).
    pub(crate) async fn resolve_full(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
    ) -> Result<Arc<ResolvedLane>, RpcError> {
        self.resolve_with(project, lane, None).await
    }

    /// Like [`Self::resolve_full`]; `fallback` serves read-only requests (such as
    /// `env --explain`) on a lane that has no stored env yet. It is not stored.
    pub(crate) async fn resolve_with(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        fallback: Option<Arc<EnvMap>>,
    ) -> Result<Arc<ResolvedLane>, RpcError> {
        self.resolve_planned(project, lane, fallback, None, None)
            .await
    }

    /// Like [`Self::resolve_full`] with `extra` layered over the lane's overrides,
    /// for one `run --env` that must not change the lane's later plans.
    pub(crate) async fn resolve_overridden(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        extra: &EnvMap,
    ) -> Result<Arc<ResolvedLane>, RpcError> {
        self.resolve_planned(project, lane, None, None, Some(extra))
            .await
    }

    /// Like [`Self::resolve_full`], against `config` instead of the lane's current
    /// one: a start keeps planning what it selected even if the file reloads.
    pub(crate) async fn resolve_config(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        config: Arc<devlanes_config::ProjectConfig>,
    ) -> Result<Arc<ResolvedLane>, RpcError> {
        self.resolve_planned(project, lane, None, Some(config), None)
            .await
    }

    /// Shared body of the resolve methods; `config` overrides the lane's, `extra`
    /// is layered over the lane's overrides.
    async fn resolve_planned(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        fallback: Option<Arc<EnvMap>>,
        config: Option<Arc<devlanes_config::ProjectConfig>>,
        extra: Option<&EnvMap>,
    ) -> Result<Arc<ResolvedLane>, RpcError> {
        let reference = lane.st().reference(&project.name());
        let (config, allocation, env, mut overrides, profiles, infra_ns, index) = {
            let st = lane.st();
            let config = match (config, &st.config) {
                (Some(config), _) => config,
                (None, ConfigState::Loaded(config)) => config.clone(),
                (None, ConfigState::Invalid(error)) => return Err(error.clone()),
                (None, ConfigState::Missing) => {
                    return Err(RpcError::new("no_config", json!({"lane": reference}), 1));
                }
            };
            let allocation = st.allocation.clone().ok_or_else(|| {
                RpcError::new(
                    "unassigned",
                    json!({"lane": reference, "reason": "no_ports"}),
                    1,
                )
            })?;
            let env = st
                .env
                .clone()
                .or(fallback)
                .ok_or_else(|| RpcError::new("needs_env", json!({"lane": reference}), 1))?;
            (
                config,
                allocation,
                env,
                st.overrides.clone(),
                st.profiles.clone(),
                st.infra_ns.clone().unwrap_or(Value::Null),
                st.index(),
            )
        };
        if let Some(extra) = extra {
            for (name, value) in extra {
                devlanes_core::env::insert(&mut overrides, name.clone(), value.clone());
            }
        }
        let facts = self.facts(project, lane)?;
        let (lane_dir, primary_dir) = self.config_dirs(project, lane);
        let dotenv = read_dotenv(lane_dir, primary_dir).await;
        let lanes_main = if index == Some(0) {
            None
        } else {
            project
                .lane_by_index(0)
                .and_then(|main| main.st().view.as_ref().map(|view| view.svc.clone()))
        };
        let projects = self.projects_namespace(project, lane);
        let mut input = ResolveInput::new(&config, &facts, allocation.block, &allocation.offsets);
        input.client_env = &env;
        input.secrets = Some(self.secrets.as_ref());
        input.dotenv = &dotenv;
        input.infra = &infra_ns;
        input.lanes_main = lanes_main.as_ref();
        input.projects = &projects;
        input.profiles = &profiles;
        input.overrides = &overrides;
        input.contributors = &self.integ.contributors;
        input.script_runners = &self.integ.script_runners;
        input.ca_env = &self.integ.ca_env;
        input.public_https_port = self.integ.routes.https_port();
        input.shell = &self.integ.shell;
        devlanes_resolve::resolve_lane(input)
            .await
            .map(Arc::new)
            .map_err(|error| resolve_error(&error))
    }

    /// `projects.<name>.svc` for other projects with running lanes: the lane on the
    /// same branch label, else the same lane name, else lane 0 (plan 10.3).
    fn projects_namespace(&self, project: &ProjectRt, lane: &LaneRt) -> Value {
        let (label, name) = {
            let st = lane.st();
            (
                st.branch.as_deref().map(devlanes_resolve::branch_label),
                st.record.as_ref().map(|r| r.name.clone()),
            )
        };
        let mut namespace = serde_json::Map::new();
        for other in self.all_projects() {
            if other.id == project.id {
                continue;
            }
            let lanes: Vec<_> = other
                .lanes()
                .into_iter()
                .filter(|l| l.st().services_alive())
                .collect();
            let pick =
                |predicate: &dyn Fn(&LaneRt) -> bool| lanes.iter().find(|l| predicate(l)).cloned();
            let chosen = pick(&|l| {
                label.is_some()
                    && l.st().branch.as_deref().map(devlanes_resolve::branch_label) == label
            })
            .or_else(|| {
                pick(&|l| name.is_some() && l.st().record.as_ref().map(|r| r.name.clone()) == name)
            })
            .or_else(|| pick(&|l| l.st().index() == Some(0)));
            if let Some(chosen) = chosen
                && let Some(view) = chosen.st().view.clone()
            {
                namespace.insert(other.name(), json!({"svc": view.svc}));
            }
        }
        Value::Object(namespace)
    }

    /// The lane's infra with expressions evaluated and ports filled from its blocks.
    pub(crate) async fn infra_spec(
        self: &Arc<Self>,
        project: &Arc<ProjectRt>,
        lane: &Arc<LaneRt>,
        needs: &[InfraPortNeed],
    ) -> Result<InfraSpec, RpcError> {
        let (config, allocation, view, env, infra_ns) = {
            let st = lane.st();
            let config = st
                .config
                .loaded()
                .cloned()
                .ok_or_else(|| RpcError::new("no_config", json!({"path": st.path}), 1))?;
            let allocation = st
                .allocation
                .clone()
                .ok_or_else(|| RpcError::new("unassigned", json!({"reason": "no_ports"}), 1))?;
            (
                config,
                allocation,
                st.view.clone(),
                st.env.clone(),
                st.infra_ns.clone().unwrap_or(Value::Null),
            )
        };
        let facts = self.facts(project, lane)?;
        let (lane_dir, primary_dir) = self.config_dirs(project, lane);
        let dotenv = read_dotenv(lane_dir.clone(), primary_dir.clone()).await;
        let client: EnvMap = env.map(|e| (*e).clone()).unwrap_or_default();
        let (domain, port, vars) = match &view {
            Some(view) => (view.domain.clone(), view.port, view.vars.clone()),
            None => (String::new(), allocation.block.start, Default::default()),
        };
        let context = lane_context(&facts, &domain, port, &vars, &infra_ns, &client, &dotenv);
        let infra = &config.infra;
        let evaluate = |path: &str, value: &devlanes_config::Templated<String>| {
            value.evaluate(&context).map_err(|error| {
                RpcError::new(
                    "config_error",
                    json!({"path": path, "message": error.message}),
                    1,
                )
            })
        };
        let mut env_map = BTreeMap::new();
        for (key, value) in &infra.env {
            let value = value.evaluate(&context).map_err(|error| {
                RpcError::new(
                    "config_error",
                    json!({"path": format!("infra.env.{key}"), "message": error.message}),
                    1,
                )
            })?;
            env_map.insert(key.clone(), value.to_string());
        }
        let mut containers = Vec::new();
        for (name, container) in &infra.containers {
            let path = format!("infra.containers.{name}");
            let image = match &container.image {
                Some(image) => evaluate(&format!("{path}.image"), image)?,
                None => {
                    return Err(RpcError::new(
                        "config_error",
                        json!({"path": path, "message": "image is required"}),
                        1,
                    ));
                }
            };
            let command = match &container.command {
                Some(Run::Argv(items)) => Some(
                    items
                        .iter()
                        .map(|item| evaluate(&format!("{path}.command"), item))
                        .collect::<Result<Vec<_>, _>>()?,
                ),
                Some(Run::Shell(text)) => Some(vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    evaluate(&format!("{path}.command"), text)?,
                ]),
                None => None,
            };
            let mut volumes = Vec::new();
            for volume in container.volumes.iter().flatten() {
                volumes.push(evaluate(&format!("{path}.volumes"), volume)?);
            }
            let mut ports = Vec::new();
            for (port_name, value) in &container.ports {
                let port = value.evaluate(&context).map_err(|error| {
                    RpcError::new("config_error", json!({"path": format!("{path}.ports.{port_name}"), "message": error.message}), 1)
                })?;
                ports.push((port_name.clone(), port));
            }
            let per_lane = match &container.per_lane {
                Some(value) => value.evaluate(&context).map_err(|error| {
                    RpcError::new(
                        "config_error",
                        json!({"path": format!("{path}.per_lane"), "message": error.message}),
                        1,
                    )
                })?,
                None => false,
            };
            let mut env = BTreeMap::new();
            for (key, value) in &container.env {
                let value = value.evaluate(&context).map_err(|error| {
                    RpcError::new(
                        "config_error",
                        json!({"path": format!("{path}.env.{key}"), "message": error.message}),
                        1,
                    )
                })?;
                env.insert(key.clone(), value.to_string());
            }
            let ready = match &container.ready {
                Some(ready) => {
                    let cwd = if per_lane { &lane_dir } else { &primary_dir };
                    devlanes_resolve::container_ready(
                        ready,
                        cwd,
                        self.integ.shell.kind,
                        &context,
                        &format!("{path}.ready"),
                    )
                    .map_err(|error| resolve_error(&error))?
                }
                None => None,
            };
            containers.push(ContainerDecl {
                name: name.clone(),
                image,
                command,
                volumes,
                ports,
                per_lane,
                env,
                ready,
            });
        }
        let project_name = match &infra.project_name {
            Some(value) => Some(evaluate("infra.project_name", value)?),
            None => None,
        };
        let compose_files = match &infra.compose {
            Some(ComposeFiles::File(file)) => vec![PathBuf::from(file)],
            Some(ComposeFiles::Files(files)) => files.iter().map(PathBuf::from).collect(),
            None => Vec::new(),
        };
        let shared = self
            .store
            .allocation(BlockOwner::Project(project.id))
            .await
            .map_err(store_error)?;
        let mut ports = BTreeMap::new();
        for need in needs {
            let key = PortName::new(need.unit.clone(), need.name.clone());
            let (block, offsets) = match need.scope {
                PortScope::Lane => (Some(allocation.block), &allocation.offsets),
                PortScope::Shared => match &shared {
                    Some(shared) => (Some(shared.block), &shared.offsets),
                    None => (None, &allocation.offsets),
                },
            };
            if let (Some(block), Some(offset)) = (block, offsets.get(&key))
                && let Ok(port) = PortBlock::port(block, *offset)
            {
                ports.insert(need.key(), port);
            }
        }
        let index = facts.index;
        Ok(InfraSpec {
            project: project.name(),
            lane: index,
            lane_dir,
            primary_dir,
            compose_files,
            project_name,
            exclude: infra.exclude.clone().unwrap_or_default(),
            per_lane: infra.per_lane.clone().unwrap_or_default(),
            profiles: infra.profiles.clone().unwrap_or_default(),
            env: env_map,
            containers,
            block: (allocation.block.start, allocation.block.size),
            shared_block: shared.map(|s| (s.block.start, s.block.size)),
            ports,
            state_dir: self
                .opts
                .home
                .join("infra")
                .join(project.name())
                .join(format!("lane{index}")),
        })
    }
}

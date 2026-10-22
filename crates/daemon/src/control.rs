//! Mutating methods: start, stop, restart, resume, lanes, exec, run, unlock, infra
//! and reset. Each returns structured data only.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
    time::Duration,
};

use devlanes_config::{ExpressionContext, OpaqueExtensions};
use devlanes_core::{LaunchSpec, TerminalSize, ports::PortName};
use devlanes_proto::{
    Context, ExecParams, Page, RpcError, RunParams, Selection, UnitSummary, events,
};
use devlanes_state::DesiredState;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    Session,
    engine::{Inner, LanePair, carries_env, confirmation, params, required},
    lifecycle::{SpawnRequest, StartOptions},
    model::{EnvMap, Kind, lane_summary, lock, unit_summary},
    registry::{internal, store_error},
    stream::LaneEvents,
};

/// `resume` parameters.
#[derive(Debug, Default, Deserialize)]
struct ResumeParams {
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Stream the starts' lane events on the request.
    #[serde(default)]
    events: bool,
    /// Bypass the start gate, as `start --all-at-once` does. The lanes start
    /// concurrently either way; without it they queue in the gate together.
    #[serde(default)]
    all_at_once: bool,
}

/// `lane.new` parameters.
#[derive(Debug, Deserialize)]
struct LaneNewParams {
    branch: String,
    #[serde(default)]
    start: bool,
    /// Stream the start's lane events on the request.
    #[serde(default)]
    events: bool,
}

/// `lane.clean` parameters.
#[derive(Debug, Default, Deserialize)]
struct LaneCleanParams {
    #[serde(default)]
    lane: Option<String>,
}

/// `run` parameters: proto's [`RunParams`] plus the CLI's timeout.
#[derive(Debug, Default, Deserialize)]
struct RunArgs {
    #[serde(flatten)]
    run: RunParams,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// `unlock` parameters; values stay in memory only.
#[derive(Default, Deserialize)]
struct UnlockParams {
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    remove: Vec<String>,
    #[serde(default)]
    profile: Option<String>,
}

impl Inner {
    /// Wait limit of a request.
    fn timeout(&self, timeout_ms: Option<u64>) -> Duration {
        timeout_ms.map_or(self.opts.start_timeout, Duration::from_millis)
    }

    /// Resolves the lane for a selection and re-lists its worktrees first (plan 5.2).
    async fn selection_lane(
        self: &Arc<Self>,
        units: &[String],
        context: &Context,
    ) -> Result<(LanePair, Vec<String>), RpcError> {
        if let Some(project) = self.project_for_cwd(&context.cwd).await? {
            let _ = self.relist(&project).await;
        }
        self.lane_for_units(units, context).await
    }

    /// `start`: the lane pipeline, then waits for ready/failed (StartResult).
    pub(crate) async fn start_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        // First of all: the start gate serves requests in the order they came in,
        // whatever their lane lookup, infra or tasks take.
        let arrival = self.arrive();
        let selection: Selection = params(value)?;
        let ((project, lane), selectors) = self.selection_lane(&selection.units, context).await?;
        self.fresh_config(&project, &lane).await;
        let start_id = self.new_start_id();
        let stream = selection
            .events
            .then(|| LaneEvents::starts(vec![(project.clone(), lane.clone())], vec![start_id]));
        let start = self.start_lane(
            &project,
            &lane,
            StartOptions {
                selectors,
                profiles: selection.profile_names(),
                env: carries_env(context),
                overrides: selection.env.clone(),
                actor: context.actor.clone(),
                timeout: self.timeout(selection.timeout_ms),
                session: None,
                setup_only: false,
                all_at_once: selection.all_at_once,
                start_id: Some(start_id),
                arrival,
            },
        );
        let result = self.with_lane_events(session, stream, start).await?;
        serde_json::to_value(result).map_err(|e| internal("encode", e))
    }

    /// `stop`: selected units, the whole lane, or (`mine`) the caller's units.
    pub(crate) async fn stop_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let selection: Selection = params(value)?;
        let mut stopped: Vec<UnitSummary> = Vec::new();
        // `stop --mine` across lanes: one lane's failure must not leave the rest running.
        let mut errors: Vec<Value> = Vec::new();
        if selection.mine && selection.units.is_empty() && context.lane.is_none() {
            for (project, lane) in self.all_lanes() {
                if lane.st().any_running() {
                    match self
                        .stop_lane(&project, &lane, None, Some(&context.actor))
                        .await
                    {
                        Ok(units) => stopped.extend(units),
                        Err(error) => errors.push(json!({
                            "lane": lane.st().reference(&project.name()),
                            "error": error,
                        })),
                    }
                }
            }
        } else {
            let ((project, lane), selectors) =
                self.lane_for_units(&selection.units, context).await?;
            let selectors = (!selectors.is_empty()).then_some(selectors);
            let mine = selection.mine.then_some(&context.actor);
            stopped = self.stop_lane(&project, &lane, selectors, mine).await?;
        }
        let mut result =
            serde_json::to_value(Page::complete(stopped)).map_err(|e| internal("encode", e))?;
        if !errors.is_empty() {
            result["errors"] = json!(errors);
        }
        Ok(result)
    }

    /// `restart`: stops and starts named units with fresh plans.
    pub(crate) async fn restart_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let arrival = self.arrive();
        let selection: Selection = params(value)?;
        if selection.units.is_empty() {
            return Err(RpcError::new("units_required", json!({}), 2));
        }
        let ((project, lane), selectors) = self.lane_for_units(&selection.units, context).await?;
        self.fresh_config(&project, &lane).await;
        let start_id = self.new_start_id();
        let stream = selection
            .events
            .then(|| LaneEvents::starts(vec![(project.clone(), lane.clone())], vec![start_id]));
        let restart = self.restart_units(
            &project,
            &lane,
            &selectors,
            StartOptions {
                selectors: selectors.clone(),
                profiles: selection.profile_names(),
                env: carries_env(context),
                overrides: selection.env.clone(),
                actor: context.actor.clone(),
                timeout: self.timeout(selection.timeout_ms),
                session: None,
                setup_only: false,
                all_at_once: selection.all_at_once,
                start_id: Some(start_id),
                arrival,
            },
        );
        let result = self.with_lane_events(session, stream, restart).await?;
        serde_json::to_value(result).map_err(|e| internal("encode", e))
    }

    /// `resume`: starts every lane whose desired state is running, with this shell's env.
    pub(crate) async fn resume_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let arrival = self.arrive();
        let args: ResumeParams = params(value)?;
        let Some(env) = carries_env(context) else {
            return Err(RpcError::new("needs_env", json!({}), 1));
        };
        let wanted: Vec<LanePair> = self
            .all_lanes()
            .into_iter()
            .filter(|(_, lane)| {
                let st = lane.st();
                st.present
                    && !st.services_alive()
                    && st
                        .record
                        .as_ref()
                        .is_some_and(|r| r.desired_state == DesiredState::Running)
            })
            .collect();
        let starts: Vec<u64> = wanted.iter().map(|_| self.new_start_id()).collect();
        let stream = args
            .events
            .then(|| LaneEvents::starts(wanted.clone(), starts.clone()));
        // Concurrently, under the one arrival of this request: the lanes queue in
        // the start gate together instead of one after another's readiness.
        let resume = async {
            let results = futures_util::future::join_all(wanted.iter().zip(starts).map(
                |((project, lane), start_id)| {
                    self.start_lane(
                        project,
                        lane,
                        StartOptions {
                            selectors: Vec::new(),
                            profiles: Vec::new(),
                            env: Some(env.clone()),
                            overrides: EnvMap::new(),
                            actor: context.actor.clone(),
                            timeout: self.timeout(args.timeout_ms),
                            session: None,
                            setup_only: false,
                            all_at_once: args.all_at_once,
                            start_id: Some(start_id),
                            arrival,
                        },
                    )
                },
            ))
            .await;
            let mut items = Vec::new();
            for result in results {
                items.push(match result {
                    Ok(result) => {
                        serde_json::to_value(result).map_err(|e| internal("encode", e))?
                    }
                    Err(error) => json!({"lane": error.data.get("lane"), "error": error}),
                });
            }
            Ok(json!({"items": items, "truncated": false, "next_cursor": null}))
        };
        self.with_lane_events(session, stream, resume).await
    }

    /// `lane.new`: `git worktree add` under `lanes.worktree_dir`, then registration.
    pub(crate) async fn lane_new(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let arrival = self.arrive();
        let args: LaneNewParams = required(value)?;
        let Some(project) = self.project_for_cwd(&context.cwd).await? else {
            return Err(crate::engine::no_config(&context.cwd).await);
        };
        if !project.git {
            return Err(RpcError::new(
                "not_a_git_project",
                json!({"project": project.name()}),
                1,
            ));
        }
        let primary = self.primary_path(&project);
        let config = project
            .lanes()
            .iter()
            .find_map(|lane| lane.st().config.loaded().cloned())
            .ok_or_else(|| RpcError::new("no_config", json!({}), 1))?;
        let name = project.name();
        let context_ns = ExpressionContext {
            project: json!({"name": name, "primary_path": primary.display().to_string()}),
            ..ExpressionContext::default()
        };
        let dir = match &config.lanes.worktree_dir {
            Some(value) => value.evaluate(&context_ns).map_err(|error| {
                RpcError::new(
                    "config_error",
                    json!({"path": "lanes.worktree_dir", "message": error.message}),
                    1,
                )
            })?,
            None => format!("../{name}-lanes"),
        };
        let base = primary.join(&project.config_dir).join(dir);
        let destination = base.join(devlanes_resolve::branch_label(&args.branch));
        let root = project.st().root.clone();
        devlanes_git::add(&root, &destination, &args.branch)
            .await
            .map_err(|error| {
                RpcError::new("git_failed", json!({"message": error.to_string()}), 1)
            })?;
        let canonical = tokio::task::spawn_blocking({
            let destination = destination.clone();
            // dunce: Git lists `C:\...`, never the verbatim `\\?\C:\...` form.
            move || dunce::canonicalize(&destination).unwrap_or(destination)
        })
        .await
        .unwrap_or(destination);
        let report = self.relist(&project).await?;
        drop(report);
        let lane = project
            .lanes()
            .into_iter()
            .find(|lane| devlanes_core::paths::same_path(&lane.st().path, &canonical))
            .ok_or_else(|| RpcError::new("not_found", json!({"reference": canonical}), 3))?;
        let summary = lane_summary(&name, &lane.st());
        let mut result = json!({"lane": summary});
        if args.start {
            let env = carries_env(context).or_else(|| project.st().last_env.clone());
            let start_id = self.new_start_id();
            let stream = args
                .events
                .then(|| LaneEvents::starts(vec![(project.clone(), lane.clone())], vec![start_id]));
            let start = self.start_lane(
                &project,
                &lane,
                StartOptions {
                    selectors: Vec::new(),
                    profiles: Vec::new(),
                    env,
                    overrides: EnvMap::new(),
                    actor: context.actor.clone(),
                    timeout: self.opts.start_timeout,
                    session: None,
                    setup_only: false,
                    all_at_once: false,
                    start_id: Some(start_id),
                    arrival,
                },
            );
            let start = self.with_lane_events(session, stream, start).await?;
            result["start"] = serde_json::to_value(start).map_err(|e| internal("encode", e))?;
        }
        Ok(result)
    }

    /// `lane.clean` (requires `yes`).
    pub(crate) async fn lane_clean_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: LaneCleanParams = params(value)?;
        let reference = args.lane.or_else(|| context.lane.clone());
        let (project, lane) = match &reference {
            Some(reference) => self.resolve_lane_ref(reference, context).await?,
            None => self.cwd_lane(context).await?,
        };
        let name = lane.st().reference(&project.name());
        if !context.yes {
            return Err(confirmation(json!({"lane": name, "flag": "--yes"})));
        }
        self.clean_lane(&project, &lane, Some(&context.actor)).await
    }

    /// `lane.gc`: cleans lanes whose worktrees are gone and drops port offsets no
    /// config uses any more. Needs `yes` only when there are lanes to clean; the
    /// confirmation lists them under `lanes`.
    pub(crate) async fn lane_gc(self: &Arc<Self>, context: &Context) -> Result<Value, RpcError> {
        if let Some(project) = self.project_for_cwd(&context.cwd).await? {
            let _ = self.relist(&project).await;
        }
        if !context.yes {
            let lanes: Vec<String> = self
                .all_lanes()
                .iter()
                .filter_map(|(project, lane)| {
                    let st = lane.st();
                    let running = st.any_running() || st.busy.is_some();
                    (!st.present && !running).then(|| st.reference(&project.name()))
                })
                .collect();
            if !lanes.is_empty() {
                return Err(confirmation(json!({"flag": "--yes", "lanes": lanes})));
            }
        }
        let mut cleaned = Vec::new();
        let mut kept_running = Vec::new();
        for (project, lane) in self.all_lanes() {
            let (present, assigned, reference, running) = {
                let st = lane.st();
                (
                    st.present,
                    st.record.is_some(),
                    st.reference(&project.name()),
                    st.any_running() || st.busy.is_some(),
                )
            };
            if present {
                continue;
            }
            // A presence flag can be wrong (a re-list from elsewhere, a racing
            // watcher); a lane that runs something is never collected.
            if running {
                kept_running.push(reference);
                continue;
            }
            // Gone only after the check above: it was not confirmed.
            if !context.yes {
                continue;
            }
            if assigned {
                self.clean_lane(&project, &lane, Some(&context.actor))
                    .await?;
            } else {
                project.st().lanes.remove(&lane.worktree);
                lane.watch.cancel();
            }
            cleaned.push(reference);
        }
        let mut removed = 0;
        for project in self.all_projects() {
            let configs: Vec<_> = project
                .lanes()
                .iter()
                .filter_map(|lane| lane.st().config.loaded().cloned())
                .collect();
            let Some(config) = configs.first() else {
                continue;
            };
            let Ok(needs) = devlanes_resolve::port_needs(config, &self.integ.contributors) else {
                continue;
            };
            let existing = self
                .store
                .allocation(devlanes_state::BlockOwner::Project(project.id))
                .await
                .ok()
                .flatten();
            let mut live: BTreeSet<PortName> = needs.names.clone();
            for lane in project.lanes() {
                if let Some(allocation) = &lane.st().allocation {
                    // Infra names come from the provider; keep them while their lane exists.
                    live.extend(
                        allocation
                            .offsets
                            .keys()
                            .filter(|name| name.unit.starts_with("infra."))
                            .cloned(),
                    );
                }
            }
            if let Some(existing) = existing {
                live.extend(
                    existing
                        .offsets
                        .keys()
                        .filter(|name| name.unit.starts_with("infra."))
                        .cloned(),
                );
            }
            removed += self
                .store
                .gc_offsets(project.id, live, needs.reserved.clone())
                .await
                .map_err(store_error)?;
        }
        for project in self.all_projects() {
            self.refresh_views(&project).await;
        }
        Ok(json!({"cleaned": cleaned, "kept_running": kept_running, "offsets_removed": removed}))
    }

    /// `exec`: an ad-hoc unit in the cwd's lane (kind `exec`).
    pub(crate) async fn exec_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: ExecParams = required(value)?;
        let Some((program, rest)) = args.command.split_first() else {
            return Err(RpcError::new(
                "bad_request",
                json!({"message": "empty command"}),
                2,
            ));
        };
        if args.name.is_empty() || args.name.contains(['#', ':', '@', '/']) {
            return Err(RpcError::new(
                "invalid_reference",
                json!({"reference": args.name}),
                2,
            ));
        }
        let (project, lane) = self.target_lane(context).await?;
        // An exec unit under a configured name would take that unit's place: its
        // logs, its stop and the next start of the real one.
        let taken = {
            let st = lane.st();
            let config = st.config.loaded();
            let service = config.is_some_and(|c| c.services.contains_key(&args.name))
                || st
                    .view
                    .as_ref()
                    .is_some_and(|view| view.services.iter().any(|s| s.unit == args.name));
            let task = config.is_some_and(|c| c.tasks.contains_key(&args.name));
            match (service, task) {
                (true, _) => Some("service"),
                (_, true) => Some("task"),
                _ => None,
            }
            .map(|kind| (kind, st.reference(&project.name())))
        };
        if let Some((kind, reference)) = taken {
            return Err(RpcError::new(
                "name_taken",
                json!({"name": args.name, "kind": kind, "lane": reference}),
                2,
            ));
        }
        if let Some(env) = carries_env(context) {
            lane.st().env = Some(env.clone());
            project.st().last_env = Some(env);
        }
        let (mut env, lane_env, reference) = {
            let st = lane.st();
            (
                st.env.as_deref().cloned().unwrap_or_default(),
                st.view
                    .as_ref()
                    .map(|view| view.lane_env.clone())
                    .unwrap_or_default(),
                st.reference(&project.name()),
            )
        };
        env.extend(lane_env);
        env.extend(args.env.clone());
        let cwd = if context.cwd.as_os_str().is_empty() {
            lane.st().path.clone()
        } else {
            context.cwd.clone()
        };
        let environment =
            devlanes_core::env::assemble([(devlanes_core::env::EnvLayer::Client, env.clone())]);
        let request = SpawnRequest {
            name: args.name.clone(),
            kind: Kind::Exec,
            spec: LaunchSpec {
                program: program.clone(),
                args: rest.to_vec(),
                cwd,
                env,
                pty: args.pty,
                size: TerminalSize::default(),
            },
            env: Arc::new(environment),
            files: Vec::new(),
            actor: context.actor.clone(),
            ports: BTreeMap::new(),
            urls: Vec::new(),
            tags: Vec::new(),
            hidden: false,
            autostart: false,
            stop: devlanes_core::StopPolicy::default(),
            literal_port: None,
            preset: None,
            color: None,
            sync: Vec::new(),
        };
        // Held until the unit is recorded, so a clean or reset that begins meanwhile
        // waits and then stops it, or refuses it once the lane is released.
        let (adhoc, _) = lane.enter_adhoc(&self.shutdown).await;
        let (host, _) = self.spawn_unit(&project, &lane, request).await?;
        drop(adhoc);
        host.set_state(crate::units::UnitState::Ready);
        let st = lane.st();
        let summary = st
            .units
            .get(&args.name)
            .map(|rec| unit_summary(&reference, &args.name, rec))
            .unwrap_or_default();
        serde_json::to_value(summary).map_err(|e| internal("encode", e))
    }

    /// `run`: one task, streamed as `log` events unless detached.
    pub(crate) async fn run_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: RunArgs = required(value)?;
        let run = args.run;
        let (project, lane) = self.target_lane(context).await?;
        self.fresh_config(&project, &lane).await;
        let reference = lane.st().reference(&project.name());
        if let Some(env) = carries_env(context) {
            lane.st().env = Some(env.clone());
            project.st().last_env = Some(env);
        }
        // `--env` applies to this run only; the lane keeps its own overrides.
        let resolved = self.resolve_overridden(&project, &lane, &run.env).await?;
        if let Some(error) = resolved.errors.get(&format!("tasks.{}", run.task)) {
            return Err(crate::resolve::resolve_error(error));
        }
        let task = resolved.tasks.get(&run.task).ok_or_else(|| {
            RpcError::new(
                "not_found",
                json!({"reference": run.task, "candidates": resolved.tasks.keys().collect::<Vec<_>>()}),
                3,
            )
        })?;
        if let Some(prompt) = &task.confirm
            && !context.yes
        {
            return Err(confirmation(
                json!({"task": run.task, "prompt": prompt, "flag": "--yes"}),
            ));
        }
        let unit = task.launch.as_ref().map(|_| run.task.clone());
        if task.interactive && !run.terminal {
            return Err(RpcError::new(
                "terminal_required",
                json!({"task": run.task}),
                2,
            ));
        }
        let background = task.interactive || run.detach;
        if background {
            let engine = self.clone();
            let actor = context.actor.clone();
            let name = run.task.clone();
            let (task_project, task_lane) = (project.clone(), lane.clone());
            let resolved = resolved.clone();
            // Ends with the daemon or a clean or reset of the lane; `stop` of the
            // task's unit ends it otherwise.
            let (adhoc, cancel) = lane.enter_adhoc(&self.shutdown).await;
            tokio::spawn(async move {
                let _adhoc = adhoc;
                let _ = engine
                    .run_task(
                        &task_project,
                        &task_lane,
                        &resolved,
                        &name,
                        "manual",
                        &actor,
                        None,
                        &cancel,
                        0,
                    )
                    .await;
            });
            // The client attaches to this unit, so it must exist before we answer.
            if let Some(unit) = &unit {
                for _ in 0..100 {
                    if lane
                        .st()
                        .units
                        .get(unit)
                        .is_some_and(|rec| rec.host.is_alive() || rec.host.exit_outcome().is_some())
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
            return Ok(json!({
                "task": run.task,
                "lane": reference,
                "unit": unit,
                "interactive": task.interactive,
                "detached": run.detach && !task.interactive,
            }));
        }
        // The limit fires the task's token, so `run_task` itself stops the unit,
        // finishes the row and emits the closing event before it returns. A clean
        // or reset of the lane fires it too, then waits for `adhoc` to drop.
        let (adhoc, cancel) = lane.enter_adhoc(&self.shutdown).await;
        let expired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let timer = args.timeout_ms.map(|ms| {
            let (cancel, expired) = (cancel.clone(), expired.clone());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                expired.store(true, std::sync::atomic::Ordering::SeqCst);
                cancel.cancel();
            })
        });
        let outcome = self
            .run_task(
                &project,
                &lane,
                &resolved,
                &run.task,
                "manual",
                &context.actor,
                Some(session),
                &cancel,
                0,
            )
            .await;
        drop(adhoc);
        if let Some(timer) = timer {
            timer.abort();
        }
        // A clean, reset or shutdown that fired the token is no timeout, and a task
        // that succeeded just as the limit fired still succeeded.
        let succeeded = matches!(&outcome, Ok(outcome) if outcome.code == Some(0));
        let expired = expired.load(std::sync::atomic::Ordering::SeqCst);
        if expired && !self.shutdown.is_cancelled() && !succeeded {
            return Err(RpcError::new("timeout", json!({"task": run.task}), 124));
        }
        let outcome = outcome?;
        let mut result = json!({
            "task": run.task,
            "lane": reference,
            "unit": unit,
            "interactive": false,
            "exit_code": outcome.code,
            "state": if outcome.code == Some(0) { "exited" } else { "failed" },
        });
        // Daemons the task started (`pg_ctl start`) outlive it on purpose; say so.
        if !outcome.leftovers.is_empty() {
            result["leftovers"] = json!(outcome.leftovers);
        }
        Ok(result)
    }

    /// `profile.list`: the profiles of the lane's config, for pickers. The lane's
    /// current choice is `LaneSummary.profiles`.
    pub(crate) async fn profile_list(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let (project, lane) = self.target_lane(context).await?;
        let st = lane.st();
        let reference = st.reference(&project.name());
        let config = match &st.config {
            crate::model::ConfigState::Loaded(config) => config,
            crate::model::ConfigState::Invalid(error) => return Err(error.clone()),
            crate::model::ConfigState::Missing => {
                return Err(RpcError::new("no_config", json!({"lane": reference}), 1));
            }
        };
        let items = devlanes_resolve::profile_summaries(config);
        Ok(json!({"lane": reference, "items": items}))
    }

    /// `task.list`: tasks plus the `run_script` sources the config references.
    pub(crate) async fn task_list(self: &Arc<Self>, context: &Context) -> Result<Value, RpcError> {
        let (project, lane) = self.target_lane(context).await?;
        let (config, path) = {
            let st = lane.st();
            let config = st
                .config
                .loaded()
                .cloned()
                .ok_or_else(|| RpcError::new("no_config", json!({"path": st.path}), 1))?;
            (config, st.path.join(&project.config_dir))
        };
        let mut items = Vec::new();
        let mut sources: BTreeSet<String> = BTreeSet::new();
        for (name, task) in &config.tasks {
            if let Some(script) = &task.run_script
                && let Ok((source, _)) = script.source_and_name()
            {
                sources.insert(source.to_owned());
            }
            items.push(json!({
                "name": name,
                "when": task.when,
                "after": task.after,
                "profiles": task.profiles,
                "steps": task.steps,
                "confirm": task.confirm.is_some(),
                "interactive": matches!(task.interactive, Some(devlanes_config::Templated::Literal(true))),
                "run_script": task.run_script.as_ref().and_then(|s| s.source_and_name().ok()).map(|(source, name)| json!({"source": source, "name": name})),
            }));
        }
        for service in config.services.values() {
            if let Some(script) = &service.run_script
                && let Ok((source, _)) = script.source_and_name()
            {
                sources.insert(source.to_owned());
            }
        }
        let sources: Vec<Value> = sources
            .into_iter()
            .map(|source| {
                let file = path.join(&source);
                let runner = self
                    .integ
                    .script_runners
                    .iter()
                    .find(|runner| runner.matches(&file))
                    .map(|runner| runner.id());
                json!({"source": source, "runner": runner})
            })
            .collect();
        Ok(json!({"items": items, "sources": sources, "truncated": false, "next_cursor": null}))
    }

    /// `unlock`: stores secret values for the lane's worktree (memory only).
    pub(crate) async fn unlock_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: UnlockParams = params(value)?;
        let (project, lane) = self.target_lane(context).await?;
        let (path, project_profile) = {
            let st = lane.st();
            let profile = st
                .config
                .loaded()
                .and_then(|config| config.fnox.as_ref())
                .and_then(|fnox| fnox.profile.as_ref())
                .and_then(|profile| profile.evaluate(&ExpressionContext::default()).ok());
            (st.path.clone(), profile)
        };
        let remove: BTreeSet<String> = args.remove.into_iter().collect();
        let keys = self.secrets.unlock(
            path.clone(),
            args.profile.clone(),
            args.env.clone(),
            remove.clone(),
        );
        // The CLI's default profile is the project's fnox profile.
        if args.profile.is_none() && project_profile.is_some() {
            self.secrets
                .unlock(path, project_profile.clone(), args.env, remove);
        }
        let reference = lane.st().reference(&project.name());
        lane.st().last_error = None;
        self.emit_lane(&project, &lane);
        Ok(json!({"lane": reference, "profile": args.profile.or(project_profile), "keys": keys}))
    }

    /// `daemon.reset-state` (requires `yes`): stops everything and empties state.db.
    pub(crate) async fn reset_state(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        if !context.yes {
            return Err(confirmation(
                json!({"processes": self.all_lanes().iter().map(|(_, l)| l.st().units.values().filter(|u| u.host.is_alive()).count()).sum::<usize>(), "flag": "--yes"}),
            ));
        }
        // Every lane stays locked until the reset is done, so no start spawns a
        // unit after the stop and before the records it would need are gone.
        let lanes = self.all_lanes();
        let _ops = self.lock_lanes(&lanes).await;
        // `exec` and `run` take no `op`: interrupt runs and wait for spawns in
        // flight, so the stop below sees every unit.
        let mut closed = Vec::with_capacity(lanes.len());
        for (_, lane) in &lanes {
            closed.push(lane.close_adhoc().await);
        }
        let stopped = Self::stop_lanes_locked(&lanes).await;
        // The reset would erase the only record of orphans (of known and unknown
        // lanes) and their escaped descendants, so they are terminated first.
        let terminated = self.end_recorded_runs().await?;
        for project in self.all_projects() {
            project.watch.cancel();
        }
        // A refused reset leaves the loaded projects in place.
        self.store.reset().await.map_err(store_error)?;
        // A start waiting for one of these lanes finds it unassigned once it runs.
        for (_, lane) in &lanes {
            let mut st = lane.st();
            st.record = None;
            st.units.clear();
            st.orphans.clear();
        }
        lock(&self.projects).clear();
        self.secrets.forget(None);
        self.publish_routes();
        self.bus
            .emit(events::RESYNC, json!({"reason": "reset_state"}));
        Ok(json!({"stopped": stopped, "terminated": terminated}))
    }

    /// Spec with ports for the request's lane, allocating infra ports first.
    async fn infra_for(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<(LanePair, crate::integrations::InfraSpec), RpcError> {
        let (project, lane) = self.target_lane(context).await?;
        let spec = self.infra_spec(&project, &lane, &[]).await?;
        let needs = self
            .integ
            .infra
            .port_needs(&spec)
            .await
            .map_err(RpcError::from)?;
        self.allocate_lane(&project, &lane, &needs).await?;
        let spec = self.infra_spec(&project, &lane, &needs).await?;
        Ok(((project, lane), spec))
    }

    /// `infra.up` for the request's lane (shared plus per-lane containers).
    pub(crate) async fn infra_up_method(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let ((project, lane), spec) = self.infra_for(context).await?;
        let _op = lane.op.lock().await;
        let up = self.integ.infra.up(&spec).await.map_err(RpcError::from)?;
        {
            let mut st = lane.st();
            st.infra_ns = Some(up.namespace.clone());
            st.infra_containers = up.containers.clone();
            st.setup_env = up.setup_env.clone();
        }
        self.refresh_views_held(&project, &lane).await;
        Ok(
            json!({"lane": lane.st().reference(&project.name()), "infra": up.report, "endpoints": up.namespace}),
        )
    }

    /// `infra.down`: stops shared containers (data kept). Refused while lanes run
    /// unless confirmed, because shared infra serves every lane of the project.
    pub(crate) async fn infra_down_method(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let ((project, _), spec) = self.infra_for(context).await?;
        let running: Vec<String> = project
            .lanes()
            .iter()
            .filter(|lane| lane.st().any_running())
            .map(|lane| lane.st().reference(&project.name()))
            .collect();
        if !running.is_empty() && !context.yes {
            return Err(confirmation(
                json!({"lanes_running": running, "flag": "--yes"}),
            ));
        }
        self.integ.infra.down(&spec).await.map_err(RpcError::from)
    }

    /// `infra.status` for the request's lane.
    pub(crate) async fn infra_status_method(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let ((project, lane), spec) = self.infra_for(context).await?;
        let status = self
            .integ
            .infra
            .status(&spec)
            .await
            .map_err(RpcError::from)?;
        Ok(json!({"lane": lane.st().reference(&project.name()), "infra": status}))
    }

    /// `config.check` on the cwd's config, with preset option validation.
    pub(crate) async fn config_check(
        self: &Arc<Self>,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let Some(file) = devlanes_git::find_config(&context.cwd)
            .await
            .map_err(|error| internal("io", error))?
        else {
            return Err(crate::engine::no_config(&context.cwd).await);
        };
        // The lane's warnings below come from its loaded config; bring it up to date.
        if let Ok((project, lane)) = self.target_lane(context).await {
            self.fresh_config(&project, &lane).await;
        }
        let checks = self.config_checks();
        let known = crate::platforms::known(&self.integ.contributors);
        let path = file.clone();
        let shell = self.integ.shell.clone();
        let (report, unknown) = tokio::task::spawn_blocking(move || {
            let config = devlanes_config::load_project_with::<OpaqueExtensions>(&path)?;
            let report = crate::registry::check_config(&config, &checks)?;
            let mut extra = crate::platforms::unknown(&config, &known);
            // Windows without Git for Windows: run strings go to cmd.exe.
            extra.extend(
                devlanes_resolve::shell_warnings(&config, &shell)
                    .into_iter()
                    .map(
                        |w| json!({"path": w.path, "message": w.message, "code": "shell_fallback"}),
                    ),
            );
            Ok::<_, devlanes_config::ConfigError>((report, extra))
        })
        .await
        .map_err(|error| internal("config_check", error))?
        .map_err(|error| crate::model::config_error(&error))?;
        let mut warnings =
            serde_json::to_value(&report.warnings).map_err(|e| internal("encode", e))?;
        if let Some(list) = warnings.as_array_mut() {
            list.extend(unknown);
            // Hostnames need the lane's evaluated layout; a lane that cannot be
            // laid out yet (unassigned) only misses this advice.
            if let Ok((project, lane)) = self.target_lane(context).await {
                let st = lane.st();
                for warning in self.ca_warnings(&st.reference(&project.name()), &st) {
                    let data = &warning.data;
                    let path = st
                        .view
                        .as_ref()
                        .and_then(|view| {
                            view.routes
                                .iter()
                                .find(|route| route.host == data["host"])
                                .map(|route| format!("services.{}.hosts", route.service))
                        })
                        .unwrap_or_else(|| "infra.hosts".into());
                    list.push(json!({
                        "path": path,
                        "message": format!(
                            "{} is outside the names the devlanes CA can certify, so HTTPS fails there (plain HTTP works); use a name like {}",
                            data["host"].as_str().unwrap_or_default(),
                            data["suggestion"].as_str().unwrap_or_default(),
                        ),
                        "code": warning.code,
                        "host": data["host"],
                        "suggestion": data["suggestion"],
                    }));
                }
                list.extend(self.config_check_warnings(&st.reference(&project.name()), &st));
            }
        }
        Ok(json!({"project": report.project, "path": file, "warnings": warnings}))
    }
}

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use devlanes_config::{
    Env, ExpressionContext, Extensions, FnoxConfig, InfraCommand, ProfileConfig, ProfileOptions,
    ReadyConfig, Run, ServiceConfig, StopConfig, SyncConfig, TaskAfter, TaskConfig, TcpReady,
    Templated,
};
use devlanes_core::{
    StopPolicy, StopSignal,
    color::unit_color,
    contrib::{
        ContribCx, ContribMemo, Contributor, DetectCx, DraftCommand, LaunchDraft, ScriptInfo,
        ScriptPlan, ScriptRequest, ScriptRunner, SecretEnv, SecretsError, SecretsRequest, UnitKind,
    },
    env::{EnvLayer, assemble},
};
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::{
    DEFAULT_READY_TIMEOUT, Launch, Ready, ReadySpec, ResolveError, ResolveInput, ResolvedCommand,
    ResolvedLane, ResolvedService, ResolvedTask, ServiceLayout, SyncSpec,
    layout::{self, Namespaces, env_namespace, replica_value},
    select::{active_profiles, merge_options},
    util::shell_quote,
};

/// Plans every service replica, task and infra command of a lane.
///
/// Profiles come from `input.profile` followed by `input.profiles`. Their `env`
/// layers apply in that order (later wins), `run_scripts` remaps merge in that
/// order, `infra` stays on unless every profile evaluates `infra: false`, and a
/// task with `profiles:` runs when any profile in effect matches.
///
/// Fails as a whole only for lane-level problems (an unknown profile, `lanes.vars`,
/// ports, hostnames, `infra: false` or the project fnox profile). A failure in one
/// unit, including a locked vault, lands in [`ResolvedLane::errors`]; check it with
/// [`ResolvedLane::check_services`] before starting the selection.
///
/// Environment layers, lowest first: client env, contributors, secrets,
/// `DEVLANES_*`/`PORT`/`HOST` and the CA variables, `global_env`, the unit's
/// `env`, the profile's `env`, `--env` overrides. `.env` values only feed
/// `${{ env.X }}`, which reads the client env, then secrets, then `.env`.
/// Lane vars, ports and hostnames are evaluated first, as in [`crate::lane_layout`],
/// so their `${{ env.X }}` sees no secrets.
pub async fn resolve_lane<E: Extensions>(
    input: ResolveInput<'_, E>,
) -> Result<ResolvedLane, ResolveError> {
    let config = input.config;
    let mut lane = plan_lane(input)
        .await
        .map_err(|error| error.locate(config))?;
    for error in lane.errors.values_mut() {
        *error = error.clone().locate(config);
    }
    Ok(lane)
}

/// [`resolve_lane`] before errors get their source location.
async fn plan_lane<E: Extensions>(
    input: ResolveInput<'_, E>,
) -> Result<ResolvedLane, ResolveError> {
    let config = input.config;
    let requested: Vec<&str> = input
        .profile
        .into_iter()
        .chain(input.profiles.iter().map(String::as_str))
        .collect();
    let profiles = active_profiles(config, &requested)?;
    // Only the merged `run_scripts` is used from this; env layers stay per profile
    // so error paths name the profile that failed.
    let remap = merge_options(config, &profiles);
    let base_env = env_namespace(input.client_env, None, input.dotenv);
    let (layout, ns) = layout::build(&input, &base_env)?;
    let lane_context = ns.lane_context(&base_env);
    let mut profile_envs = Vec::new();
    let mut every_profile_skips_infra = !profiles.is_empty();
    let mut any_configured = false;
    for name in &profiles {
        let Some(profile) = config.profiles.get(name) else {
            continue;
        };
        any_configured = true;
        let skips = match profile {
            ProfileConfig::Object(options) => {
                profile_envs.push((format!("profiles.{name}.env"), &options.env));
                match &options.infra {
                    Some(value) => !value.evaluate(&lane_context).map_err(|e| {
                        ResolveError::expression(format!("profiles.{name}.infra"), e)
                    })?,
                    None => false,
                }
            }
            ProfileConfig::Selectors(_) => false,
        };
        every_profile_skips_infra &= skips;
    }
    let infra_enabled = !(any_configured && every_profile_skips_infra);
    let project_fnox = match config.fnox.as_ref().and_then(|f| f.profile.as_ref()) {
        Some(value) => Some(
            value
                .evaluate(&lane_context)
                .map_err(|e| ResolveError::expression("fnox.profile", e))?,
        ),
        None => None,
    };
    let mut contributors = input.contributors.to_vec();
    // Stable sort: equal priorities keep registration order.
    contributors.sort_by_key(|c| c.priority());
    let mut devlanes = BTreeMap::from([
        ("DEVLANES".to_owned(), "1".to_owned()),
        ("DEVLANES_PROJECT".to_owned(), config.project.clone()),
        ("DEVLANES_LANE".to_owned(), input.facts.index.to_string()),
        ("DEVLANES_LANE_NAME".to_owned(), input.facts.name.clone()),
        (
            crate::LANE_DATA_ENV.to_owned(),
            devlanes_core::paths::forward_slashes(&input.facts.data_path),
        ),
    ]);
    devlanes.extend(input.ca_env.iter().map(|(k, v)| (k.clone(), v.clone())));

    let mut planner = Planner {
        input: &input,
        ns: &ns,
        lane_context,
        remap: remap.as_ref(),
        profile_envs,
        contributors,
        memo: ContribMemo::default(),
        secrets: BTreeMap::new(),
        project_fnox,
        devlanes,
        origins: layout.origins.join(","),
    };

    let mut services = IndexMap::new();
    let mut errors = IndexMap::new();
    for unit in layout.services.values() {
        let key = format!("services.{}", unit.name);
        if errors.contains_key(&key) || !input.plans(&key) {
            continue;
        }
        match planner.service(unit).await {
            Ok(service) => {
                services.insert(unit.unit.clone(), service);
            }
            Err(error) => {
                services.retain(|_, s: &mut ResolvedService| s.name != unit.name);
                errors.insert(key, error);
            }
        }
    }
    // Tasks limited to `default` run when no profile is in effect.
    let default_only = ["default".to_owned()];
    let task_profiles: &[String] = if profiles.is_empty() {
        &default_only
    } else {
        &profiles
    };
    let mut tasks = IndexMap::new();
    for (name, task) in &config.tasks {
        if !input.plans(&format!("tasks.{name}")) {
            continue;
        }
        match planner.task(name, task, task_profiles).await {
            Ok(task) => {
                tasks.insert(name.clone(), task);
            }
            Err(error) => {
                errors.insert(format!("tasks.{name}"), error);
            }
        }
    }
    let mut setup = IndexMap::new();
    let mut teardown = IndexMap::new();
    for (phase, entries, target) in [
        ("setup", &config.infra.setup, &mut setup),
        ("teardown", &config.infra.teardown, &mut teardown),
    ] {
        for (name, entry) in entries {
            if !input.plans(&format!("infra.{phase}.{name}")) {
                continue;
            }
            match planner.infra_command(phase, name, entry).await {
                Ok(command) => {
                    target.insert(name.clone(), command);
                }
                Err(error) => {
                    errors.insert(format!("infra.{phase}.{name}"), error);
                }
            }
        }
    }
    let services_namespace = ns.services.clone();
    Ok(ResolvedLane {
        facts: input.facts.clone(),
        domain: layout.domain,
        port: layout.port,
        vars: layout.vars,
        profile: profiles.first().cloned(),
        profiles,
        infra_enabled,
        services,
        tasks,
        setup,
        teardown,
        routes: layout.routes,
        origins: layout.origins,
        errors,
        warnings: layout.warnings,
        lane_env: layout.lane_env,
        services_namespace,
    })
}

/// Per-lane planning state.
struct Planner<'p, 'a, E: Extensions> {
    input: &'p ResolveInput<'a, E>,
    ns: &'p Namespaces,
    /// Lane-level context without secrets, for fnox profile names.
    lane_context: ExpressionContext,
    /// Merged profile options; only `run_scripts` is read.
    remap: Option<&'p ProfileOptions>,
    /// Each profile's `env` with its config path, in the order given.
    profile_envs: Vec<(String, &'a Env)>,
    contributors: Vec<Arc<dyn Contributor>>,
    memo: ContribMemo,
    /// One provider call per (profile, explicit) per lane resolution.
    secrets: BTreeMap<(Option<String>, bool), Arc<SecretEnv>>,
    project_fnox: Option<String>,
    devlanes: BTreeMap<String, String>,
    origins: String,
}

/// What [`Planner::launch`] needs about one unit.
struct LaunchInput<'s> {
    kind: UnitKind,
    /// Config path for errors, such as `services.api`.
    path: &'s str,
    name: &'s str,
    unit: &'s str,
    run: Option<&'s Run>,
    script: Option<(String, String)>,
    cwd: Option<&'s Templated<String>>,
    env: &'s Env,
    platform: Option<&'s str>,
    preset: Option<&'s str>,
    preset_options: Option<Value>,
    layout: Option<&'s ServiceLayout>,
    pty: bool,
}

impl<E: Extensions> Planner<'_, '_, E> {
    /// Fetches secrets for a unit's fnox setting, once per profile.
    async fn unit_secrets(
        &mut self,
        fnox: Option<&FnoxConfig>,
    ) -> Result<Arc<SecretEnv>, ResolveError> {
        let (profile, explicit) = match fnox.and_then(|f| f.profile.as_ref()) {
            Some(value) => (
                Some(
                    value
                        .evaluate(&self.lane_context)
                        .map_err(|e| ResolveError::expression("fnox.profile", e))?,
                ),
                true,
            ),
            None => (
                self.project_fnox.clone(),
                fnox.is_some() || self.input.config.fnox.is_some(),
            ),
        };
        let key = (profile.clone(), explicit);
        if let Some(cached) = self.secrets.get(&key) {
            return Ok(cached.clone());
        }
        let resolved = match self.input.secrets {
            None => SecretEnv::default(),
            Some(provider) => {
                let request = SecretsRequest {
                    worktree: &self.input.facts.path,
                    profile: profile.as_deref(),
                    explicit,
                    base_env: self.input.client_env,
                };
                provider.env(&request).await.map_err(|error| match error {
                    SecretsError::Locked { message } => ResolveError::SecretsLocked {
                        profile: profile.clone(),
                        message,
                    },
                    SecretsError::Other(message) => ResolveError::Secrets {
                        profile: profile.clone(),
                        message,
                    },
                })?
            }
        };
        let resolved = Arc::new(resolved);
        self.secrets.insert(key, resolved.clone());
        Ok(resolved)
    }

    /// Context for one unit with its secrets visible to `${{ env.X }}`.
    fn context(&self, secrets: &SecretEnv, this: Value, replica: Value) -> ExpressionContext {
        let env = env_namespace(self.input.client_env, Some(&secrets.set), self.input.dotenv);
        self.ns.context(&env, this, replica)
    }

    /// Plans one service replica.
    async fn service(&mut self, layout: &ServiceLayout) -> Result<ResolvedService, ResolveError> {
        let config = self.input.config;
        let path = format!("services.{}", layout.name);
        let service: &ServiceConfig<E> = config
            .services
            .get(&layout.name)
            .ok_or_else(|| ResolveError::invalid(&path, "service disappeared from the config"))?;
        let secrets = self.unit_secrets(service.fnox.as_ref()).await?;
        let context = self.context(&secrets, layout.self_value(), replica_value(layout));
        let pty = flag(&service.pty, &context, &format!("{path}.pty"), true)?;
        let preset_options = match service
            .preset
            .as_ref()
            .and_then(|p| service.preset_options.get(p))
        {
            Some(options) => {
                let evaluated =
                    devlanes_config::evaluate_typed(options, &context).map_err(|e| {
                        ResolveError::expression(
                            format!("{path}.{}", service.preset.as_deref().unwrap_or_default()),
                            e,
                        )
                    })?;
                Some(
                    serde_json::to_value(evaluated)
                        .map_err(|e| ResolveError::invalid(&path, e.to_string()))?,
                )
            }
            None => None,
        };
        let script = match &service.run_script {
            Some(script) => {
                let (source, name) = script
                    .file_and_name_for_profile(self.remap)
                    .map_err(|e| ResolveError::expression(format!("{path}.run_script"), e))?;
                Some((source.to_owned(), name.to_owned()))
            }
            None => None,
        };
        let launch = self
            .launch(
                LaunchInput {
                    kind: UnitKind::Service,
                    path: &path,
                    name: &layout.name,
                    unit: &layout.unit,
                    run: service.run.as_ref(),
                    script,
                    cwd: service.cwd.as_ref(),
                    env: &service.env,
                    platform: service.platform.as_deref(),
                    preset: service.preset.as_deref(),
                    preset_options,
                    layout: Some(layout),
                    pty,
                },
                &context,
                &secrets,
            )
            .await?;
        let ready = ready(
            &service.effective_ready(),
            layout,
            &launch.cwd,
            &context,
            &path,
        )?;
        let stop = stop_policy(service.stop.as_ref(), &context, &path)?;
        #[cfg(windows)]
        let launch = with_windows_stop_note(launch, stop.signal);
        let sync = self.sync(service.sync.as_deref().unwrap_or_default(), &context, &path)?;
        Ok(ResolvedService {
            name: layout.name.clone(),
            unit: layout.unit.clone(),
            replica: layout.replica,
            replicas: layout.replicas,
            launch,
            port: layout.port,
            ports: layout.ports.clone(),
            url: layout.url.clone(),
            public_url: layout.public_url.clone(),
            hosts: layout.hosts.clone(),
            ready,
            stop,
            pty,
            autostart: layout.autostart,
            hidden: layout.hidden,
            tags: layout.tags.clone(),
            color: layout.color.clone(),
            sync,
            literal_port: layout.literal_port,
            preset: service.preset.clone(),
            display_name: layout.display_name.clone(),
        })
    }

    /// Plans one task; steps-only tasks get no launch.
    async fn task(
        &mut self,
        name: &str,
        task: &TaskConfig,
        profiles: &[String],
    ) -> Result<ResolvedTask, ResolveError> {
        let path = format!("tasks.{name}");
        let secrets = self.unit_secrets(task.fnox.as_ref()).await?;
        let context = self.context(&secrets, json!({"name": name}), json!({}));
        let interactive = flag(
            &task.interactive,
            &context,
            &format!("{path}.interactive"),
            false,
        )?;
        // Interactive tasks talk to a terminal; others keep stdout and stderr apart.
        let pty = flag(&task.pty, &context, &format!("{path}.pty"), interactive)?;
        let launch = if task.steps.is_some() {
            None
        } else {
            let script = match &task.run_script {
                Some(script) => {
                    let (source, name) = script
                        .file_and_name()
                        .map_err(|e| ResolveError::expression(format!("{path}.run_script"), e))?;
                    Some((source.to_owned(), name.to_owned()))
                }
                None => None,
            };
            Some(
                self.launch(
                    LaunchInput {
                        kind: UnitKind::Task,
                        path: &path,
                        name,
                        unit: name,
                        run: task.run.as_ref(),
                        script,
                        cwd: task.cwd.as_ref(),
                        env: &task.env,
                        platform: None,
                        preset: None,
                        preset_options: None,
                        layout: None,
                        pty,
                    },
                    &context,
                    &secrets,
                )
                .await?,
            )
        };
        let confirm = match &task.confirm {
            Some(text) => Some(
                text.evaluate(&context)
                    .map_err(|e| ResolveError::expression(format!("{path}.confirm"), e))?,
            ),
            None => None,
        };
        Ok(ResolvedTask {
            name: name.to_owned(),
            launch,
            steps: task.steps.clone().unwrap_or_default(),
            when: task.when,
            after_services: matches!(task.after, Some(TaskAfter::Services)),
            profiles: task.profiles.clone(),
            active: task
                .profiles
                .as_ref()
                .is_none_or(|only| only.iter().any(|p| profiles.contains(p))),
            confirm,
            interactive,
            timeout: duration(task.timeout.as_ref(), &context, &format!("{path}.timeout"))?,
            pty,
            color: unit_color(&[], name, None, &BTreeMap::new()).to_owned(),
        })
    }

    /// Plans one `infra.setup` or `infra.teardown` entry.
    async fn infra_command(
        &mut self,
        phase: &str,
        name: &str,
        entry: &InfraCommand,
    ) -> Result<ResolvedCommand, ResolveError> {
        let path = format!("infra.{phase}.{name}");
        let empty = Env::default();
        let (run, options) = match entry {
            InfraCommand::Command(run) => (Some(run), None),
            InfraCommand::Object(options) => (options.run.as_ref(), Some(options.as_ref())),
        };
        let secrets = self.unit_secrets(None).await?;
        let context = self.context(&secrets, json!({"name": name}), json!({}));
        let launch = self
            .launch(
                LaunchInput {
                    kind: UnitKind::InfraCommand,
                    path: &path,
                    name,
                    unit: &path,
                    run,
                    script: None,
                    cwd: options.and_then(|o| o.cwd.as_ref()),
                    env: options.map_or(&empty, |o| &o.env),
                    platform: None,
                    preset: None,
                    preset_options: None,
                    layout: None,
                    pty: false,
                },
                &context,
                &secrets,
            )
            .await?;
        let ignore_errors = flag(
            &options.and_then(|o| o.ignore_errors.clone()),
            &context,
            &format!("{path}.ignore_errors"),
            false,
        )?;
        Ok(ResolvedCommand {
            name: name.to_owned(),
            launch,
            container: options.and_then(|o| o.container.clone()),
            ignore_errors,
            timeout: duration(
                options.and_then(|o| o.timeout.as_ref()),
                &context,
                &format!("{path}.timeout"),
            )?,
        })
    }

    /// Evaluates the command, runs contributors and assembles the environment.
    async fn launch(
        &self,
        spec: LaunchInput<'_>,
        context: &ExpressionContext,
        secrets: &SecretEnv,
    ) -> Result<Launch, ResolveError> {
        let input = self.input;
        let facts = input.facts;
        let lane_path = facts.path.as_path();
        let path = spec.path;
        let global = env_map(&input.config.global_env, context, "global_env")?;
        let own = env_map(spec.env, context, &format!("{path}.env"))?;
        // Later profiles win over earlier ones within the profile layer.
        let mut profile = BTreeMap::new();
        for (env_path, env) in &self.profile_envs {
            profile.extend(env_map(env, context, env_path)?);
        }
        let cwd = match spec.cwd {
            Some(value) => lane_path.join(
                value
                    .evaluate(context)
                    .map_err(|e| ResolveError::expression(format!("{path}.cwd"), e))?,
            ),
            None => lane_path.to_path_buf(),
        };
        let run_path = format!("{path}.run");
        let command = match (spec.run, spec.script) {
            (Some(Run::Shell(text)), _) => DraftCommand::Shell(
                text.evaluate(context)
                    .map_err(|e| ResolveError::expression(&run_path, e))?,
            ),
            (Some(Run::Argv(items)), _) => DraftCommand::Argv(
                items
                    .iter()
                    .map(|item| item.evaluate(context))
                    .collect::<Result<_, _>>()
                    .map_err(|e| ResolveError::expression(&run_path, e))?,
            ),
            (None, Some((source, name))) => DraftCommand::Script {
                source: lane_path.join(source),
                name,
            },
            (None, None) => return Err(ResolveError::invalid(path, "needs run or run_script")),
        };
        let mut first = None;
        let mut script = None;
        if let DraftCommand::Script { source, name } = &command {
            let (runner, plan) = self.render_script(spec.unit, source, name, &[]).await?;
            script = Some(ScriptInfo {
                source: source.clone(),
                name: name.clone(),
                body: plan.body.clone(),
                cwd: plan.cwd.clone(),
                runner: runner.id(),
            });
            first = Some(plan);
        }
        let base_cwd = script
            .as_ref()
            .map_or_else(|| cwd.clone(), |s| s.cwd.clone());
        let platform = match spec.platform {
            Some(platform) => Some(platform.to_owned()),
            None => {
                let detect = DetectCx {
                    kind: spec.kind,
                    name: spec.name,
                    lane_path,
                    cwd: &base_cwd,
                    script: script.as_ref(),
                };
                detect_platform(&self.contributors, &detect).await
            }
        };
        let no_ports = BTreeMap::new();
        let cx = ContribCx {
            kind: spec.kind,
            project: &input.config.project,
            name: spec.name,
            replica: spec.layout.map(|l| l.replica),
            lane_index: facts.index,
            lane_name: &facts.name,
            lane_path,
            lane_data_path: &facts.data_path,
            primary_path: &facts.primary_path,
            platform: platform.as_deref(),
            preset: spec.preset,
            preset_options: spec.preset_options.as_ref(),
            port: spec.layout.map(|l| l.port),
            ports: spec.layout.map_or(&no_ports, |l| &l.ports),
            public_url: spec.layout.and_then(|l| l.public_url.as_deref()),
            pty: spec.pty,
            script: script.as_ref(),
            client_env: input.client_env,
            ca_env: input.ca_env,
            memo: &self.memo,
        };
        let mut draft = LaunchDraft {
            command: command.clone(),
            args: Vec::new(),
            cwd: base_cwd,
            env: BTreeMap::new(),
            remove: BTreeSet::new(),
            files: Vec::new(),
            notes: Vec::new(),
        };
        let mut applied = Vec::new();
        for contributor in &self.contributors {
            contributor
                .apply(&mut draft, &cx)
                .await
                .map_err(|e| ResolveError::Contributor {
                    unit: spec.unit.to_owned(),
                    id: contributor.id().to_owned(),
                    message: e.to_string(),
                })?;
            applied.push(contributor.id());
        }
        let LaunchDraft {
            command: drafted,
            args,
            cwd: drafted_cwd,
            env: contributed,
            remove,
            files,
            mut notes,
        } = draft;
        let (argv, cwd) = match drafted {
            DraftCommand::Shell(text) => {
                let text = if args.is_empty() {
                    text
                } else {
                    // Appending to the last line keeps multi-line scripts valid shell.
                    let quoted: Vec<_> = args.iter().map(|a| shell_quote(a)).collect();
                    format!("{} {}", text.trim_end(), quoted.join(" "))
                };
                (
                    vec!["/bin/sh".to_owned(), "-c".to_owned(), text],
                    drafted_cwd,
                )
            }
            DraftCommand::Argv(mut argv) => {
                argv.extend(args);
                (argv, drafted_cwd)
            }
            DraftCommand::Script { source, name } => {
                let unchanged = command
                    == DraftCommand::Script {
                        source: source.clone(),
                        name: name.clone(),
                    };
                let plan = match first {
                    Some(plan) if unchanged && args.is_empty() => plan,
                    _ => {
                        self.render_script(spec.unit, &source, &name, &args)
                            .await?
                            .1
                    }
                };
                // The rendered plan's findings (how the runner was chosen, a
                // missing packageManager field) surface with the unit's notes.
                let runner = script.as_ref().map_or("run_script", |s| s.runner);
                notes.extend(plan.detail.map(|detail| format!("{runner}: {detail}")));
                notes.extend(plan.notes);
                (plan.argv, plan.cwd)
            }
        };
        let Some((program, rest)) = argv.split_first() else {
            return Err(ResolveError::invalid(run_path, "the command is empty"));
        };
        // Folded here as `assemble` folds every layer, so a removal spelled
        // `PATH` also drops a client's `Path` on Windows.
        let mut client = devlanes_core::env::fold_names(input.client_env.clone());
        for key in remove.iter().chain(&secrets.remove) {
            client.remove(&devlanes_core::env::fold_name(key.clone()));
        }
        if !secrets.files.is_empty() {
            let keys: Vec<_> = secrets.files.keys().map(String::as_str).collect();
            notes.push(format!(
                "file-backed secrets are not supported yet; skipped {}",
                keys.join(", ")
            ));
        }
        let mut devlanes = self.devlanes.clone();
        if let (UnitKind::Service, Some(layout)) = (spec.kind, spec.layout) {
            devlanes.insert("PORT".into(), layout.port.to_string());
            devlanes.insert("HOST".into(), "127.0.0.1".into());
            devlanes.insert("DEVLANES_SERVICE".into(), layout.name.clone());
            devlanes.insert("DEVLANES_ORIGINS".into(), self.origins.clone());
        }
        let env = assemble([
            (EnvLayer::Client, client),
            (EnvLayer::Contributor, contributed),
            (EnvLayer::Secrets, secrets.set.clone()),
            (EnvLayer::Devlanes, devlanes),
            (EnvLayer::Global, global),
            (EnvLayer::Service, own),
            (EnvLayer::Profile, profile),
            (EnvLayer::Override, input.overrides.clone()),
        ]);
        Ok(Launch {
            program: program.clone(),
            args: rest.to_vec(),
            cwd,
            env,
            files,
            notes,
            platform,
            script,
            contributors: applied,
        })
    }

    /// Finds the runner for `source` and renders the script.
    async fn render_script(
        &self,
        unit: &str,
        source: &Path,
        name: &str,
        extra_args: &[String],
    ) -> Result<(Arc<dyn ScriptRunner>, ScriptPlan), ResolveError> {
        let runner = self
            .input
            .script_runners
            .iter()
            .find(|r| r.matches(source))
            .ok_or_else(|| ResolveError::NoScriptRunner {
                unit: unit.to_owned(),
                file: source.display().to_string(),
            })?;
        let request = ScriptRequest {
            worktree: &self.input.facts.path,
            source,
            name,
            extra_args,
        };
        let plan = runner
            .resolve(&request)
            .await
            .map_err(|e| ResolveError::Script {
                unit: unit.to_owned(),
                message: e.to_string(),
            })?;
        Ok((runner.clone(), plan))
    }

    /// Resolves `sync` entries; `to` defaults to `~/devlanes/sync/<project>/<lane>/<dir>`.
    fn sync(
        &self,
        entries: &[SyncConfig],
        context: &ExpressionContext,
        path: &str,
    ) -> Result<Vec<SyncSpec>, ResolveError> {
        let facts = self.input.facts;
        let path = format!("{path}.sync");
        let home = || {
            self.input
                .client_env
                .get("HOME")
                .map(PathBuf::from)
                .ok_or_else(|| {
                    ResolveError::invalid(&path, "HOME is not set in the client environment")
                })
        };
        let mut specs = Vec::new();
        for entry in entries {
            let evaluate = |t: &Templated<String>| {
                t.evaluate(context)
                    .map_err(|e| ResolveError::expression(&path, e))
            };
            let from = facts.path.join(evaluate(&entry.from)?);
            let to = match &entry.to {
                Some(to) => {
                    let to = evaluate(to)?;
                    if to == "~" {
                        home()?
                    } else if let Some(rest) = to.strip_prefix("~/") {
                        home()?.join(rest)
                    } else {
                        facts.path.join(to)
                    }
                }
                None => {
                    let leaf = from.file_name().ok_or_else(|| {
                        ResolveError::invalid(&path, "`from` needs a directory name")
                    })?;
                    home()?
                        .join("devlanes/sync")
                        .join(&self.input.config.project)
                        .join(&facts.name)
                        .join(leaf)
                }
            };
            specs.push(SyncSpec {
                from,
                to,
                ignore: entry.ignore.clone(),
            });
        }
        Ok(specs)
    }
}

/// Asks every platform contributor to claim a unit without `platform` and keeps
/// the highest confidence. Ties go to the higher priority, then the smaller id,
/// so the result does not depend on registration order. Explicit `platform:`
/// values skip detection entirely.
pub async fn detect_platform(
    contributors: &[Arc<dyn Contributor>],
    cx: &DetectCx<'_>,
) -> Option<String> {
    let mut best: Option<(u8, i32, &'static str, &'static str)> = None;
    for contributor in contributors {
        let Some(platform) = contributor.platform() else {
            continue;
        };
        let Some(confidence) = contributor.detect(cx).await else {
            continue;
        };
        let claim = (
            confidence,
            contributor.priority(),
            contributor.id(),
            platform,
        );
        let wins = best.is_none_or(|(c, p, id, _)| {
            (claim.0, claim.1) > (c, p) || ((claim.0, claim.1) == (c, p) && claim.2 < id)
        });
        if wins {
            best = Some(claim);
        }
    }
    best.map(|(_, _, _, platform)| platform.to_owned())
}

/// Evaluates an env map and renders values as strings.
///
/// `${{ env.X }}` reads only the client env, secrets and `.env`, never sibling
/// keys, so `SENTRY_DSN: ${{ env.SENTRY_DSN | default('') }}` passes the shell's
/// value through. `devlanes_config::evaluate_env` would treat that as a cycle.
fn env_map(
    values: &Env,
    context: &ExpressionContext,
    path: &str,
) -> Result<BTreeMap<String, String>, ResolveError> {
    let mut result = BTreeMap::new();
    for (key, value) in values {
        let value = value
            .evaluate(context)
            .map_err(|e| ResolveError::expression(format!("{path}.{key}"), e))?;
        result.insert(key.clone(), value.to_string());
    }
    Ok(result)
}

/// Evaluates an optional boolean with a default.
fn flag(
    value: &Option<Templated<bool>>,
    context: &ExpressionContext,
    path: &str,
    default: bool,
) -> Result<bool, ResolveError> {
    value.as_ref().map_or(Ok(default), |v| {
        v.evaluate(context)
            .map_err(|e| ResolveError::expression(path, e))
    })
}

/// Evaluates an optional duration.
fn duration(
    value: Option<&Templated<devlanes_config::ConfigDuration>>,
    context: &ExpressionContext,
    path: &str,
) -> Result<Option<Duration>, ResolveError> {
    value
        .map(|v| {
            v.evaluate(context)
                .map(|d| d.0)
                .map_err(|e| ResolveError::expression(path, e))
        })
        .transpose()
}

/// Renders a `run` value as one shell command line.
fn shell_line(run: &Run, context: &ExpressionContext, path: &str) -> Result<String, ResolveError> {
    match run {
        Run::Shell(text) => text
            .evaluate(context)
            .map_err(|e| ResolveError::expression(path, e)),
        Run::Argv(items) => {
            let mut words = Vec::new();
            for item in items {
                let word = item
                    .evaluate(context)
                    .map_err(|e| ResolveError::expression(path, e))?;
                words.push(shell_quote(&word));
            }
            Ok(words.join(" "))
        }
    }
}

/// Converts `infra.containers.<name>.ready` (`path` is that key). `None` for
/// `started`, which `up` already waits for. Port names stay unresolved: the
/// container's host ports are allocated with its infra. An http-specific
/// timeout wins over the check-level one.
pub fn container_ready(
    config: &ReadyConfig,
    cwd: &Path,
    context: &ExpressionContext,
    path: &str,
) -> Result<Option<crate::ContainerReady>, ResolveError> {
    use crate::ContainerCheck;
    let ReadyConfig::Check(check) = config else {
        return Ok(None);
    };
    let mut timeout = duration(check.timeout.as_ref(), context, path)?;
    let port_name = |name: &str| (name != "default").then(|| name.to_owned());
    let spec = if let Some(tcp) = &check.tcp {
        ContainerCheck::Tcp {
            port: match tcp {
                TcpReady::Port(name) => port_name(name),
                TcpReady::Enabled(_) => None,
            },
        }
    } else if let Some(http) = &check.http {
        if let Some(own) = duration(http.timeout.as_ref(), context, path)? {
            timeout = Some(own);
        }
        let route = match &http.path {
            Some(value) => value
                .evaluate(context)
                .map_err(|e| ResolveError::expression(path, e))?,
            None => "/".to_owned(),
        };
        let status = match &http.status {
            Some(value) => Some(
                value
                    .evaluate(context)
                    .map_err(|e| ResolveError::expression(path, e))?,
            ),
            None => None,
        };
        ContainerCheck::Http {
            port: None,
            path: route,
            status,
        }
    } else if let Some(log) = &check.log {
        ContainerCheck::Log {
            pattern: log
                .evaluate(context)
                .map_err(|e| ResolveError::expression(path, e))?,
        }
    } else if let Some(cmd) = &check.cmd {
        ContainerCheck::Cmd {
            shell: shell_line(cmd, context, path)?,
            cwd: cwd.to_path_buf(),
        }
    } else {
        return Ok(None);
    };
    Ok(Some(crate::ContainerReady {
        check: spec,
        timeout: timeout.unwrap_or(DEFAULT_READY_TIMEOUT),
    }))
}

/// Converts the effective `ready` config. An http-specific timeout wins over the
/// check-level one.
fn ready(
    config: &ReadyConfig,
    layout: &ServiceLayout,
    cwd: &Path,
    context: &ExpressionContext,
    path: &str,
) -> Result<Ready, ResolveError> {
    let path = format!("{path}.ready");
    let ReadyConfig::Check(check) = config else {
        return Ok(Ready {
            check: ReadySpec::Started,
            timeout: DEFAULT_READY_TIMEOUT,
        });
    };
    let mut timeout = duration(check.timeout.as_ref(), context, &path)?;
    let spec = if let Some(tcp) = &check.tcp {
        let port = match tcp {
            TcpReady::Port(name) if name != "default" => {
                *layout
                    .ports
                    .get(name)
                    .ok_or_else(|| ResolveError::MissingOffset {
                        unit: layout.unit.clone(),
                        name: name.clone(),
                    })?
            }
            _ => layout.port,
        };
        ReadySpec::Tcp { port }
    } else if let Some(http) = &check.http {
        if let Some(own) = duration(http.timeout.as_ref(), context, &path)? {
            timeout = Some(own);
        }
        let route = match &http.path {
            Some(value) => value
                .evaluate(context)
                .map_err(|e| ResolveError::expression(&path, e))?,
            None => "/".to_owned(),
        };
        let status = match &http.status {
            Some(value) => Some(
                value
                    .evaluate(context)
                    .map_err(|e| ResolveError::expression(&path, e))?,
            ),
            None => None,
        };
        ReadySpec::Http {
            port: layout.port,
            path: route,
            status,
        }
    } else if let Some(log) = &check.log {
        ReadySpec::Log {
            pattern: log
                .evaluate(context)
                .map_err(|e| ResolveError::expression(&path, e))?,
        }
    } else if let Some(cmd) = &check.cmd {
        ReadySpec::Cmd {
            shell: shell_line(cmd, context, &path)?,
            cwd: cwd.to_path_buf(),
        }
    } else {
        ReadySpec::Started
    };
    Ok(Ready {
        check: spec,
        timeout: timeout.unwrap_or(DEFAULT_READY_TIMEOUT),
    })
}

/// Adds the `stop_signal_mapped` note when a Windows stop sends something other
/// than the configured signal. Only a `run:` string runs through the shell, whose
/// programs ignore Ctrl-C, so SIGINT is mapped for those alone.
#[cfg(windows)]
fn with_windows_stop_note(mut launch: Launch, signal: StopSignal) -> Launch {
    let through_shell =
        launch.program == "/bin/sh" && launch.args.first().is_some_and(|a| a == "-c");
    launch
        .notes
        .extend(signal.windows_launch_note(through_shell));
    launch
}

/// Converts `stop` into a policy; defaults are SIGTERM and ten seconds.
fn stop_policy(
    stop: Option<&StopConfig>,
    context: &ExpressionContext,
    path: &str,
) -> Result<StopPolicy, ResolveError> {
    let mut policy = StopPolicy::default();
    let Some(stop) = stop else {
        return Ok(policy);
    };
    if let Some(signal) = &stop.signal {
        policy.signal = StopSignal::from_config_name(signal).ok_or_else(|| {
            ResolveError::invalid(
                format!("{path}.stop.signal"),
                format!("unsupported signal `{signal}`"),
            )
        })?;
    }
    if let Some(timeout) = duration(
        stop.timeout.as_ref(),
        context,
        &format!("{path}.stop.timeout"),
    )? {
        policy.timeout = timeout;
    }
    Ok(policy)
}

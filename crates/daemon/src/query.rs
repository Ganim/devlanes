//! Read-mostly methods: status, lanes, logs, wait, errors, why, env, url, doctor
//! and subscriptions.

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use devlanes_core::env::EnvLayer;
use devlanes_proto::{Context, LaneSummary, LogLine, LogQuery, Page, RpcError, events};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::Instant;

use crate::{
    Session,
    engine::{Inner, LanePair, params, required},
    model::{ConfigState, is_mine, lane_summary},
    registry::internal,
    stream::LaneEvents,
    units::{UnitHost, UnitState, now_ms, query_log_dir},
};

/// `status` parameters.
#[derive(Debug, Default, Deserialize)]
struct StatusParams {
    #[serde(default)]
    all: bool,
    #[serde(default)]
    mine: bool,
    #[serde(default)]
    watch: bool,
}

/// `logs` parameters: proto's [`LogQuery`] plus `all` (cli `LaneLogQuery`).
#[derive(Debug, Default, Deserialize)]
struct LogsParams {
    #[serde(flatten)]
    query: LogQuery,
    #[serde(default)]
    all: bool,
}

/// `wait` parameters.
#[derive(Debug, Deserialize)]
struct WaitParams {
    target: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Stream the lane's (or unit's) events on the request while waiting.
    #[serde(default)]
    events: bool,
}

/// `errors` parameters.
#[derive(Debug, Default, Deserialize)]
struct ErrorsParams {
    #[serde(default)]
    since: Option<String>,
}

/// Parameters naming one unit.
#[derive(Debug, Default, Deserialize)]
struct UnitParams {
    #[serde(default)]
    unit: Option<String>,
}

/// `env` parameters.
#[derive(Debug, Default, Deserialize)]
struct EnvParams {
    #[serde(default)]
    unit: Option<String>,
    #[serde(default)]
    explain: bool,
    #[serde(default)]
    lane_env: bool,
}

/// `subscribe` parameters.
#[derive(Debug, Default, Deserialize)]
struct SubscribeParams {
    #[serde(default)]
    follow: Option<bool>,
}

/// Byte budget of a log query that names none (the logs crate's default).
const LOG_BUDGET: usize = 1024 * 1024;

/// Parses `5m`, `30s`, `2h`, `1d` or milliseconds into a duration.
fn parse_duration(text: &str) -> Option<Duration> {
    let text = text.trim();
    let digits: String = text.chars().take_while(char::is_ascii_digit).collect();
    let value: u64 = digits.parse().ok()?;
    let unit = &text[digits.len()..];
    let seconds = match unit {
        "" | "ms" => return Some(Duration::from_millis(value)),
        "s" => value,
        "m" => value.checked_mul(60)?,
        "h" => value.checked_mul(3600)?,
        "d" => value.checked_mul(86_400)?,
        _ => return None,
    };
    Some(Duration::from_secs(seconds))
}

/// Lowercase name of an env layer for `env --explain`.
fn layer_word(layer: EnvLayer) -> &'static str {
    match layer {
        EnvLayer::Client => "client",
        EnvLayer::Contributor => "contributor",
        EnvLayer::Devlanes => "devlanes",
        EnvLayer::Global => "global",
        EnvLayer::Service => "service",
        EnvLayer::Profile => "profile",
        EnvLayer::Override => "override",
    }
}

impl Inner {
    /// Lanes a read request covers: `all`, the context lane/project, else the cwd's
    /// project, else every project (outside any repo).
    pub(crate) async fn scope_lanes(
        self: &Arc<Self>,
        context: &Context,
        all: bool,
    ) -> Result<Vec<LanePair>, RpcError> {
        // Never registers a project: completion asks `all` without a cwd.
        if all {
            return Ok(self.all_lanes());
        }
        let cwd_project = if context.cwd.as_os_str().is_empty() {
            None
        } else {
            self.project_for_cwd(&context.cwd).await.ok().flatten()
        };
        if context.lane.is_some() {
            return Ok(vec![self.target_lane(context).await?]);
        }
        if let Some(name) = &context.project {
            let lanes: Vec<_> = self
                .all_lanes()
                .into_iter()
                .filter(|(project, _)| project.name() == *name)
                .collect();
            if lanes.is_empty() {
                return Err(RpcError::new("not_found", json!({"reference": name}), 3));
            }
            return Ok(lanes);
        }
        if let Some(project) = &cwd_project
            && let Some(lane) = Self::lane_at(project, &context.cwd)
        {
            self.claim_lane(project, &lane).await;
        }
        match cwd_project {
            Some(project) => Ok(project
                .lanes()
                .into_iter()
                .map(|lane| (project.clone(), lane))
                .collect()),
            None => Ok(self.all_lanes()),
        }
    }

    /// Lane summaries with the sampler's latest CPU and memory.
    fn summaries(
        &self,
        lanes: &[LanePair],
        mine: Option<&devlanes_proto::Actor>,
    ) -> Vec<LaneSummary> {
        let mut items = Vec::new();
        let gate = self.gate_snapshot();
        for (project, lane) in lanes {
            if !lane.st().listed() {
                continue;
            }
            let mut summary = lane_summary(&project.name(), &lane.st());
            if lane.st().record.is_some() {
                crate::model::add_shared(&mut summary, &crate::model::shared_containers(project));
            }
            self.fill_queued_with(gate.as_ref(), &mut summary);
            if let Some(caller) = mine {
                summary
                    .units
                    .retain(|unit| is_mine(&unit.actor, caller) && unit.state != "idle");
                if summary.units.is_empty() {
                    continue;
                }
            }
            self.fill_metrics(&mut summary);
            items.push(summary);
        }
        items.sort_by(|a, b| (a.project.as_str(), a.index).cmp(&(b.project.as_str(), b.index)));
        items
    }

    /// `status`, optionally streaming `lane.changed` until the client leaves.
    pub(crate) async fn status_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: StatusParams = params(value)?;
        let lanes = self.scope_lanes(context, args.all).await?;
        let mine = args.mine.then_some(&context.actor);
        let items = self.summaries(&lanes, mine);
        if args.watch {
            let scope: BTreeSet<String> = items.iter().map(|lane| lane.reference.clone()).collect();
            let everything = args.all || context.lane.is_none() && context.project.is_none();
            // The first snapshot goes to this watcher only. It is built once the
            // watcher subscribed: a change between `items` and the subscription
            // would otherwise reach the watcher in neither.
            let snapshot = |seq: u64| {
                self.summaries(&lanes, mine)
                    .iter()
                    .map(|item| devlanes_proto::Event {
                        seq,
                        event: events::LANE_CHANGED.into(),
                        data: json!({"lane": item}),
                    })
                    .collect()
            };
            self.bus
                .forward_from(session, snapshot, |event| {
                    event.event == events::LANE_CHANGED
                        && (everything
                            || event
                                .data
                                .get("lane")
                                .and_then(|l| l.get("reference"))
                                .and_then(Value::as_str)
                                .is_some_and(|r| scope.contains(r)))
                })
                .await;
        }
        serde_json::to_value(Page::complete(items)).map_err(|e| internal("encode", e))
    }

    /// `lane.list`: re-lists worktrees first so new ones show immediately.
    pub(crate) async fn lane_list(self: &Arc<Self>, context: &Context) -> Result<Value, RpcError> {
        let lanes = self.scope_lanes(context, false).await?;
        let mut projects: Vec<_> = lanes.iter().map(|(p, _)| p.clone()).collect();
        projects.dedup_by_key(|p| p.id);
        for project in &projects {
            let _ = self.relist(project).await;
        }
        let lanes = self.scope_lanes(context, false).await?;
        serde_json::to_value(Page::complete(self.summaries(&lanes, None)))
            .map_err(|e| internal("encode", e))
    }

    /// Lines of one unit from its live host or, without one, its log directory.
    async fn unit_logs(
        &self,
        pair: &LanePair,
        unit: &str,
        query: LogQuery,
    ) -> Result<Page<LogLine>, RpcError> {
        let (project, lane) = pair;
        let host = lane.st().units.get(unit).map(|rec| rec.host.clone());
        match host {
            // A closed run's snapshot misses notes written after it, such as a
            // failed respawn's `[devlanes]` line; the directory has them.
            Some(host) if !host.log_closed() => host.logs(query).await,
            _ => {
                let Some(index) = lane.st().index() else {
                    return Ok(Page::complete(Vec::new()));
                };
                let dir =
                    crate::lifecycle::unit_log_dir(&self.opts.logs, &project.name(), index, unit);
                query_log_dir(dir, unit.to_owned(), query).await
            }
        }
    }

    /// `logs`: one unit as a page; several (or `all`) grouped by unit; `follow` streams.
    pub(crate) async fn logs_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: LogsParams = params(value)?;
        let mut query = args.query;
        let (pair, selectors) = if query.units.is_empty() {
            (self.target_lane(context).await?, Vec::new())
        } else {
            self.lane_for_units(&query.units, context).await?
        };
        let (project, lane) = &pair;
        let reference = lane.st().reference(&project.name());
        if query.since.as_deref() == Some("start") {
            let started = lane.st().started_at_ms.unwrap_or(0);
            query.since = Some(started.to_string());
        }
        let candidates: Vec<(String, Vec<String>)> = {
            let st = lane.st();
            let mut names: Vec<(String, Vec<String>)> = st
                .units
                .iter()
                .filter(|(_, rec)| !query.mine || is_mine(&rec.host.info().actor, &context.actor))
                .map(|(name, rec)| (name.clone(), rec.tags.clone()))
                .collect();
            if let Some(view) = &st.view
                && !query.mine
            {
                for service in &view.services {
                    if !names.iter().any(|(n, _)| *n == service.unit) {
                        names.push((service.unit.clone(), service.tags.clone()));
                    }
                }
            }
            names
        };
        let units = if args.all || selectors.is_empty() {
            if !args.all && selectors.is_empty() {
                return Err(RpcError::new(
                    "units_required",
                    json!({"lane": reference}),
                    2,
                ));
            }
            candidates.iter().map(|(name, _)| name.clone()).collect()
        } else {
            crate::engine::match_units(&selectors, &candidates, &reference)?
        };
        query.units = Vec::new();
        query.mine = false;
        let follow = std::mem::take(&mut query.follow);
        // One response carries every unit's page, so they share the byte budget.
        let mut each = query.clone();
        if units.len() > 1 {
            let budget = query.max_bytes.unwrap_or(LOG_BUDGET);
            each.max_bytes = Some((budget / units.len()).max(1));
        }
        if args.all {
            let mut groups = Vec::new();
            for unit in &units {
                let page = self.unit_logs(&pair, unit, each.clone()).await?;
                groups
                    .push(json!({"unit": unit, "lines": page.items, "truncated": page.truncated}));
            }
            return Ok(json!({"lane": reference, "units": groups}));
        }
        let page = if let [unit] = units.as_slice() {
            self.unit_logs(&pair, unit, query.clone()).await?
        } else {
            let mut items = Vec::new();
            let mut truncated = false;
            for unit in &units {
                let page = self.unit_logs(&pair, unit, each.clone()).await?;
                truncated |= page.truncated;
                items.extend(page.items);
            }
            items.sort_by_key(|line| (line.timestamp_ms, line.seq));
            // `head` is the first N of the merged range, not N per unit.
            if let Some(head) = query.head
                && items.len() > head
            {
                items.truncate(head);
                truncated = true;
            }
            Page {
                items,
                truncated,
                next_cursor: None,
            }
        };
        if !follow {
            return serde_json::to_value(page).map_err(|e| internal("encode", e));
        }
        for line in &page.items {
            let event = devlanes_proto::Event {
                seq: 0,
                event: events::LOG.into(),
                data: json!({"unit": line.unit, "line": line}),
            };
            if session.event(event).await.is_err() {
                return Ok(json!({"items": [], "truncated": false, "next_cursor": null}));
            }
        }
        self.follow_logs(lane.clone(), units, page.items, session)
            .await;
        Ok(json!({"items": [], "truncated": false, "next_cursor": null}))
    }

    /// Streams new lines of `units` until the client disconnects, following restarts.
    async fn follow_logs(
        &self,
        lane: Arc<crate::model::LaneRt>,
        units: Vec<String>,
        seen: Vec<LogLine>,
        session: &Session,
    ) {
        let mut cursors: std::collections::BTreeMap<String, String> =
            std::collections::BTreeMap::new();
        for line in seen {
            cursors.insert(line.unit.clone(), format!("{}#{}", line.unit, line.seq));
        }
        let disconnected = session.disconnected();
        loop {
            for unit in &units {
                let host = lane.st().units.get(unit).map(|rec| rec.host.clone());
                let Some(host) = host else { continue };
                let query = LogQuery {
                    cursor: cursors.get(unit).cloned(),
                    max_bytes: Some(256 * 1024),
                    ..LogQuery::default()
                };
                let Ok(page) = host.logs(query).await else {
                    continue;
                };
                for line in page.items {
                    cursors.insert(unit.clone(), format!("{}#{}", line.unit, line.seq));
                    let event = devlanes_proto::Event {
                        seq: 0,
                        event: events::LOG.into(),
                        data: json!({"unit": unit, "line": line}),
                    };
                    if session.event(event).await.is_err() {
                        return;
                    }
                }
            }
            tokio::select! {
                () = disconnected.cancelled() => return,
                () = tokio::time::sleep(Duration::from_millis(250)) => {}
            }
        }
    }

    /// `wait`: returns once a unit or lane is ready or failed; `timeout` (exit 124) otherwise.
    pub(crate) async fn wait_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: WaitParams = required(value)?;
        let deadline = Instant::now()
            + args
                .timeout_ms
                .map_or(self.opts.start_timeout, Duration::from_millis);
        let as_unit = self
            .lane_for_units(std::slice::from_ref(&args.target), context)
            .await;
        // A unit added by an edit just now must be known before it is looked up.
        if let Ok(((project, lane), _)) = &as_unit {
            self.fresh_config(project, lane).await;
        }
        let unit_target = match &as_unit {
            Ok(((_, lane), selectors)) => {
                let st = lane.st();
                let selector = selectors.first().cloned().unwrap_or_default();
                let known = st.units.contains_key(&selector)
                    || st
                        .view
                        .as_ref()
                        .is_some_and(|v| v.services.iter().any(|s| s.unit == selector));
                known.then_some(selector)
            }
            Err(_) => None,
        };
        let (project, lane, unit) = match (as_unit, unit_target) {
            (Ok(((project, lane), _)), Some(unit)) => (project, lane, Some(unit)),
            _ => {
                let (project, lane) = self.resolve_lane_ref(&args.target, context).await?;
                (project, lane, None)
            }
        };
        let reference = lane.st().reference(&project.name());
        let stream = args.events.then(|| {
            let mut summary = lane_summary(&project.name(), &lane.st());
            self.fill_queued(&mut summary);
            LaneEvents::lane(project.clone(), lane.clone(), unit.clone()).with_snapshot(summary)
        });
        let disconnected = session.disconnected();
        let wait = async {
            loop {
                let (hosts, busy): (Vec<(String, UnitHost)>, bool) = {
                    let st = lane.st();
                    let hosts = match &unit {
                        Some(unit) => st
                            .units
                            .get(unit)
                            .map(|rec| vec![(unit.clone(), rec.host.clone())])
                            .unwrap_or_default(),
                        None => st
                            .units
                            .iter()
                            .filter(|(_, rec)| rec.kind == crate::model::Kind::Service)
                            .map(|(name, rec)| (name.clone(), rec.host.clone()))
                            .collect(),
                    };
                    (hosts, st.busy.is_some())
                };
                let states: Vec<UnitState> = hosts.iter().map(|(_, host)| host.state()).collect();
                let failed: Vec<usize> = states
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| matches!(s, UnitState::Failed(_)))
                    .map(|(i, _)| i)
                    .collect();
                if let Some(&first) = failed.first() {
                    let (name, host) = &hosts[first];
                    let failure = crate::lifecycle::failure_of(&reference, name, host).await;
                    let mut answer = json!({
                        "lane": reference, "unit": unit, "state": "failed", "ready": false,
                        "failed": failed.iter().map(|i| hosts[*i].0.clone()).collect::<Vec<_>>(),
                        "reason": failure.reason, "exit_code": failure.exit_code, "log_tail": failure.log_tail,
                    });
                    if !failure.log_tail_styled.is_empty() {
                        answer["log_tail_styled"] = json!(failure.log_tail_styled);
                    }
                    return Ok(answer);
                }
                let all_ready =
                    !states.is_empty() && states.iter().all(|s| matches!(s, UnitState::Ready));
                if all_ready && !busy {
                    return Ok(
                        json!({"lane": reference, "unit": unit, "state": "ready", "ready": true}),
                    );
                }
                if hosts.is_empty() && !busy {
                    return Ok(
                        json!({"lane": reference, "unit": unit, "state": "idle", "ready": false}),
                    );
                }
                if let Some((name, host)) = hosts
                    .iter()
                    .find(|(_, h)| matches!(h.state(), UnitState::Exited(_)))
                    .filter(|_| unit.is_some())
                {
                    return Ok(
                        json!({"lane": reference, "unit": name, "state": host.state().word(), "ready": false, "exit_code": host.exit_outcome().and_then(|o| o.code)}),
                    );
                }
                if Instant::now() >= deadline {
                    return Err(RpcError::new(
                        "timeout",
                        json!({"lane": reference, "unit": unit}),
                        124,
                    ));
                }
                // Nobody is left to tell; polling on would only hold the lane's state.
                tokio::select! {
                    () = disconnected.cancelled() => {
                        return Err(RpcError::new("client_disconnected", Value::Null, 4));
                    }
                    () = tokio::time::sleep(Duration::from_millis(150)) => {}
                }
            }
        };
        self.with_lane_events(session, stream, wait).await
    }

    /// `errors`: exit codes, crash counts, last error lines and port conflicts.
    pub(crate) async fn errors_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: ErrorsParams = params(value)?;
        let since_ms = args
            .since
            .as_deref()
            .and_then(parse_duration)
            .map_or(0, |d| {
                now_ms().saturating_sub(u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
            });
        let mut items = Vec::new();
        for (project, lane) in self.scope_lanes(context, false).await? {
            let (reference, lane_id, units) = {
                let st = lane.st();
                let units: Vec<_> = st
                    .units
                    .iter()
                    .map(|(name, rec)| {
                        (
                            name.clone(),
                            rec.host.clone(),
                            (rec.failure_tail.clone(), rec.failure_tail_styled.clone()),
                            rec.ports.clone(),
                        )
                    })
                    .collect();
                (
                    st.reference(&project.name()),
                    st.record.as_ref().map(|r| r.id),
                    units,
                )
            };
            let history = match lane_id {
                Some(id) => self.store.unit_runs(id, 500).await.unwrap_or_default(),
                None => Vec::new(),
            };
            for (name, host, tail, ports) in units {
                let crashes = history
                    .iter()
                    .filter(|run| {
                        run.unit == name
                            && u64::try_from(run.started_at).unwrap_or(0) >= since_ms
                            && run.outcome.is_some_and(|o| o.code.is_some_and(|c| c != 0))
                    })
                    .count();
                let state = host.state();
                let failed = matches!(state, UnitState::Failed(_));
                let (error_lines, error_lines_styled) = if failed {
                    tail
                } else {
                    let query = LogQuery {
                        tail: Some(10),
                        level: Some("error".into()),
                        since: (since_ms > 0).then(|| since_ms.to_string()),
                        styled: true,
                        ..LogQuery::default()
                    };
                    devlanes_proto::split_styled(
                        host.logs(query)
                            .await
                            .map(|page| {
                                page.items
                                    .into_iter()
                                    .filter(|line| host.of_this_run(line))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    )
                };
                let mut conflicts = Vec::new();
                if failed {
                    for port in ports.values() {
                        if crate::ready::tcp_open(*port).await {
                            let pid = self.integ.ports.holder(*port).await;
                            if pid.is_some_and(|pid| Some(pid) != host.identity().map(|i| i.pid))
                                || !host.is_alive()
                            {
                                conflicts.push(json!({"port": port, "pid": pid}));
                            }
                        }
                    }
                }
                if crashes == 0 && !failed && error_lines.is_empty() {
                    continue;
                }
                let reason = match &state {
                    UnitState::Failed(reason) => reason.clone(),
                    _ => None,
                };
                items.push(json!({
                    "lane": reference,
                    "unit": name,
                    "state": state.word(),
                    "exit_code": host.exit_outcome().and_then(|o| o.code),
                    "crashes": crashes,
                    "reason": reason,
                    "error_lines": error_lines,
                    "port_conflicts": conflicts,
                }));
                // Colors only when some line had them, as `log_tail_styled`.
                if !error_lines_styled.is_empty()
                    && let Some(item) = items.last_mut()
                {
                    item["error_lines_styled"] = json!(error_lines_styled);
                }
            }
        }
        Ok(json!({"items": items, "truncated": false, "next_cursor": null}))
    }

    /// `why`: reason codes explaining a unit's state.
    pub(crate) async fn why_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: UnitParams = params(value)?;
        let unit = args
            .unit
            .ok_or_else(|| RpcError::new("units_required", json!({}), 2))?;
        let ((project, lane), selectors) = self
            .lane_for_units(std::slice::from_ref(&unit), context)
            .await?;
        let name = selectors.into_iter().next().unwrap_or_default();
        let reference = lane.st().reference(&project.name());
        let mut reasons: Vec<Value> = Vec::new();
        let push = |reasons: &mut Vec<Value>, code: &str, data: Value| {
            reasons.push(json!({"code": code, "data": data}))
        };
        let (host, service, st_reason, needs_env, config, busy, unassigned, orphan) = {
            let st = lane.st();
            let orphan = st.orphan_error(&project.name(), &name);
            let host = st.units.get(&name).map(|rec| rec.host.clone());
            let service = st
                .view
                .as_ref()
                .and_then(|v| v.services.iter().find(|s| s.unit == name).cloned());
            (
                host,
                service,
                st.last_error.clone(),
                st.env.is_none(),
                st.config.clone(),
                st.busy,
                st.unassigned.clone(),
                orphan,
            )
        };
        // Left running by a previous daemon at `started_at_ms`, found at
        // `orphaned_since_ms`, pid `pid`; `command` stops it.
        if let Some(orphan) = orphan {
            push(&mut reasons, "orphaned", orphan.data);
            return Ok(json!({
                "lane": reference,
                "unit": name,
                "state": devlanes_proto::ORPHANED,
                "reasons": reasons,
            }));
        }
        if let Some(reason) = unassigned {
            push(&mut reasons, "unassigned", reason.data);
        }
        match &config {
            ConfigState::Missing => push(&mut reasons, "no_config", json!({})),
            ConfigState::Invalid(error) => push(&mut reasons, "config_error", error.data.clone()),
            ConfigState::Loaded(_) => {}
        }
        if host.is_none() && service.is_none() && reasons.is_empty() {
            return Err(RpcError::new("not_found", json!({"reference": unit}), 3));
        }
        let state = host.as_ref().map_or("idle", |h| h.state().word());
        if let Some(host) = &host {
            match host.state() {
                UnitState::Failed(Some(reason)) => push(&mut reasons, &reason.code, reason.data),
                UnitState::Starting => push(&mut reasons, "starting", json!({})),
                UnitState::Exited(outcome) => push(
                    &mut reasons,
                    "stopped",
                    json!({"exit_code": outcome.code, "signal": outcome.signal, "requested": host.stop_was_requested()}),
                ),
                _ => {}
            }
        }
        let alive = host.as_ref().is_some_and(UnitHost::is_alive);
        if !alive && let Some(service) = &service {
            if needs_env {
                push(&mut reasons, "needs_env", json!({"lane": reference}));
            }
            if let Some(error) = st_reason.filter(|e| e.code == "config_error") {
                push(&mut reasons, &error.code, error.data);
            }
            if let Some((profiles, selected)) = Inner::profile_services(&lane)
                && !selected.contains(&service.name)
            {
                if !service.autostart {
                    push(&mut reasons, "autostart_off", json!({}));
                } else {
                    push(
                        &mut reasons,
                        "not_in_profile",
                        json!({"profile": profiles.first(), "profiles": profiles}),
                    );
                }
            }
            if crate::ready::tcp_open(service.port).await {
                let pid = self.integ.ports.holder(service.port).await;
                push(
                    &mut reasons,
                    "port_in_use",
                    json!({"port": service.port, "pid": pid}),
                );
            }
        }
        if busy == Some("starting")
            && let Some(config) = config.loaded()
            && let Some(task) = config.tasks.get(&name)
            && task.after.is_some()
        {
            push(&mut reasons, "waiting_after_services", json!({}));
        }
        if let Some((code, data)) = self.hosts_reason(&reference, &name) {
            push(&mut reasons, code, data);
        }
        let state = match self.queued_reason(&reference, &name) {
            Some(data) => {
                push(&mut reasons, "queued", data);
                "queued"
            }
            None => state,
        };
        let mut result =
            json!({"lane": reference, "unit": name, "state": state, "reasons": reasons});
        // What the last run left behind: kept daemons, or processes that escaped.
        let leftovers = lane.st().units.get(&name).map(|rec| rec.leftovers(&name));
        if let Some(leftovers) = leftovers.filter(|l| !l.is_empty()) {
            if !leftovers.left_running.is_empty() {
                result["left_running"] = json!(leftovers.left_running);
            }
            if !leftovers.escaped.is_empty() {
                result["escaped"] = json!(leftovers.escaped);
            }
        }
        Ok(result)
    }

    /// `env`: lane env (`lane_env: true` or no unit) or one unit's env, optionally
    /// with provenance.
    pub(crate) async fn env_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: EnvParams = params(value)?;
        if args.lane_env || args.unit.is_none() {
            let (project, lane) = self.target_lane(context).await?;
            if lane.st().view.is_none() {
                self.refresh_views(&project).await;
            }
            let st = lane.st();
            let env = st
                .view
                .as_ref()
                .map(|view| view.lane_env.clone())
                .ok_or_else(|| {
                    st.last_error.clone().unwrap_or_else(|| {
                        RpcError::new(
                            "unassigned",
                            json!({"lane": st.reference(&project.name())}),
                            1,
                        )
                    })
                })?;
            return Ok(json!({"lane": st.reference(&project.name()), "env": env}));
        }
        let unit = args.unit.unwrap_or_default();
        let ((project, lane), selectors) = self
            .lane_for_units(std::slice::from_ref(&unit), context)
            .await?;
        let name = selectors.into_iter().next().unwrap_or_default();
        let reference = lane.st().reference(&project.name());
        let live = lane
            .st()
            .units
            .get(&name)
            .filter(|rec| rec.host.is_alive())
            .map(|rec| rec.env.clone());
        let env = match live {
            Some(env) => env,
            None => {
                let resolved = self
                    .resolve_with(&project, &lane, crate::engine::carries_env(context))
                    .await?;
                if let Some(error) = resolved
                    .errors
                    .get(&format!("services.{name}"))
                    .or_else(|| resolved.errors.get(&format!("tasks.{name}")))
                {
                    return Err(crate::resolve::resolve_error(error));
                }
                let launch = resolved
                    .services
                    .get(&name)
                    .map(|s| &s.launch)
                    .or_else(|| resolved.tasks.get(&name).and_then(|t| t.launch.as_ref()))
                    .ok_or_else(|| {
                        // Units with an environment of their own: services, then
                        // tasks that launch something.
                        let mut candidates: Vec<&String> = resolved.services.keys().collect();
                        candidates.extend(
                            resolved
                                .tasks
                                .iter()
                                .filter(|(_, task)| task.launch.is_some())
                                .map(|(name, _)| name),
                        );
                        RpcError::new(
                            "not_found",
                            json!({"reference": unit, "candidates": candidates}),
                            3,
                        )
                    })?;
                Arc::new(crate::lifecycle::copy_env(&launch.env))
            }
        };
        if args.explain {
            let items: Vec<Value> = env
                .values
                .iter()
                .map(|(key, value)| {
                    let layer = env.sources.get(key).copied().unwrap_or(EnvLayer::Client);
                    json!({"key": key, "value": value, "layer": layer_word(layer)})
                })
                .collect();
            return Ok(
                json!({"lane": reference, "unit": name, "items": items, "truncated": false, "next_cursor": null}),
            );
        }
        Ok(json!({"lane": reference, "unit": name, "env": env.values}))
    }

    /// `url`: the public URL of a service (internal URL without hosts).
    pub(crate) async fn url_method(
        self: &Arc<Self>,
        value: Value,
        context: &Context,
    ) -> Result<Value, RpcError> {
        let args: UnitParams = params(value)?;
        let unit = args
            .unit
            .ok_or_else(|| RpcError::new("units_required", json!({}), 2))?;
        let ((project, lane), selectors) = self
            .lane_for_units(std::slice::from_ref(&unit), context)
            .await?;
        let name = selectors.into_iter().next().unwrap_or_default();
        let st = lane.st();
        let service = st
            .view
            .as_ref()
            .and_then(|view| {
                view.services
                    .iter()
                    .find(|s| s.unit == name || s.name == name)
            })
            .ok_or_else(|| RpcError::new("not_found", json!({"reference": unit}), 3))?;
        let mut urls = vec![service.url.clone()];
        urls.extend(service.public_url.clone());
        Ok(json!({
            "lane": st.reference(&project.name()),
            "unit": service.unit,
            "url": service.public_url.clone().unwrap_or_else(|| service.url.clone()),
            "urls": urls,
        }))
    }

    /// `subscribe`: streams every event until disconnect; `follow: false` returns
    /// the recent events instead.
    pub(crate) async fn subscribe_method(
        self: &Arc<Self>,
        value: Value,
        session: &Session,
    ) -> Result<Value, RpcError> {
        let args: SubscribeParams = params(value)?;
        if args.follow == Some(false) {
            let items = self.bus.recent();
            return serde_json::to_value(Page::complete(items)).map_err(|e| internal("encode", e));
        }
        // A resync to this subscriber only: a status fetched right after it
        // subscribed may predate a change, so the client fetches again.
        let resync = |seq: u64| {
            vec![devlanes_proto::Event {
                seq,
                event: events::RESYNC.into(),
                data: json!({"reason": "subscribed"}),
            }]
        };
        self.bus.forward_from(session, resync, |_| true).await;
        Ok(json!({}))
    }

    /// `doctor`: daemon, state, sandbox, Docker, proxy and CA facts.
    pub(crate) async fn doctor(self: &Arc<Self>) -> Result<Value, RpcError> {
        let schema = self.store.schema_version().await.ok();
        let sandbox = json!({
            "codex_sandbox": std::env::var_os("CODEX_SANDBOX").is_some(),
            "app_sandbox": std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some(),
        });
        let mut doctor = json!({
            "version": devlanes_version::VERSION,
            "pid": std::process::id(),
            "home": self.opts.home,
            "db_path": self.opts.db_path,
            "schema_version": schema,
            "supported_schema_version": devlanes_state::SCHEMA_VERSION,
            "socket": self.opts.socket,
            "sandbox": sandbox,
            "infra": self.integ.infra.doctor().await,
            "proxy": self.proxy_doctor().await,
            "ca": self.integ.ca_status,
            // Routed hosts the CA cannot certify (`host_outside_ca`, see `certs`).
            "ca_warnings": self.all_ca_warnings(),
            // Hostnames two lanes claim and names that are not valid DNS names.
            "route_warnings": self.route_warnings(),
            "projects": self.all_projects().len(),
            "subscribers": self.bus.subscribers(),
            "hosts": self.hosts_status(),
            "dns": self.dns_doctor().await,
            "state": self.state_doctor().await,
            "user_config": self.user_config_doctor().await,
            // The CLI compares them with the service manager's stop window.
            "stop_timeouts": self.stop_timeouts(),
        });
        // An elevated daemon runs every unit elevated; `doctor` warns.
        #[cfg(windows)]
        {
            doctor["elevated"] = json!(devlanes_sys_windows::token::elevated().ok());
            doctor["reserved_ports"] = self.reserved_ports_doctor().await;
            // `run:` strings and `ready.cmd` need Git for Windows' sh.exe.
            doctor["shell"] = json!({ "path": self.integ.shell });
        }
        // Inside WSL only: worktrees on Windows drives, where file watching misses edits.
        if let Some(worktrees) = self.windows_drive_doctor().await {
            doctor["windows_drive_worktrees"] = worktrees;
        }
        Ok(doctor)
    }

    /// `doctor`'s `reserved_ports`: the Windows reserved port ranges checked
    /// against `ports.range`, the lanes' assigned ports, the proxy ports and
    /// the mirrored ports. Read through the shared cache, since the TUI polls
    /// `doctor`.
    #[cfg(windows)]
    async fn reserved_ports_doctor(&self) -> Value {
        let reading = crate::reserved_ports::RESERVED.get().await;
        let user = &self.opts.user;
        let assigned: Vec<(String, std::collections::BTreeSet<u16>)> = self
            .all_lanes()
            .into_iter()
            .filter_map(|(project, lane)| {
                let st = lane.st();
                let allocation = st.allocation.as_ref()?;
                let block = allocation.block;
                let ports = allocation
                    .offsets
                    .values()
                    .chain(&allocation.reserved)
                    .filter(|offset| **offset < block.size)
                    .filter_map(|offset| block.start.checked_add(*offset))
                    .collect();
                Some((st.reference(&project.name()), ports))
            })
            .collect();
        crate::reserved_ports::report(
            &reading,
            user.ports.bounds().ok(),
            &assigned,
            [
                ("http", user.proxy.http_port),
                ("https", user.proxy.https_port),
            ],
            &self.remote.advertised_ports(),
        )
    }

    /// `doctor`'s `stop_timeouts`: the longest stop timeout of each service of
    /// each project, `[{project, unit, timeout_ms}]`, from running services and
    /// the loaded configs. A timeout written as an expression counts once a run
    /// resolved it.
    fn stop_timeouts(&self) -> Vec<Value> {
        use devlanes_config::Templated;
        let mut longest: std::collections::BTreeMap<(String, String), Duration> =
            std::collections::BTreeMap::new();
        let mut note = |project: &str, unit: &str, timeout: Duration| {
            let entry = longest
                .entry((project.to_owned(), unit.to_owned()))
                .or_default();
            *entry = (*entry).max(timeout);
        };
        for (project, lane) in self.all_lanes() {
            let name = project.name();
            let st = lane.st();
            for (unit, rec) in &st.units {
                if rec.kind == crate::model::Kind::Service {
                    note(&name, unit, rec.stop.timeout);
                }
            }
            let Some(config) = st.config.loaded() else {
                continue;
            };
            for (service, service_config) in &config.services {
                let written = service_config
                    .stop
                    .as_ref()
                    .and_then(|stop| stop.timeout.as_ref());
                let timeout = match written {
                    None => devlanes_core::StopPolicy::default().timeout,
                    Some(Templated::Literal(timeout)) => timeout.0,
                    Some(Templated::Expression(_)) => continue,
                };
                note(&name, service, timeout);
            }
        }
        longest
            .into_iter()
            .map(|((project, unit), timeout)| {
                json!({
                    "project": project,
                    "unit": unit,
                    "timeout_ms": u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                })
            })
            .collect()
    }

    /// The proxy's listener status; a listener that fell back from its
    /// configured port (`requested_port`) names what holds that port now as
    /// `requested_holder: {pid, command}`.
    async fn proxy_doctor(&self) -> Value {
        let mut status = self.integ.routes.status();
        for listener in ["http", "https"] {
            let Some(entry) = status.get_mut(listener) else {
                continue;
            };
            let requested = entry
                .get("requested_port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok());
            let bound = entry
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok());
            let Some(requested) = requested.filter(|requested| Some(*requested) != bound) else {
                continue;
            };
            let Some(pid) = self.integ.ports.holder(requested).await else {
                continue;
            };
            let command = tokio::task::spawn_blocking(move || {
                devlanes_sys::process(pid)
                    .ok()
                    .flatten()
                    .map(|info| info.name)
            })
            .await
            .ok()
            .flatten();
            entry["requested_holder"] = json!({"pid": pid, "command": command});
        }
        status
    }

    /// `doctor`'s `user_config`: the file on disk checked now, since it may have
    /// changed since the daemon loaded it. `errors` are located; with any, the next
    /// daemon start (or `daemon restart`) refuses to run until the file is fixed.
    async fn user_config_doctor(&self) -> Value {
        let path = self.opts.home.join("config.yaml");
        let file = path.clone();
        let checked = tokio::task::spawn_blocking(move || {
            if !file.exists() {
                return None;
            }
            let (config, errors) = devlanes_config::load_user_lenient(&file);
            let errors: Vec<Value> = errors.iter().map(located).collect();
            Some((errors, config.warnings().to_vec()))
        })
        .await
        .ok()
        .flatten();
        // Without a file the loaded settings are all there is to report.
        let (errors, warnings) =
            checked.unwrap_or_else(|| (Vec::new(), self.opts.user.warnings().to_vec()));
        json!({
            "path": path,
            "valid": errors.is_empty(),
            // The first error alone, for clients that show one.
            "error": errors.first(),
            "errors": errors,
            "warnings": warnings,
        })
    }
}

/// A config error as `{message, file, line, column}`.
pub(crate) fn located(error: &devlanes_config::ConfigError) -> Value {
    json!({
        "message": error.message,
        "file": error.location.file,
        "line": error.location.line,
        "column": error.location.column,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// `errors --since` accepts the CLI's duration forms.
    fn durations() {
        assert_eq!(parse_duration("5m"), Some(Duration::from_secs(300)));
        assert_eq!(parse_duration("250"), Some(Duration::from_millis(250)));
        assert_eq!(parse_duration("2h"), Some(Duration::from_secs(7200)));
        assert_eq!(parse_duration("x"), None);
        // Overflow is no duration rather than a panic (debug) or a wrap (release).
        assert_eq!(parse_duration(&format!("{}d", u64::MAX)), None);
    }
}

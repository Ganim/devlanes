//! Registry and process-history queries run only by the store actor.

use crate::{
    models::*,
    schema::{
        lane_setup, lanes, port_blocks, projects, task_completions, task_runs, unit_run_members,
        unit_runs, worktrees,
    },
    *,
};
use devlanes_core::{
    ExitOutcome, LaneId, ProcessIdentity, ProjectId, UnitRunId, WorktreeId,
    refs::valid_project_name,
};
use diesel::{dsl::exists, prelude::*};
use std::time::{SystemTime, UNIX_EPOCH};

impl Store {
    /// Inserts by canonical identity without changing an existing project's ID.
    pub async fn upsert_project(&self, input: ProjectInput) -> Result<Project, Error> {
        if !valid_project_name(&input.name) {
            return Err(Error::Invalid("invalid project name".into()));
        }
        self.call(move |c| {
            let row = NewProject {
                identity: path_bytes(input.common_dir.as_deref().unwrap_or(&input.root_path)),
                name: input.name,
                common_dir: input.common_dir.as_deref().map(path_bytes),
                root_path: path_bytes(&input.root_path),
                primary_branch: input.primary_branch,
            };
            Ok(diesel::insert_into(projects::table)
                .values(&row)
                .on_conflict(projects::identity)
                .do_update()
                .set(&row)
                .returning(ProjectRow::as_returning())
                .get_result::<ProjectRow>(c)?
                .into())
        })
        .await
    }
    /// Finds a project by persistent ID.
    pub async fn project(&self, id: ProjectId) -> Result<Option<Project>, Error> {
        self.call(move |c| {
            Ok(projects::table
                .find(id.0)
                .select(ProjectRow::as_select())
                .first(c)
                .optional()?
                .map(Project::from))
        })
        .await
    }
    /// Lists all projects, including duplicate display names.
    pub async fn projects(&self) -> Result<Vec<Project>, Error> {
        self.call(|c| {
            Ok(projects::table
                .order(projects::id)
                .select(ProjectRow::as_select())
                .load(c)?
                .into_iter()
                .map(Project::from)
                .collect())
        })
        .await
    }
    /// Records a shell-driven start without persisting the shell environment.
    pub async fn mark_project_started(&self, id: ProjectId) -> Result<(), Error> {
        self.call(move |c| {
            require_changed(
                diesel::update(projects::table.find(id.0))
                    .set(projects::last_start.eq(now()?))
                    .execute(c)?,
                "project",
            )
        })
        .await
    }
    /// Removes project metadata after the caller has stopped its resources.
    /// Fails with [`Error::ActiveRuns`] while any of its lanes has unfinished runs,
    /// since the cascade would erase the identities needed to stop them.
    pub async fn delete_project(&self, id: ProjectId) -> Result<(), Error> {
        self.call(move |c| {
            c.immediate_transaction(|c| {
                let lanes: Vec<i64> = lanes::table
                    .filter(lanes::project_id.eq(id.0))
                    .select(lanes::id)
                    .load(c)?;
                require_no_active_runs(c, Some(&lanes))?;
                require_changed(
                    diesel::delete(projects::table.find(id.0)).execute(c)?,
                    "project",
                )
            })
        })
        .await
    }
    /// Updates by Git administrative identity so worktree moves preserve the ID.
    pub async fn upsert_worktree(&self, input: WorktreeInput) -> Result<Worktree, Error> {
        if input.identity.is_empty() {
            return Err(Error::Invalid("empty worktree identity".into()));
        }
        self.call(move |c| {
            let timestamp = now()?;
            let row = NewWorktree {
                project_id: input.project_id.0,
                identity: input.identity,
                path: path_bytes(&input.path),
                branch: input.branch,
                present: i32::from(input.present),
                first_seen: timestamp,
                last_seen: timestamp,
            };
            Ok(diesel::insert_into(worktrees::table)
                .values(&row)
                .on_conflict((worktrees::project_id, worktrees::identity))
                .do_update()
                .set((
                    worktrees::path.eq(&row.path),
                    worktrees::branch.eq(&row.branch),
                    worktrees::present.eq(row.present),
                    worktrees::last_seen.eq(timestamp),
                ))
                .returning(WorktreeRow::as_returning())
                .get_result::<WorktreeRow>(c)?
                .into())
        })
        .await
    }
    /// Lists registered worktrees, including missing paths.
    pub async fn worktrees(&self, project: ProjectId) -> Result<Vec<Worktree>, Error> {
        self.call(move |c| {
            Ok(worktrees::table
                .filter(worktrees::project_id.eq(project.0))
                .order(worktrees::id)
                .select(WorktreeRow::as_select())
                .load(c)?
                .into_iter()
                .map(Worktree::from)
                .collect())
        })
        .await
    }
    /// Finds a worktree by persistent ID.
    pub async fn worktree(&self, id: WorktreeId) -> Result<Option<Worktree>, Error> {
        self.call(move |c| {
            Ok(worktrees::table
                .find(id.0)
                .select(WorktreeRow::as_select())
                .first(c)
                .optional()?
                .map(Worktree::from))
        })
        .await
    }
    /// Records absence without deleting data or allocations.
    pub async fn mark_worktree_missing(&self, id: WorktreeId) -> Result<(), Error> {
        self.call(move |c| {
            require_changed(
                diesel::update(worktrees::table.find(id.0))
                    .set((worktrees::present.eq(0), worktrees::last_seen.eq(now()?)))
                    .execute(c)?,
                "worktree",
            )
        })
        .await
    }
    /// Registers a lane; conflicting active indices and foreign worktrees fail.
    pub async fn upsert_lane(&self, input: LaneInput) -> Result<Lane, Error> {
        if input.name.is_empty() {
            return Err(Error::Invalid("empty lane name".into()));
        }
        let profiles = if input.profiles.is_empty() {
            legacy_profiles(input.profile)
        } else {
            input.profiles
        };
        let profile = profiles.first().cloned();
        let profiles = encode_profiles(&profiles)?;
        self.call(move |c| {
            c.immediate_transaction(|c| {
                if !diesel::select(exists(
                    worktrees::table
                        .find(input.worktree_id.0)
                        .filter(worktrees::project_id.eq(input.project_id.0)),
                ))
                .get_result::<bool>(c)?
                {
                    return Err(Error::NotFound("project worktree"));
                }
                let revived = lanes::table
                    .filter(lanes::worktree_id.eq(input.worktree_id.0))
                    .select(lanes::cleaned_at)
                    .first::<Option<i64>>(c)
                    .optional()?
                    .flatten()
                    .is_some();
                let row = NewLane {
                    project_id: input.project_id.0,
                    worktree_id: input.worktree_id.0,
                    lane_index: i64::from(input.index),
                    name: input.name,
                    desired_state: input.desired_state.as_str().into(),
                    profile,
                    profiles,
                    created_at: now()?,
                };
                diesel::insert_into(lanes::table)
                    .values(&row)
                    .on_conflict(lanes::worktree_id)
                    .do_update()
                    .set((
                        lanes::lane_index.eq(row.lane_index),
                        lanes::name.eq(&row.name),
                        lanes::desired_state.eq(&row.desired_state),
                        lanes::profile.eq(&row.profile),
                        lanes::profiles.eq(&row.profiles),
                        lanes::cleaned_at.eq(None::<i64>),
                    ))
                    .execute(c)?;
                // A cleaned lane coming back starts over, so its age does too.
                if revived {
                    diesel::update(lanes::table.filter(lanes::worktree_id.eq(row.worktree_id)))
                        .set(lanes::created_at.eq(row.created_at))
                        .execute(c)?;
                }
                lanes::table
                    .filter(lanes::worktree_id.eq(row.worktree_id))
                    .select(LaneRow::as_select())
                    .first::<LaneRow>(c)?
                    .try_into()
            })
        })
        .await
    }
    /// Returns a lane, including cleaned history records.
    pub async fn lane(&self, id: LaneId) -> Result<Option<Lane>, Error> {
        self.call(move |c| {
            lanes::table
                .find(id.0)
                .select(LaneRow::as_select())
                .first::<LaneRow>(c)
                .optional()?
                .map(Lane::try_from)
                .transpose()
        })
        .await
    }
    /// Lists one project's lanes, or all lanes for `None`. A row that cannot be
    /// decoded is logged and skipped so it cannot hide every other lane; `lane`
    /// still reports it as [`Error::Corrupt`].
    pub async fn lanes(&self, project: Option<ProjectId>) -> Result<Vec<Lane>, Error> {
        self.call(move |c| {
            let mut query = lanes::table.into_boxed();
            if let Some(project) = project {
                query = query.filter(lanes::project_id.eq(project.0));
            }
            Ok(query
                .order((lanes::project_id, lanes::lane_index, lanes::id))
                .select(LaneRow::as_select())
                .load::<LaneRow>(c)?
                .into_iter()
                .filter_map(|row| {
                    Lane::try_from(row)
                        .map_err(|error| tracing::warn!(%error, "skipping unreadable lane row"))
                        .ok()
                })
                .collect())
        })
        .await
    }
    /// Compatibility setter for a plain name or an older daemon's encoded array.
    /// Use `set_lane_desired_profiles` to persist an ordered selection directly.
    pub async fn set_lane_desired(
        &self,
        id: LaneId,
        desired: DesiredState,
        profile: Option<String>,
    ) -> Result<(), Error> {
        self.set_lane_desired_profiles(id, desired, legacy_profiles(profile))
            .await
    }

    /// Updates resume intent and the complete ordered profile selection atomically.
    /// An empty vector clears both the selection and its compatibility first entry.
    pub async fn set_lane_desired_profiles(
        &self,
        id: LaneId,
        desired: DesiredState,
        profiles: Vec<String>,
    ) -> Result<(), Error> {
        let profile = profiles.first().cloned();
        let profiles = encode_profiles(&profiles)?;
        self.call(move |c| {
            require_changed(
                diesel::update(lanes::table.find(id.0).filter(lanes::cleaned_at.is_null()))
                    .set((
                        lanes::desired_state.eq(desired.as_str()),
                        lanes::profile.eq(profile),
                        lanes::profiles.eq(profiles),
                    ))
                    .execute(c)?,
                "active lane",
            )
        })
        .await
    }

    /// [`Self::set_lane_desired_profiles`] that also replaces the services a plain
    /// start brings back, in one update.
    pub async fn set_lane_selection(
        &self,
        id: LaneId,
        desired: DesiredState,
        profiles: Vec<String>,
        services: Vec<String>,
    ) -> Result<(), Error> {
        let profile = profiles.first().cloned();
        let profiles = encode_profiles(&profiles)?;
        let services = encode_profiles(&services)?;
        self.call(move |c| {
            require_changed(
                diesel::update(lanes::table.find(id.0).filter(lanes::cleaned_at.is_null()))
                    .set((
                        lanes::desired_state.eq(desired.as_str()),
                        lanes::profile.eq(profile),
                        lanes::profiles.eq(profiles),
                        lanes::services.eq(services),
                    ))
                    .execute(c)?,
                "active lane",
            )
        })
        .await
    }
    /// Frees the lane's range and active index after clean phases finish, and
    /// forgets its setup records, once-only task markers and task history in the
    /// same transaction, so a failed clean leaves the lane as it was. Unit run
    /// history stays.
    pub async fn clean_lane(&self, id: LaneId) -> Result<(), Error> {
        self.call(move |c| {
            c.immediate_transaction(|c| {
                require_no_active_runs(c, Some(&[id.0]))?;
                require_changed(
                    diesel::update(lanes::table.find(id.0))
                        .set((
                            lanes::cleaned_at.eq(now()?),
                            lanes::desired_state.eq("stopped"),
                            lanes::services.eq("[]"),
                        ))
                        .execute(c)?,
                    "lane",
                )?;
                diesel::delete(port_blocks::table.filter(port_blocks::lane_id.eq(id.0)))
                    .execute(c)?;
                diesel::delete(lane_setup::table.filter(lane_setup::lane_id.eq(id.0)))
                    .execute(c)?;
                diesel::delete(task_completions::table.filter(task_completions::lane_id.eq(id.0)))
                    .execute(c)?;
                diesel::delete(task_runs::table.filter(task_runs::lane_id.eq(id.0))).execute(c)?;
                Ok(())
            })
        })
        .await
    }
    /// Deletes a lane and its history after the caller stops resources.
    pub async fn delete_lane(&self, id: LaneId) -> Result<(), Error> {
        self.call(move |c| {
            c.immediate_transaction(|c| {
                require_no_active_runs(c, Some(&[id.0]))?;
                require_changed(diesel::delete(lanes::table.find(id.0)).execute(c)?, "lane")
            })
        })
        .await
    }
    /// Records the verified leader identity immediately after spawn. A second
    /// unfinished run of the same lane and unit fails with [`Error::Conflict`].
    pub async fn record_unit_run(&self, input: UnitRunInput) -> Result<UnitRun, Error> {
        if input.unit.is_empty() || input.process.pid == 0 || stored_session(&input.process) == 0 {
            return Err(Error::Invalid(
                "unit name, PID and SID must be nonempty/nonzero".into(),
            ));
        }
        self.call(move |c| {
            c.immediate_transaction(|c| {
                if !diesel::select(exists(
                    lanes::table
                        .find(input.lane_id.0)
                        .filter(lanes::cleaned_at.is_null()),
                ))
                .get_result::<bool>(c)?
                {
                    return Err(Error::NotFound("active lane"));
                }
                let row = NewUnitRun {
                    lane_id: input.lane_id.0,
                    unit: input.unit,
                    pid: i64::from(input.process.pid),
                    sid: stored_session(&input.process),
                    process_start_time: input.process.start_time.to_string(),
                    started_at: now()?,
                    actor_kind: actor_kind(input.actor.kind),
                    actor_agent_id: input.actor.agent_id,
                    actor_session_id: input.actor.session_id,
                    log_path: input.log_path.as_deref().map(path_bytes),
                    boot_id: input.boot_id,
                };
                diesel::insert_into(unit_runs::table)
                    .values(&row)
                    .returning(UnitRunRow::as_returning())
                    .get_result::<UnitRunRow>(c)?
                    .try_into()
            })
        })
        .await
    }
    /// Completes a run once; repeated calls retain the original outcome and time.
    pub async fn finish_unit_run(&self, id: UnitRunId, outcome: ExitOutcome) -> Result<(), Error> {
        self.call(move |c| {
            let changed = diesel::update(
                unit_runs::table
                    .find(id.0)
                    .filter(unit_runs::ended_at.is_null()),
            )
            .set((
                unit_runs::ended_at.eq(now()?),
                unit_runs::exit_code.eq(outcome.code),
                unit_runs::exit_signal.eq(outcome.signal),
            ))
            .execute(c)?;
            if changed == 0
                && !diesel::select(exists(unit_runs::table.find(id.0))).get_result::<bool>(c)?
            {
                return Err(Error::NotFound("unit run"));
            }
            Ok(())
        })
        .await
    }
    /// Lists unfinished identities for PID-reuse-checked reconciliation.
    pub async fn active_unit_runs(&self) -> Result<Vec<UnitRun>, Error> {
        self.call(|c| {
            unit_runs::table
                .filter(unit_runs::ended_at.is_null())
                .order(unit_runs::id)
                .select(UnitRunRow::as_select())
                .load::<UnitRunRow>(c)?
                .into_iter()
                .map(UnitRun::try_from)
                .collect()
        })
        .await
    }
    /// Finishes, with an unknown outcome, every unfinished run recorded under a
    /// boot other than `current`. Their PIDs name unrelated processes now, so they
    /// must never be probed or signalled. Rows without a boot ID are kept for the
    /// caller's identity check. Returns how many runs ended.
    pub async fn finish_other_boot_runs(&self, current: String) -> Result<usize, Error> {
        self.call(move |c| {
            Ok(diesel::update(
                unit_runs::table
                    .filter(unit_runs::ended_at.is_null())
                    .filter(unit_runs::boot_id.is_not_null())
                    .filter(unit_runs::boot_id.ne(current)),
            )
            .set((
                unit_runs::ended_at.eq(now()?),
                unit_runs::exit_code.eq(None::<i32>),
                unit_runs::exit_signal.eq(None::<i32>),
            ))
            .execute(c)?)
        })
        .await
    }
    /// Remembers descendants of an unfinished run that left the leader's session,
    /// so stopping the run after a daemon crash can still reach them. Repeated
    /// identities are ignored; returns how many were new. Finished runs are not
    /// extended.
    pub async fn record_unit_run_members(
        &self,
        run: UnitRunId,
        members: Vec<ProcessIdentity>,
    ) -> Result<usize, Error> {
        if members.is_empty() {
            return Ok(0);
        }
        if members.iter().any(|member| member.pid == 0) {
            return Err(Error::Invalid("member PID must be nonzero".into()));
        }
        self.call(move |c| {
            c.immediate_transaction(|c| {
                if !diesel::select(exists(
                    unit_runs::table
                        .find(run.0)
                        .filter(unit_runs::ended_at.is_null()),
                ))
                .get_result::<bool>(c)?
                {
                    return Err(Error::NotFound("unfinished unit run"));
                }
                let rows: Vec<MemberRow> = members
                    .into_iter()
                    .map(|member| MemberRow {
                        unit_run_id: run.0,
                        pid: i64::from(member.pid),
                        sid: i64::from(member.sid),
                        process_start_time: member.start_time.to_string(),
                    })
                    .collect();
                Ok(diesel::insert_or_ignore_into(unit_run_members::table)
                    .values(&rows)
                    .execute(c)?)
            })
        })
        .await
    }
    /// Lists the recorded escaped descendants of a run, in PID order.
    pub async fn unit_run_members(&self, run: UnitRunId) -> Result<Vec<ProcessIdentity>, Error> {
        self.call(move |c| {
            unit_run_members::table
                .filter(unit_run_members::unit_run_id.eq(run.0))
                .order((unit_run_members::pid, unit_run_members::process_start_time))
                .select(MemberRow::as_select())
                .load::<MemberRow>(c)?
                .into_iter()
                .map(ProcessIdentity::try_from)
                .collect()
        })
        .await
    }
    /// Lists bounded run history, newest first.
    pub async fn unit_runs(&self, lane: LaneId, limit: u32) -> Result<Vec<UnitRun>, Error> {
        self.call(move |c| {
            unit_runs::table
                .filter(unit_runs::lane_id.eq(lane.0))
                .order(unit_runs::id.desc())
                .limit(i64::from(limit))
                .select(UnitRunRow::as_select())
                .load::<UnitRunRow>(c)?
                .into_iter()
                .map(UnitRun::try_from)
                .collect()
        })
        .await
    }
}

/// Keeps the old decoder's literal fallback while callers adopt ordered selections.
fn legacy_profiles(profile: Option<String>) -> Vec<String> {
    profile.map_or_else(Vec::new, |value| {
        serde_json::from_str(&value).unwrap_or_else(|_| vec![value])
    })
}

/// Serializes only the typed profile list; JSON representation stays inside state.
fn encode_profiles(profiles: &[String]) -> Result<String, Error> {
    serde_json::to_string(profiles).map_err(|error| Error::Invalid(error.to_string()))
}

/// Counts unfinished unit and task runs in `lanes`, or everywhere for `None`.
pub(crate) fn active_runs(
    c: &mut SqliteConnection,
    lanes: Option<&[i64]>,
) -> Result<(u64, u64), Error> {
    let mut units = unit_runs::table
        .filter(unit_runs::ended_at.is_null())
        .into_boxed();
    let mut tasks = task_runs::table
        .filter(task_runs::ended_at.is_null())
        .into_boxed();
    if let Some(lanes) = lanes {
        units = units.filter(unit_runs::lane_id.eq_any(lanes));
        tasks = tasks.filter(task_runs::lane_id.eq_any(lanes));
    }
    let units: i64 = units.count().get_result(c)?;
    let tasks: i64 = tasks.count().get_result(c)?;
    Ok((from_sql_u64(units), from_sql_u64(tasks)))
}

/// Rejects cleanup while process or task identities remain unfinished.
fn require_no_active_runs(c: &mut SqliteConnection, lanes: Option<&[i64]>) -> Result<(), Error> {
    match active_runs(c, lanes)? {
        (0, 0) => Ok(()),
        (units, tasks) => Err(Error::ActiveRuns { units, tasks }),
    }
}
/// Converts an absent update to a domain not-found error.
pub(crate) fn require_changed(changed: usize, record: &'static str) -> Result<(), Error> {
    if changed == 0 {
        Err(Error::NotFound(record))
    } else {
        Ok(())
    }
}
/// The `unit_runs.sid` value for a leader: its session.
#[cfg(unix)]
fn stored_session(process: &ProcessIdentity) -> i64 {
    i64::from(process.sid)
}
/// The `unit_runs.sid` value for a leader. The column must be positive and
/// Windows has no sessions (`sid` 0), so the leader's PID fills it; nothing
/// on Windows reads it as a session.
#[cfg(windows)]
fn stored_session(process: &ProcessIdentity) -> i64 {
    i64::from(if process.sid == 0 {
        process.pid
    } else {
        process.sid
    })
}
/// Unix milliseconds for audit timestamps.
pub(crate) fn now() -> Result<i64, Error> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::Invalid(error.to_string()))?
        .as_millis();
    i64::try_from(millis).map_err(|error| Error::Invalid(error.to_string()))
}

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    path::PathBuf,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use serde::Serialize;
use tokio::sync::{OnceCell, OwnedMutexGuard};

use crate::{
    Error, NameConflict,
    cli::{ComposeCli, ComposeProject, ExistingContainer, UpOptions},
    model::{ComposeModel, Health, service_health},
    overrides::{render_override, write_override},
    ports::{Endpoints, LanePorts, PortBlock, PortRequest, endpoints_from_ps, port_requests},
    version::ComposeVersion,
};

/// Compose's own project label, present on every container it creates.
const COMPOSE_PROJECT_LABEL: &str = "com.docker.compose.project";

/// The shared compose project of a devlanes project, run from the primary worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedInfra {
    /// `infra.project_name`; `None` keeps Compose's default (`name:` or directory).
    pub project_name: Option<String>,
    /// Directory of `devlanes.yaml` in the primary worktree; compose paths resolve here.
    pub primary_dir: PathBuf,
    pub files: Vec<PathBuf>,
    /// The primary worktree's `.env`; skipped when the file does not exist.
    pub env_file: Option<PathBuf>,
    pub profiles: Vec<String>,
    pub exclude: BTreeSet<String>,
    pub per_lane: BTreeSet<String>,
    /// `infra.env`, applied to Compose's `${VAR}` interpolation.
    pub env: BTreeMap<String, String>,
}

/// The per-lane compose project `<project>-lane<N>`, run from the lane's worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneInfra {
    /// devlanes project name; it becomes the lane project's prefix.
    pub project: String,
    pub lane: u32,
    /// Directory of `devlanes.yaml` in the lane's worktree.
    pub lane_dir: PathBuf,
    pub files: Vec<PathBuf>,
    /// Skipped when the file does not exist.
    pub env_file: Option<PathBuf>,
    pub profiles: Vec<String>,
    pub exclude: BTreeSet<String>,
    pub per_lane: BTreeSet<String>,
    pub env: BTreeMap<String, String>,
    /// devlanes-owned directory for the generated override file.
    pub state_dir: PathBuf,
}

impl LaneInfra {
    /// The lane project name, `<project>-lane<N>`.
    pub fn project_name(&self) -> Result<String, Error> {
        lane_project_name(&self.project, self.lane)
    }

    /// `per_lane` without `exclude`, in name order.
    pub fn services(&self) -> Vec<String> {
        self.per_lane.difference(&self.exclude).cloned().collect()
    }
}

/// State of the shared project after `up` or `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SharedReport {
    pub project: String,
    pub services: Vec<String>,
    /// Services whose fixed `container_name` already existed in this project.
    pub adopted: Vec<String>,
    pub endpoints: Endpoints,
    pub health: BTreeMap<String, Health>,
}

/// State of a lane project after `up` or `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LaneReport {
    pub project: String,
    pub services: Vec<String>,
    pub override_path: Option<PathBuf>,
    pub endpoints: Endpoints,
    pub health: BTreeMap<String, Health>,
    /// Isolation problems that devlanes cannot fix with an override, such as external
    /// volumes shared by every lane.
    pub warnings: Vec<String>,
}

/// Serializes mutating operations per compose project name. Concurrent lane starts share
/// one shared project, and two `up` calls on one project race on container creation.
#[derive(Debug, Default)]
pub struct ProjectLocks {
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl ProjectLocks {
    /// Waits for exclusive access to `project`. The guard releases it on drop.
    pub async fn lock(&self, project: &str) -> OwnedMutexGuard<()> {
        let mutex = {
            // A panic while holding the map lock cannot leave the map inconsistent.
            let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
            locks.entry(project.to_owned()).or_default().clone()
        };
        mutex.lock_owned().await
    }
}

/// The Compose adapter. Clones share the version cache and the per-project locks, so
/// create one per daemon.
#[derive(Debug, Clone, Default)]
pub struct Compose {
    cli: ComposeCli,
    locks: Arc<ProjectLocks>,
    version: Arc<OnceCell<ComposeVersion>>,
}

impl Compose {
    /// Wraps a CLI runner.
    pub fn new(cli: ComposeCli) -> Self {
        Self {
            cli,
            locks: Arc::default(),
            version: Arc::default(),
        }
    }

    /// The underlying CLI runner.
    pub fn cli(&self) -> &ComposeCli {
        &self.cli
    }

    /// The per-project locks, for callers that run setup entries against a project.
    pub fn locks(&self) -> &Arc<ProjectLocks> {
        &self.locks
    }

    /// The installed Compose version, read once per adapter (for `doctor`).
    pub async fn version(&self) -> Result<ComposeVersion, Error> {
        self.version
            .get_or_try_init(|| self.cli.version())
            .await
            .copied()
    }

    /// Resolves the shared project and its model. The returned project always has a name:
    /// `infra.project_name`, else the name Compose derived.
    pub async fn shared_project(
        &self,
        infra: &SharedInfra,
    ) -> Result<(ComposeProject, ComposeModel), Error> {
        if let Some(name) = &infra.project_name {
            validate_project_name(name)?;
        }
        let mut project = ComposeProject {
            name: infra.project_name.clone(),
            working_dir: infra.primary_dir.clone(),
            files: infra.files.clone(),
            env_file: existing_env_file(&infra.primary_dir, infra.env_file.as_ref()).await?,
            profiles: infra.profiles.clone(),
            env: infra.env.clone(),
        };
        let model = self.cli.config(&project, &[]).await?;
        project.name = Some(model.name.clone());
        Ok((project, model))
    }

    /// Starts the shared services (all resolved services except `exclude` and `per_lane`)
    /// with `up --wait`, after checking that no foreign container holds one of their
    /// `container_name`s.
    pub async fn shared_up(
        &self,
        infra: &SharedInfra,
        options: UpOptions,
    ) -> Result<SharedReport, Error> {
        let (project, model) = self.shared_project(infra).await?;
        let name = model.name.clone();
        let _guard = self.locks.lock(&name).await;
        let services = model.shared_services(&infra.exclude, &infra.per_lane)?;
        let wanted = fixed_names(&model, &services);
        let names: Vec<String> = wanted.iter().map(|(_, name)| name.clone()).collect();
        let existing = self.cli.inspect_containers(&names).await?;
        let (adopted, conflicts) = classify_conflicts(&name, &wanted, &existing);
        if !conflicts.is_empty() {
            return Err(Error::ContainerNameConflict(conflicts));
        }
        if !services.is_empty() {
            self.cli
                .up(&project, &[], &services, false, options)
                .await?;
        }
        let statuses = self.cli.ps(&name).await?;
        let mut endpoints = endpoints_from_ps(&model, &statuses);
        endpoints.retain(|service, _| services.contains(service));
        Ok(SharedReport {
            project: name,
            health: service_health(&services, &statuses),
            services,
            adopted,
            endpoints,
        })
    }

    /// Reports health and endpoints of the shared services without changing anything.
    pub async fn shared_status(&self, infra: &SharedInfra) -> Result<SharedReport, Error> {
        let (_, model) = self.shared_project(infra).await?;
        let services = model.shared_services(&infra.exclude, &infra.per_lane)?;
        let statuses = self.cli.ps(&model.name).await?;
        let mut endpoints = endpoints_from_ps(&model, &statuses);
        endpoints.retain(|service, _| services.contains(service));
        Ok(SharedReport {
            project: model.name,
            health: service_health(&services, &statuses),
            services,
            adopted: Vec::new(),
            endpoints,
        })
    }

    /// Stops the shared project's containers and keeps their data. The caller decides when
    /// no lane needs them any more.
    pub async fn shared_stop(&self, project: &str, timeout: Duration) -> Result<(), Error> {
        let _guard = self.locks.lock(project).await;
        self.cli.stop(project, timeout).await
    }

    /// Container ports of the lane's services that need host ports from the lane block.
    /// Allocate them (for example by [`PortRequest::key`]) and pass them to [`Self::lane_up`].
    pub async fn lane_port_requests(&self, lane: &LaneInfra) -> Result<Vec<PortRequest>, Error> {
        let (_, model) = self.lane_model(lane).await?;
        port_requests(&model, &lane.services())
    }

    /// Writes the lane override and starts the lane's services with `up --wait --no-deps`
    /// (shared dependencies are reached through their published ports, not started again).
    pub async fn lane_up(
        &self,
        lane: &LaneInfra,
        ports: &LanePorts,
        block: PortBlock,
        options: UpOptions,
    ) -> Result<LaneReport, Error> {
        self.version().await?.require_override_tags()?;
        let services = lane.services();
        let (project, model) = self.lane_model(lane).await?;
        let name = lane.project_name()?;
        let _guard = self.locks.lock(&name).await;
        let requests = port_requests(&model, &services)?;
        let rendered =
            render_override(&lane.project, lane.lane, &services, &requests, ports, block)?;
        let override_path = write_override(&lane.state_dir, &rendered.yaml).await?;
        if !services.is_empty() {
            self.cli
                .up(
                    &project,
                    std::slice::from_ref(&override_path),
                    &services,
                    true,
                    options,
                )
                .await?;
        }
        let statuses = self.cli.ps(&name).await?;
        Ok(LaneReport {
            project: name,
            health: service_health(&services, &statuses),
            warnings: volume_warnings(&model, &services),
            services,
            override_path: Some(override_path),
            endpoints: rendered.endpoints,
        })
    }

    /// Reports health and published ports of the lane's services.
    pub async fn lane_status(&self, lane: &LaneInfra) -> Result<LaneReport, Error> {
        let services = lane.services();
        let (_, model) = self.lane_model(lane).await?;
        let name = lane.project_name()?;
        let statuses = self.cli.ps(&name).await?;
        let mut endpoints = endpoints_from_ps(&model, &statuses);
        endpoints.retain(|service, _| services.contains(service));
        Ok(LaneReport {
            project: name,
            health: service_health(&services, &statuses),
            warnings: volume_warnings(&model, &services),
            services,
            override_path: None,
            endpoints,
        })
    }

    /// Stops the lane project's containers and keeps their data.
    pub async fn lane_stop(
        &self,
        project: &str,
        lane: u32,
        timeout: Duration,
    ) -> Result<(), Error> {
        let name = lane_project_name(project, lane)?;
        let _guard = self.locks.lock(&name).await;
        self.cli.stop(&name, timeout).await
    }

    /// Removes the lane project's containers, networks and named volumes (lane clean).
    /// Works by name, so it also cleans lanes whose worktree is gone.
    pub async fn lane_down(
        &self,
        project: &str,
        lane: u32,
        timeout: Duration,
    ) -> Result<(), Error> {
        let name = lane_project_name(project, lane)?;
        let _guard = self.locks.lock(&name).await;
        self.cli.down(&name, true, timeout).await
    }

    /// Resolves the lane project from the lane's worktree without the override, so port
    /// requests reflect the files the developer wrote.
    async fn lane_model(&self, lane: &LaneInfra) -> Result<(ComposeProject, ComposeModel), Error> {
        let project = ComposeProject {
            name: Some(lane.project_name()?),
            working_dir: lane.lane_dir.clone(),
            files: lane.files.clone(),
            env_file: existing_env_file(&lane.lane_dir, lane.env_file.as_ref()).await?,
            profiles: lane.profiles.clone(),
            env: lane.env.clone(),
        };
        let model = self.cli.config(&project, &[]).await?;
        model.require_services(&lane.services())?;
        Ok((project, model))
    }
}

/// `<project>-lane<N>`, with characters Compose rejects (such as `.`) replaced by `-`.
pub fn lane_project_name(project: &str, lane: u32) -> Result<String, Error> {
    let base: String = project
        .to_ascii_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let name = format!("{base}-lane{lane}");
    validate_project_name(&name)?;
    Ok(name)
}

/// Compose project names: lowercase letters, digits, `-` and `_`, starting with a letter
/// or digit.
pub fn validate_project_name(name: &str) -> Result<(), Error> {
    let mut chars = name.chars();
    let valid_start = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let valid_rest =
        chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if valid_start && valid_rest {
        Ok(())
    } else {
        Err(Error::InvalidProjectName(name.to_owned()))
    }
}

/// Splits existing containers that hold wanted `container_name`s into adoptable ones
/// (same compose project, which `up` reuses) and conflicts (another project or no
/// compose label). Returns adopted service names and the conflicts.
pub fn classify_conflicts(
    project: &str,
    wanted: &[(String, String)],
    existing: &[ExistingContainer],
) -> (Vec<String>, Vec<NameConflict>) {
    let mut adopted = Vec::new();
    let mut conflicts = Vec::new();
    for (service, container_name) in wanted {
        let Some(container) = existing.iter().find(|found| &found.name == container_name) else {
            continue;
        };
        let owner = container.labels.get(COMPOSE_PROJECT_LABEL);
        if owner.is_some_and(|owner| owner == project) {
            adopted.push(service.clone());
        } else {
            conflicts.push(NameConflict {
                service: service.clone(),
                container_name: container_name.clone(),
                owner: owner.cloned(),
            });
        }
    }
    (adopted, conflicts)
}

/// `(service, container_name)` for selected services with a fixed name.
fn fixed_names(model: &ComposeModel, services: &[String]) -> Vec<(String, String)> {
    services
        .iter()
        .filter_map(|service| {
            let name = model.services.get(service)?.container_name.clone()?;
            Some((service.clone(), name))
        })
        .collect()
}

fn volume_warnings(model: &ComposeModel, services: &[String]) -> Vec<String> {
    model
        .shared_volumes(services)
        .into_iter()
        .map(|volume| {
            format!("volume {volume:?} is external or has a fixed name, so every lane shares it")
        })
        .collect()
}

/// Passing a missing `--env-file` makes Compose fail, while a missing `.env` is normal for
/// new worktrees.
async fn existing_env_file(
    dir: &std::path::Path,
    env_file: Option<&PathBuf>,
) -> Result<Option<PathBuf>, Error> {
    let Some(env_file) = env_file else {
        return Ok(None);
    };
    let path = dir.join(env_file);
    Ok(tokio::fs::try_exists(&path).await?.then_some(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(name: &str, project: Option<&str>) -> ExistingContainer {
        ExistingContainer {
            id: format!("id-{name}"),
            name: name.into(),
            labels: project
                .map(|project| {
                    BTreeMap::from([(COMPOSE_PROJECT_LABEL.to_owned(), project.to_owned())])
                })
                .unwrap_or_default(),
        }
    }

    #[test]
    fn lane_project_names() {
        assert_eq!(
            lane_project_name("acme", 2).ok().as_deref(),
            Some("acme-lane2")
        );
        assert_eq!(
            lane_project_name("acme.web", 0).ok().as_deref(),
            Some("acme-web-lane0")
        );
        assert!(lane_project_name("", 1).is_err());
        assert!(lane_project_name("_x", 1).is_err());
    }

    #[test]
    fn project_name_rules() {
        for valid in ["acme", "0acme", "a_b-c"] {
            assert!(validate_project_name(valid).is_ok(), "{valid}");
        }
        for invalid in ["", "Acme", "-acme", "a.b", "a b"] {
            assert!(validate_project_name(invalid).is_err(), "{invalid}");
        }
    }

    #[test]
    fn conflicts_adopt_same_project_only() {
        let wanted = vec![
            ("postgres".to_owned(), "acme-postgres".to_owned()),
            ("mail".to_owned(), "acme-mail".to_owned()),
            ("cache".to_owned(), "acme-cache".to_owned()),
            ("free".to_owned(), "acme-free".to_owned()),
        ];
        let existing = [
            container("acme-postgres", Some("monorepo")),
            container("acme-mail", Some("acme")),
            container("acme-cache", None),
        ];
        let (adopted, conflicts) = classify_conflicts("monorepo", &wanted, &existing);
        assert_eq!(adopted, ["postgres"]);
        assert_eq!(
            conflicts,
            [
                NameConflict {
                    service: "mail".into(),
                    container_name: "acme-mail".into(),
                    owner: Some("acme".into())
                },
                NameConflict {
                    service: "cache".into(),
                    container_name: "acme-cache".into(),
                    owner: None
                },
            ]
        );
        let message = Error::ContainerNameConflict(conflicts).to_string();
        assert!(message.contains("docker compose -p acme down"), "{message}");
        assert!(message.contains("docker rm -f acme-cache"), "{message}");
    }

    #[test]
    fn fixed_names_from_model() {
        let model = ComposeModel::from_json(include_str!("../tests/fixtures/config.json"))
            .expect("fixture parses");
        let services = ["postgres".to_owned(), "redis".to_owned()];
        assert_eq!(
            fixed_names(&model, &services),
            [("postgres".to_owned(), "acme-postgres".to_owned())]
        );
    }

    #[test]
    fn lane_services_drop_excluded() {
        let lane = LaneInfra {
            project: "acme".into(),
            lane: 1,
            lane_dir: "/w".into(),
            files: Vec::new(),
            env_file: None,
            profiles: Vec::new(),
            exclude: BTreeSet::from(["caddy".to_owned()]),
            per_lane: BTreeSet::from(["mailpit".to_owned(), "caddy".to_owned()]),
            env: BTreeMap::new(),
            state_dir: "/s".into(),
        };
        assert_eq!(lane.services(), ["mailpit"]);
    }

    #[tokio::test]
    async fn locks_serialize_per_project() {
        let locks = Arc::new(ProjectLocks::default());
        let guard = locks.lock("acme").await;
        // Another project is independent.
        let other = tokio::time::timeout(Duration::from_secs(1), locks.lock("other")).await;
        assert!(other.is_ok());
        let blocked = tokio::time::timeout(Duration::from_millis(50), locks.lock("acme")).await;
        assert!(blocked.is_err(), "second lock on one project must wait");
        drop(guard);
        let free = tokio::time::timeout(Duration::from_secs(1), locks.lock("acme")).await;
        assert!(free.is_ok());
    }

    #[tokio::test]
    async fn missing_env_file_is_skipped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let env = PathBuf::from(".env");
        assert_eq!(
            existing_env_file(dir.path(), Some(&env)).await.ok(),
            Some(None)
        );
        tokio::fs::write(dir.path().join(".env"), "A=1\n")
            .await
            .expect("write");
        assert_eq!(
            existing_env_file(dir.path(), Some(&env)).await.ok(),
            Some(Some(dir.path().join(".env")))
        );
    }
}

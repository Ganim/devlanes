use std::collections::BTreeMap;

use crate::{Error, cli::ComposeProject};

/// Variables for `infra.setup`/`teardown` entries, so `docker compose exec` inside them
/// targets the shared project: `COMPOSE_PROJECT_NAME`, `COMPOSE_FILE` (absolute paths of
/// the primary worktree's files), `COMPOSE_PROFILES` when profiles are active, and
/// `DEVLANES_COMPOSE_LANE_PROJECT` naming the lane's own project when it has per-lane
/// services.
///
/// `shared` must have its name resolved (see [`crate::Compose::shared_project`]); an
/// unresolved name is left out rather than guessed.
pub fn setup_env(
    shared: &ComposeProject,
    lane_project: Option<&str>,
) -> Result<BTreeMap<String, String>, Error> {
    let mut env = BTreeMap::new();
    if let Some(name) = &shared.name {
        env.insert("COMPOSE_PROJECT_NAME".to_owned(), name.clone());
    }
    let mut files = Vec::new();
    for file in shared.absolute_files() {
        let text = file.to_string_lossy().into_owned();
        // COMPOSE_FILE is split on the platform path separator.
        if text.contains(':') {
            return Err(Error::PathSeparator(file));
        }
        files.push(text);
    }
    if !files.is_empty() {
        env.insert("COMPOSE_FILE".to_owned(), files.join(":"));
    }
    if !shared.profiles.is_empty() {
        env.insert("COMPOSE_PROFILES".to_owned(), shared.profiles.join(","));
    }
    if let Some(lane_project) = lane_project {
        env.insert(
            "DEVLANES_COMPOSE_LANE_PROJECT".to_owned(),
            lane_project.to_owned(),
        );
    }
    Ok(env)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> ComposeProject {
        ComposeProject {
            name: Some("monorepo".into()),
            working_dir: "/work/acme".into(),
            files: vec!["docker-compose.yml".into(), "docker-compose.dev.yml".into()],
            env_file: None,
            profiles: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "Unix path fixtures render with `\\` on Windows, ported in the Windows path step"
    )]
    fn builds_setup_env() {
        let env = setup_env(&shared(), Some("acme-lane2")).expect("valid");
        assert_eq!(env["COMPOSE_PROJECT_NAME"], "monorepo");
        assert_eq!(
            env["COMPOSE_FILE"],
            "/work/acme/docker-compose.yml:/work/acme/docker-compose.dev.yml"
        );
        assert_eq!(env["DEVLANES_COMPOSE_LANE_PROJECT"], "acme-lane2");
        assert!(!env.contains_key("COMPOSE_PROFILES"));
    }

    #[test]
    fn profiles_and_no_lane_project() {
        let mut project = shared();
        project.profiles = vec!["search".into(), "mail".into()];
        let env = setup_env(&project, None).expect("valid");
        assert_eq!(env["COMPOSE_PROFILES"], "search,mail");
        assert!(!env.contains_key("DEVLANES_COMPOSE_LANE_PROJECT"));
    }

    #[test]
    fn rejects_separator_in_paths() {
        let mut project = shared();
        project.working_dir = "/work/a:b".into();
        assert!(matches!(
            setup_env(&project, None),
            Err(Error::PathSeparator(_))
        ));
    }
}

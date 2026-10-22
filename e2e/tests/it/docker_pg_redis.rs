//! Plan M7 acceptance against `fixtures/compose-pg-redis`: a Postgres per lane, one
//! shared Redis, and `lane clean` removing only its own lane's containers and data.

use std::{path::Path, time::Duration};

use devlanes_e2e::{Lab, Output, Result};

/// Shared Compose project, named in the fixture's `infra.project_name`.
const SHARED: &str = "devlanes-fixture-pg-redis";
/// Per-lane Compose projects, `<project>-lane<N>`.
const LANE_ZERO: &str = "fixture-pg-redis-lane0";
const LANE_ONE: &str = "fixture-pg-redis-lane1";

/// `docker <args>`, required to succeed; returns trimmed stdout.
fn docker(lab: &Lab, args: &[&str]) -> Result<String> {
    let output: Output = lab.docker_cli(args)?;
    output.expect_code(0)?;
    Ok(output.stdout.trim().to_owned())
}

/// Runs SQL in the `app` database of a lane's own Postgres.
fn sql(lab: &Lab, project: &str, statement: &str) -> Result<String> {
    docker(
        lab,
        &[
            "compose",
            "-p",
            project,
            "exec",
            "-T",
            "postgres",
            "psql",
            "-U",
            "postgres",
            "-d",
            "app",
            "-v",
            "ON_ERROR_STOP=1",
            "-tAc",
            statement,
        ],
    )
}

/// Container IDs of a Compose project, stopped ones included.
fn containers(lab: &Lab, project: &str) -> Result<Vec<String>> {
    let ids = docker(
        lab,
        &[
            "ps",
            "-aq",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
        ],
    )?;
    Ok(ids.lines().map(str::to_owned).collect())
}

/// Named volumes of a Compose project.
fn volumes(lab: &Lab, project: &str) -> Result<Vec<String>> {
    let names = docker(
        lab,
        &[
            "volume",
            "ls",
            "-q",
            "--filter",
            &format!("label=com.docker.compose.project={project}"),
        ],
    )?;
    Ok(names.lines().map(str::to_owned).collect())
}

/// `start` in a worktree, with room for image pulls.
fn start(lab: &Lab, cwd: &Path) -> Result<()> {
    let command = lab.command(cwd, &["--timeout", "5m", "start"])?;
    let output = lab.run(command, Duration::from_secs(360))?;
    let started = output.expect_code(0)?.json()?;
    assert_eq!(
        started["ready"],
        true,
        "start in {}: {output}",
        cwd.display()
    );
    Ok(())
}

/// Two lanes each get their own Postgres and data; `lane clean 1` removes lane 1's
/// containers and volume and leaves lane 0's data and the shared Redis running.
#[test]
#[cfg_attr(not(windows), ignore = "needs docker")]
#[cfg_attr(windows, ignore = "needs docker and Docker support on Windows, step 7")]
fn docker_lanes_get_their_own_postgres_and_clean_drops_only_theirs() -> Result<()> {
    let lab = Lab::docker_projects(
        "compose-pg-redis",
        &[SHARED, LANE_ZERO, LANE_ONE],
        Duration::from_secs(900),
    )?;
    let feature = lab.add_worktree("feat/pg")?;
    start(&lab, lab.repo())?;
    start(&lab, &feature)?;

    // The setup entry created the table in each lane's own database.
    sql(&lab, LANE_ZERO, "INSERT INTO notes VALUES ('lane-zero')")?;
    sql(&lab, LANE_ONE, "INSERT INTO notes VALUES ('lane-one')")?;
    assert_eq!(sql(&lab, LANE_ZERO, "SELECT body FROM notes")?, "lane-zero");
    assert_eq!(sql(&lab, LANE_ONE, "SELECT body FROM notes")?, "lane-one");

    let zero_containers = containers(&lab, LANE_ZERO)?;
    let one_containers = containers(&lab, LANE_ONE)?;
    assert_eq!(zero_containers.len(), 1, "one Postgres in lane 0");
    assert_eq!(one_containers.len(), 1, "one Postgres in lane 1");
    assert_ne!(zero_containers, one_containers);
    assert!(!volumes(&lab, LANE_ZERO)?.is_empty(), "lane 0 has a volume");
    assert!(!volumes(&lab, LANE_ONE)?.is_empty(), "lane 1 has a volume");

    // Redis is shared: one container outside the lane projects, written by lane 0.
    let redis = containers(&lab, SHARED)?;
    assert_eq!(
        redis.len(),
        1,
        "only Redis runs in the shared project: {redis:?}"
    );
    let set = docker(
        &lab,
        &[
            "compose",
            "-p",
            SHARED,
            "exec",
            "-T",
            "redis",
            "redis-cli",
            "SET",
            "shared-key",
            "kept",
        ],
    )?;
    assert_eq!(set, "OK");

    let cleaned = lab.devlanes(lab.repo(), &["lane", "clean", "1", "--yes"])?;
    cleaned.expect_code(0)?;

    assert!(
        containers(&lab, LANE_ONE)?.is_empty(),
        "lane 1's Postgres container is gone"
    );
    assert!(
        volumes(&lab, LANE_ONE)?.is_empty(),
        "lane 1's volume is gone"
    );
    assert_eq!(containers(&lab, LANE_ZERO)?, zero_containers);
    assert!(
        !volumes(&lab, LANE_ZERO)?.is_empty(),
        "lane 0 keeps its volume"
    );
    assert_eq!(
        sql(&lab, LANE_ZERO, "SELECT body FROM notes")?,
        "lane-zero",
        "lane 0 keeps its data"
    );
    assert_eq!(containers(&lab, SHARED)?, redis, "Redis was not recreated");
    let running = docker(&lab, &["inspect", "-f", "{{.State.Running}}", &redis[0]])?;
    assert_eq!(running, "true", "shared Redis keeps running");
    let kept = docker(
        &lab,
        &[
            "compose",
            "-p",
            SHARED,
            "exec",
            "-T",
            "redis",
            "redis-cli",
            "GET",
            "shared-key",
        ],
    )?;
    assert_eq!(kept, "kept");
    Ok(())
}

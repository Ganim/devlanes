use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::Deserialize;
use tokio::process::Command;

use crate::{
    Error,
    model::{ComposeModel, ContainerStatus, parse_ps},
    version::ComposeVersion,
};

/// Variables that would silently change which project, files or profiles Compose uses.
/// devlanes passes all of them as flags instead.
const OVERRIDDEN_VARIABLES: &[&str] = &[
    "COMPOSE_PROJECT_NAME",
    "COMPOSE_FILE",
    "COMPOSE_PROFILES",
    "COMPOSE_ENV_FILES",
    "COMPOSE_PATH_SEPARATOR",
];

/// Timeout for read-only commands (`version`, `config`, `ps`, `inspect`).
const QUERY_TIMEOUT: Duration = Duration::from_secs(60);

/// One Compose project invocation: `docker compose -p <name> -f <files> --env-file <file>
/// --profile <p>` run in `working_dir` with `env` added to the process environment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeProject {
    /// `None` lets Compose pick its default (top-level `name:` or directory name).
    pub name: Option<String>,
    /// Relative `files` and `env_file` resolve here; commands run here.
    pub working_dir: PathBuf,
    pub files: Vec<PathBuf>,
    pub env_file: Option<PathBuf>,
    pub profiles: Vec<String>,
    /// Interpolation values. Process environment beats `--env-file` in Compose, so these
    /// win over the `.env` file.
    pub env: BTreeMap<String, String>,
}

impl ComposeProject {
    /// Absolute compose file paths in `-f` order.
    pub fn absolute_files(&self) -> Vec<PathBuf> {
        self.files
            .iter()
            .map(|file| self.working_dir.join(file))
            .collect()
    }

    /// Global flags shared by every subcommand. Extra files (the lane override) go after
    /// the project's own files so they take precedence.
    pub fn args(&self, extra_files: &[PathBuf]) -> Vec<OsString> {
        let mut args: Vec<OsString> = vec!["compose".into()];
        if let Some(name) = &self.name {
            args.extend(["-p".into(), name.into()]);
        }
        for file in self
            .absolute_files()
            .into_iter()
            .chain(extra_files.iter().cloned())
        {
            args.extend(["-f".into(), file.into_os_string()]);
        }
        if let Some(env_file) = &self.env_file {
            args.extend([
                "--env-file".into(),
                self.working_dir.join(env_file).into_os_string(),
            ]);
        }
        for profile in &self.profiles {
            args.extend(["--profile".into(), profile.into()]);
        }
        args
    }
}

/// Options for `up`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpOptions {
    /// Passed as `--wait-timeout`; covers health after the containers start.
    pub wait_timeout: Duration,
    /// Bound on the whole command, including image pulls.
    pub command_timeout: Duration,
    /// Removes containers of services no longer in the files.
    pub remove_orphans: bool,
}

impl Default for UpOptions {
    fn default() -> Self {
        Self {
            wait_timeout: Duration::from_secs(120),
            command_timeout: Duration::from_secs(600),
            remove_orphans: false,
        }
    }
}

/// A container found by name, used for `container_name` conflict checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingContainer {
    pub id: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}

/// Runs the `docker` CLI. Cloning is cheap; each call spawns its own process.
#[derive(Debug, Clone)]
pub struct ComposeCli {
    program: PathBuf,
    base_env: Option<BTreeMap<String, String>>,
    extra_env: BTreeMap<String, String>,
}

impl Default for ComposeCli {
    fn default() -> Self {
        Self::new()
    }
}

impl ComposeCli {
    /// Uses `docker` from `PATH` and inherits the daemon's environment.
    pub fn new() -> Self {
        Self {
            program: PathBuf::from("docker"),
            base_env: None,
            extra_env: BTreeMap::new(),
        }
    }

    /// Uses another `docker` binary.
    pub fn with_program(mut self, program: impl Into<PathBuf>) -> Self {
        self.program = program.into();
        self
    }

    /// Replaces the inherited environment with `env` (it needs `PATH`, `HOME` and any
    /// `DOCKER_*` variables). Use when the daemon's own environment must not leak.
    pub fn with_base_env(mut self, env: BTreeMap<String, String>) -> Self {
        self.base_env = Some(env);
        self
    }

    /// Adds one variable to every invocation, for example `DOCKER_HOST`.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.insert(key.into(), value.into());
        self
    }

    /// Reads `docker compose version --short`.
    pub async fn version(&self) -> Result<ComposeVersion, Error> {
        let args = vec!["compose".into(), "version".into(), "--short".into()];
        let output = self
            .run(Path::new("/"), args, &BTreeMap::new(), QUERY_TIMEOUT)
            .await?;
        ComposeVersion::parse(&output)
    }

    /// Resolves the project with `config --format json`. Pass the lane override in
    /// `extra_files` to see the model a lane will actually run.
    pub async fn config(
        &self,
        project: &ComposeProject,
        extra_files: &[PathBuf],
    ) -> Result<ComposeModel, Error> {
        let mut args = project.args(extra_files);
        args.extend(["config".into(), "--format".into(), "json".into()]);
        let command = display(&args);
        let output = self
            .run(&project.working_dir, args, &project.env, QUERY_TIMEOUT)
            .await?;
        ComposeModel::from_json(&output).map_err(|source| Error::Json { command, source })
    }

    /// Starts `services` detached and waits for them to be running or healthy.
    /// `no_deps` keeps per-lane projects from starting their own copy of shared services.
    pub async fn up(
        &self,
        project: &ComposeProject,
        extra_files: &[PathBuf],
        services: &[String],
        no_deps: bool,
        options: UpOptions,
    ) -> Result<(), Error> {
        // An empty list would make Compose start every service in the files.
        if services.is_empty() {
            return Ok(());
        }
        let mut args = project.args(extra_files);
        args.extend([
            "up".into(),
            "--detach".into(),
            "--wait".into(),
            "--wait-timeout".into(),
            options.wait_timeout.as_secs().max(1).to_string().into(),
        ]);
        if options.remove_orphans {
            args.push("--remove-orphans".into());
        }
        if no_deps {
            args.push("--no-deps".into());
        }
        args.push("--".into());
        args.extend(services.iter().map(OsString::from));
        self.run(
            &project.working_dir,
            args,
            &project.env,
            options.command_timeout,
        )
        .await
        .map(drop)
    }

    /// Lists every container of the project by name only, so it works after the worktree
    /// that started it is gone.
    pub async fn ps(&self, project: &str) -> Result<Vec<ContainerStatus>, Error> {
        let args = by_name(project, ["ps", "--all", "--format", "json"]);
        let command = display(&args);
        let output = self
            .run(Path::new("/"), args, &BTreeMap::new(), QUERY_TIMEOUT)
            .await?;
        parse_ps(&output).map_err(|source| Error::Json { command, source })
    }

    /// Stops the project's containers and keeps them and their data.
    pub async fn stop(&self, project: &str, timeout: Duration) -> Result<(), Error> {
        let args = by_name(project, ["stop"]);
        self.run(Path::new("/"), args, &BTreeMap::new(), timeout)
            .await
            .map(drop)
    }

    /// Removes the project's containers and networks; `volumes` also removes its named
    /// volumes (lane clean).
    pub async fn down(&self, project: &str, volumes: bool, timeout: Duration) -> Result<(), Error> {
        let mut args = by_name(project, ["down", "--remove-orphans"]);
        if volumes {
            args.push("--volumes".into());
        }
        self.run(Path::new("/"), args, &BTreeMap::new(), timeout)
            .await
            .map(drop)
    }

    /// Looks up containers by exact name. Missing names are skipped.
    pub async fn inspect_containers(
        &self,
        names: &[String],
    ) -> Result<Vec<ExistingContainer>, Error> {
        if names.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<OsString> = vec!["container".into(), "inspect".into(), "--".into()];
        args.extend(names.iter().map(OsString::from));
        let command = display(&args);
        // `inspect` exits 1 when any name is missing but still prints the found ones.
        let output = self
            .spawn(Path::new("/"), &args, &BTreeMap::new(), QUERY_TIMEOUT)
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !output.status.success() && !stderr.contains("No such") {
            return Err(Error::Command {
                command,
                status: output.status.to_string(),
                stderr: stderr.trim().to_owned(),
            });
        }
        parse_inspect(&stdout).map_err(|source| Error::Json { command, source })
    }

    async fn run(
        &self,
        cwd: &Path,
        args: Vec<OsString>,
        env: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<String, Error> {
        let output = self.spawn(cwd, &args, env, timeout).await?;
        if !output.status.success() {
            return Err(Error::Command {
                command: display(&args),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            });
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    async fn spawn(
        &self,
        cwd: &Path,
        args: &[OsString],
        env: &BTreeMap<String, String>,
        timeout: Duration,
    ) -> Result<std::process::Output, Error> {
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // A timed-out compose call is killed instead of outliving the request.
            .kill_on_drop(true);
        if let Some(base) = &self.base_env {
            command.env_clear().envs(base);
        }
        for variable in OVERRIDDEN_VARIABLES {
            command.env_remove(variable);
        }
        command.envs(&self.extra_env).envs(env);
        // The macros bring tracing's own `display` into scope, so name it first.
        let shown = display(args);
        // Keys only: the values can be secrets.
        tracing::debug!(
            command = %shown,
            cwd = %cwd.display(),
            env_keys = %env.keys().map(String::as_str).collect::<Vec<_>>().join(","),
            "running"
        );
        let started = std::time::Instant::now();
        let result = match tokio::time::timeout(timeout, command.output()).await {
            Ok(output) => Ok(output?),
            Err(_) => Err(Error::Timeout {
                command: shown.clone(),
                timeout,
            }),
        };
        if let Ok(output) = &result {
            // Compose writes its progress to stderr; stdout carries data (`ps`,
            // `config`) that can hold resolved environment values, so it stays out.
            for line in String::from_utf8_lossy(&output.stderr).lines() {
                tracing::debug!(line, "compose output");
            }
            tracing::debug!(
                command = %shown,
                status = %output.status,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "finished"
            );
        }
        result
    }
}

/// Arguments for subcommands that only need the project name.
fn by_name<const N: usize>(project: &str, tail: [&str; N]) -> Vec<OsString> {
    let mut args: Vec<OsString> = vec!["compose".into(), "-p".into(), project.into()];
    args.extend(tail.iter().map(OsString::from));
    args
}

/// Human-readable command for errors. It never includes environment values.
fn display(args: &[OsString]) -> String {
    let mut text = String::from("docker");
    for arg in args {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text
}

#[derive(Deserialize)]
struct InspectEntry {
    #[serde(rename = "Id")]
    id: String,
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Config")]
    config: Option<InspectConfig>,
}

#[derive(Deserialize)]
struct InspectConfig {
    #[serde(rename = "Labels", default)]
    labels: Option<BTreeMap<String, String>>,
}

/// Parses `docker container inspect` output; Docker prefixes names with `/`.
fn parse_inspect(stdout: &str) -> Result<Vec<ExistingContainer>, serde_json::Error> {
    if stdout.trim().is_empty() {
        return Ok(Vec::new());
    }
    let entries: Vec<InspectEntry> = serde_json::from_str(stdout)?;
    Ok(entries
        .into_iter()
        .map(|entry| ExistingContainer {
            id: entry.id,
            name: entry.name.trim_start_matches('/').to_owned(),
            labels: entry
                .config
                .and_then(|config| config.labels)
                .unwrap_or_default(),
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[OsString]) -> Vec<String> {
        args.iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "Unix path fixtures render with `\\` on Windows, ported in the Windows path step"
    )]
    fn project_args_resolve_paths_and_order_files() {
        let project = ComposeProject {
            name: Some("acme".into()),
            working_dir: "/work/acme".into(),
            files: vec!["docker-compose.yml".into(), "/abs/extra.yml".into()],
            env_file: Some(".env".into()),
            profiles: vec!["search".into()],
            env: BTreeMap::new(),
        };
        let args = strings(&project.args(&["/state/override.yaml".into()]));
        assert_eq!(
            args,
            [
                "compose",
                "-p",
                "acme",
                "-f",
                "/work/acme/docker-compose.yml",
                "-f",
                "/abs/extra.yml",
                "-f",
                "/state/override.yaml",
                "--env-file",
                "/work/acme/.env",
                "--profile",
                "search"
            ]
        );
    }

    #[test]
    #[cfg_attr(
        windows,
        ignore = "Unix path fixtures render with `\\` on Windows, ported in the Windows path step"
    )]
    fn default_name_omits_project_flag() {
        let project = ComposeProject {
            name: None,
            working_dir: "/w".into(),
            files: vec!["c.yml".into()],
            env_file: None,
            profiles: Vec::new(),
            env: BTreeMap::new(),
        };
        assert_eq!(strings(&project.args(&[])), ["compose", "-f", "/w/c.yml"]);
    }

    #[test]
    fn inspect_output_strips_slash() {
        let json = r#"[{"Id":"abc","Name":"/acme-postgres","Config":{"Labels":{"com.docker.compose.project":"acme"}}},
            {"Id":"def","Name":"/plain","Config":{"Labels":null}}]"#;
        let found = parse_inspect(json).expect("valid");
        assert_eq!(found[0].name, "acme-postgres");
        assert_eq!(found[0].labels["com.docker.compose.project"], "acme");
        assert!(found[1].labels.is_empty());
        assert!(parse_inspect("[]\n").expect("valid").is_empty());
    }

    #[test]
    fn display_omits_environment() {
        let args = by_name("acme-lane1", ["down", "--volumes"]);
        assert_eq!(
            display(&args),
            "docker compose -p acme-lane1 down --volumes"
        );
    }
}

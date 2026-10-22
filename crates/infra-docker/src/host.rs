use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::Error;

/// The engine Docker uses without `DOCKER_HOST` or a context.
#[cfg(unix)]
pub const DEFAULT_HOST: &str = "unix:///var/run/docker.sock";
/// The engine Docker uses without `DOCKER_HOST` or a context: Docker Desktop's
/// named pipe, which bollard reaches through its `npipe://` support.
#[cfg(windows)]
pub const DEFAULT_HOST: &str = "npipe:////./pipe/docker_engine";

#[derive(Deserialize)]
struct CliConfig {
    #[serde(rename = "currentContext")]
    current_context: Option<String>,
}

#[derive(Deserialize)]
struct ContextMeta {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Endpoints", default)]
    endpoints: std::collections::BTreeMap<String, ContextEndpoint>,
}

#[derive(Deserialize)]
struct ContextEndpoint {
    #[serde(rename = "Host")]
    host: Option<String>,
}

/// Resolves the engine address the way the `docker` CLI does: `DOCKER_HOST`, else the
/// `DOCKER_CONTEXT` or `currentContext` from `<config dir>/config.json`, looked up in
/// `<config dir>/contexts/meta/*/meta.json`. bollard 0.21 only reads `DOCKER_HOST`, which
/// is wrong for OrbStack, Colima and `docker context use` setups.
///
/// `env` reads environment variables; `config_dir` is `DOCKER_CONFIG` or `~/.docker`.
/// Blocking: it reads small files, so call it off the async executor.
pub fn resolve_host(
    env: impl Fn(&str) -> Option<String>,
    config_dir: Option<&Path>,
) -> Result<String, Error> {
    if let Some(host) = env("DOCKER_HOST").filter(|host| !host.is_empty()) {
        return Ok(host);
    }
    let Some(config_dir) = config_dir else {
        return Ok(DEFAULT_HOST.to_owned());
    };
    let context = match env("DOCKER_CONTEXT").filter(|context| !context.is_empty()) {
        Some(context) => Some(context),
        None => read_current_context(&config_dir.join("config.json"))?,
    };
    let Some(context) = context.filter(|context| context != "default") else {
        return Ok(DEFAULT_HOST.to_owned());
    };
    find_context_host(&config_dir.join("contexts").join("meta"), &context)?
        .ok_or(Error::UnknownContext(context))
}

/// The variable holding the user's home, where the Docker CLI keeps `.docker`.
#[cfg(unix)]
const HOME_VAR: &str = "HOME";
/// The variable holding the user's home, where the Docker CLI keeps `.docker`;
/// Windows shells may not set `HOME`, and Docker Desktop ignores it.
#[cfg(windows)]
const HOME_VAR: &str = "USERPROFILE";

/// The CLI config directory: `DOCKER_CONFIG`, else `.docker` in the user's home
/// (`$HOME` on Unix, `%USERPROFILE%` on Windows).
pub fn config_dir(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    env("DOCKER_CONFIG")
        .filter(|dir| !dir.is_empty())
        .map(PathBuf::from)
        .or_else(|| env(HOME_VAR).map(|home| PathBuf::from(home).join(".docker")))
}

fn read_current_context(path: &Path) -> Result<Option<String>, Error> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let config: CliConfig = serde_json::from_str(&text).map_err(|source| Error::DockerConfig {
        path: path.to_owned(),
        source,
    })?;
    Ok(config.current_context)
}

/// Context directories are named by a digest of the context name; scanning avoids a
/// SHA-256 dependency and there are only a handful of contexts.
fn find_context_host(meta_dir: &Path, context: &str) -> Result<Option<String>, Error> {
    let entries = match std::fs::read_dir(meta_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path().join("meta.json");
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let meta: ContextMeta =
            serde_json::from_str(&text).map_err(|source| Error::DockerConfig { path, source })?;
        if meta.name == context {
            return Ok(meta
                .endpoints
                .get("docker")
                .and_then(|endpoint| endpoint.host.clone()));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn env(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: BTreeMap<String, String> = vars
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect();
        move |key| vars.get(key).cloned()
    }

    fn docker_config() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.json"),
            r#"{"currentContext":"orbstack"}"#,
        )
        .expect("write config");
        for (digest, name, host) in [
            ("aa", "orbstack", "unix:///home/u/.orbstack/run/docker.sock"),
            ("bb", "colima", "unix:///home/u/.colima/default/docker.sock"),
        ] {
            let meta = dir.path().join("contexts/meta").join(digest);
            std::fs::create_dir_all(&meta).expect("mkdir");
            let json = format!(
                r#"{{"Name":"{name}","Metadata":{{}},"Endpoints":{{"docker":{{"Host":"{host}","SkipTLSVerify":false}}}}}}"#
            );
            std::fs::write(meta.join("meta.json"), json).expect("write meta");
        }
        dir
    }

    #[test]
    fn docker_host_wins() {
        let dir = docker_config();
        let host = resolve_host(
            env(&[("DOCKER_HOST", "tcp://1.2.3.4:2375")]),
            Some(dir.path()),
        );
        assert_eq!(host.ok().as_deref(), Some("tcp://1.2.3.4:2375"));
    }

    #[test]
    fn current_context_from_config() {
        let dir = docker_config();
        let host = resolve_host(env(&[]), Some(dir.path()));
        assert_eq!(
            host.ok().as_deref(),
            Some("unix:///home/u/.orbstack/run/docker.sock")
        );
    }

    #[test]
    fn docker_context_overrides_config() {
        let dir = docker_config();
        let host = resolve_host(env(&[("DOCKER_CONTEXT", "colima")]), Some(dir.path()));
        assert_eq!(
            host.ok().as_deref(),
            Some("unix:///home/u/.colima/default/docker.sock")
        );
        let host = resolve_host(env(&[("DOCKER_CONTEXT", "default")]), Some(dir.path()));
        assert_eq!(host.ok().as_deref(), Some(DEFAULT_HOST));
    }

    #[test]
    fn unknown_context_is_an_error() {
        let dir = docker_config();
        let host = resolve_host(env(&[("DOCKER_CONTEXT", "nope")]), Some(dir.path()));
        assert!(matches!(host, Err(Error::UnknownContext(name)) if name == "nope"));
    }

    #[test]
    fn missing_config_uses_default_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_host(env(&[]), Some(dir.path())).ok().as_deref(),
            Some(DEFAULT_HOST)
        );
        assert_eq!(
            resolve_host(env(&[]), None).ok().as_deref(),
            Some(DEFAULT_HOST)
        );
    }

    #[test]
    fn config_dir_precedence() {
        assert_eq!(
            config_dir(env(&[("DOCKER_CONFIG", "/c"), (HOME_VAR, "/h")])),
            Some(PathBuf::from("/c"))
        );
        assert_eq!(
            config_dir(env(&[(HOME_VAR, "/h")])),
            Some(Path::new("/h").join(".docker"))
        );
        assert_eq!(config_dir(env(&[])), None);
    }
}

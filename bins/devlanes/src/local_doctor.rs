//! `devlanes doctor` without a daemon: the checks this process can make on its
//! own, in the shape of the daemon's doctor data plus `running: false`. Nothing
//! here starts a daemon or prompts.

use std::path::Path;

use devlanes_client::Paths;
use serde_json::{Value, json};

/// Doctor facts gathered locally: home and socket paths, the home's permissions,
/// the user config on disk, the CA and its trust, processes on the proxy ports,
/// and the hosts helper's version when it is installed.
pub async fn facts(paths: &Paths) -> Value {
    let config_path = paths.home.join("config.yaml");
    let (user, user_config) = tokio::task::spawn_blocking(move || user_config(&config_path))
        .await
        .unwrap_or_else(|_| (devlanes_config::UserConfig::default(), Value::Null));
    let proxy_ports = proxy_ports(&user);
    let home_check = {
        let home = paths.home.clone();
        tokio::task::spawn_blocking(move || home_check(&home))
            .await
            .unwrap_or(Value::Null)
    };
    let port_holders = {
        let ports = proxy_ports.clone();
        tokio::task::spawn_blocking(move || port_holders(&ports))
            .await
            .unwrap_or_default()
    };
    let mut facts = json!({
        "running": false,
        "version": env!("CARGO_PKG_VERSION"),
        "home": paths.home,
        "socket": paths.socket,
        "db_path": paths.db,
        "home_check": home_check,
        "sandbox": {
            "codex_sandbox": std::env::var_os("CODEX_SANDBOX").is_some(),
            "app_sandbox": std::env::var_os("APP_SANDBOX_CONTAINER_ID").is_some(),
        },
        "ca": ca(paths).await,
        "user_config": user_config,
        "port_holders": port_holders,
        "hosts": hosts_helper().await,
    });
    annotate_excluded_ports(&mut facts, &user).await;
    facts
}

/// Adds `excluded_ports` as the daemon's doctor reports it, judged against
/// `ports.range` and the proxy ports in the user config on disk (the defaults
/// without a file). No box is mirrored without a daemon.
#[cfg(windows)]
async fn annotate_excluded_ports(facts: &mut Value, user: &devlanes_config::UserConfig) {
    use devlanes_daemon::excluded_ports;
    let pool = user.ports.bounds().ok();
    let reading = excluded_ports::read().await;
    facts["excluded_ports"] = excluded_ports::report(
        &reading,
        pool,
        &std::collections::BTreeSet::new(),
        &user.proxy,
    );
}

/// Windows alone reserves port ranges this way.
#[cfg(unix)]
async fn annotate_excluded_ports(_facts: &mut Value, _user: &devlanes_config::UserConfig) {}

/// Adds `windows: {build}`, since builds older than Windows 11 24H2 are not
/// supported, and `terminal: {elevated, elevation_allowed, job}` about the
/// shell running this command: an elevated shell cannot start the daemon
/// unless `DEVLANES_ALLOW_ELEVATED=1` allows it, and a job that kills its
/// processes when the terminal closes takes a daemon started inside it along.
#[cfg(windows)]
pub fn annotate_system(data: &mut Value) {
    use devlanes_sys::windows::{
        process::job_of_self,
        token::{elevated, elevation_refused},
    };
    data["windows"] = json!({"build": devlanes_sys::windows::os_build()});
    let job = job_of_self()
        .ok()
        .flatten()
        .map(|job| json!({"kill_on_close": job.kill_on_close, "breakaway_ok": job.breakaway_ok}));
    let elevated = elevated().ok();
    // Elevated but not refused means the override is set.
    let allowed = elevated == Some(true) && elevation_refused().ok() == Some(false);
    data["terminal"] = json!({
        "elevated": elevated,
        "elevation_allowed": allowed,
        "job": job,
    });
}

/// Nothing to add on Unix: the daemon leaves the terminal's session itself.
#[cfg(unix)]
pub fn annotate_system(_data: &mut Value) {}

/// `{path, exists, problem?}` for the devlanes home: the client refuses a symlink
/// or junction and another account's home, and the daemon refuses an own home
/// that other accounts can open unless it is empty (`problem: "acl"`, the
/// Windows counterpart of `mode`).
#[cfg(windows)]
fn home_check(home: &Path) -> Value {
    use devlanes_sys::windows::security::{DirProblem, check_private_dir};
    match check_private_dir(home) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({"path": home, "exists": false})
        }
        Err(error) => {
            json!({"path": home, "exists": true, "problem": "unreadable", "message": error.to_string()})
        }
        Ok(verdict) => {
            let problem = match &verdict {
                Ok(()) => None,
                Err(DirProblem::ReparsePoint) => Some("symlink"),
                Err(DirProblem::NotADirectory) => Some("not_a_directory"),
                Err(DirProblem::Owner) => Some("owner"),
                Err(DirProblem::Acl(_)) => Some("acl"),
            };
            let fix = match &verdict {
                Err(DirProblem::Acl(others)) => crate::driver::acl_fix(home, others),
                _ => None,
            };
            json!({"path": home, "exists": true, "problem": problem, "fix": fix})
        }
    }
}

/// `{path, private, mode?}` for the devlanes home: the client refuses another
/// user's or a symlinked home, and the daemon tightens an open one to 0700.
#[cfg(unix)]
fn home_check(home: &Path) -> Value {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    match std::fs::symlink_metadata(home) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            json!({"path": home, "exists": false})
        }
        Err(error) => {
            json!({"path": home, "exists": true, "problem": "unreadable", "message": error.to_string()})
        }
        Ok(meta) => {
            let mode = meta.permissions().mode() & 0o7777;
            let problem = if meta.file_type().is_symlink() {
                Some("symlink")
            } else if !meta.is_dir() {
                Some("not_a_directory")
            } else if meta.uid() != rustix::process::geteuid().as_raw() {
                Some("owner")
            } else if mode & 0o077 != 0 {
                Some("mode")
            } else {
                None
            };
            json!({"path": home, "exists": true, "mode": format!("{mode:o}"), "problem": problem})
        }
    }
}

/// The user config, parsed once (leniently; the defaults without a file), and its
/// report as the daemon's doctor gives it: `{path, valid, error, errors,
/// warnings}`; a missing file is valid. Blocking.
fn user_config(path: &Path) -> (devlanes_config::UserConfig, Value) {
    if !path.exists() {
        return (
            devlanes_config::UserConfig::default(),
            json!({"path": path, "valid": true, "error": null, "errors": [], "warnings": []}),
        );
    }
    let (config, errors) = devlanes_config::load_user_lenient(path);
    let errors: Vec<Value> = errors
        .iter()
        .map(|error| {
            json!({
                "message": error.message,
                "file": error.location.file,
                "line": error.location.line,
                "column": error.location.column,
            })
        })
        .collect();
    let report = json!({
        "path": path,
        "valid": errors.is_empty(),
        "error": errors.first(),
        "errors": errors,
        "warnings": config.warnings(),
    });
    (config, report)
}

/// `[(listener, port)]` the proxy will bind, from the user config.
fn proxy_ports(config: &devlanes_config::UserConfig) -> Vec<(&'static str, u16)> {
    vec![
        ("http", config.proxy.http_port),
        ("https", config.proxy.https_port),
    ]
}

/// Processes listening on the proxy's ports while no daemon runs: the proxy will
/// fall back to another port when it starts. `[{listener, port, pid, command}]`.
/// Blocking.
fn port_holders(ports: &[(&'static str, u16)]) -> Vec<Value> {
    let Ok(processes) = devlanes_sys::processes() else {
        return Vec::new();
    };
    let pids: Vec<u32> = processes.iter().map(|p| p.pid).collect();
    let Ok(sockets) = devlanes_sys::listening_tcp(&pids) else {
        return Vec::new();
    };
    ports
        .iter()
        .filter_map(|(listener, port)| {
            let socket = sockets.iter().find(|s| s.addr.port() == *port)?;
            let command = processes
                .iter()
                .find(|p| p.pid == socket.pid)
                .map(|p| p.name.clone());
            Some(json!({"listener": listener, "port": port, "pid": socket.pid, "command": command}))
        })
        .collect()
}

/// The CA the daemon will use, read without creating one, with its trust in
/// each configured store.
async fn ca(paths: &Paths) -> Value {
    let mkcert = crate::local::mkcert_selected(paths);
    let path = paths.home.join("ca/ca.pem");
    let mut ca = match crate::local::certificate(paths, false).await {
        Ok(Some(certificate)) => {
            let recreate = !mkcert && !certificate.is_name_constrained();
            json!({
                "present": true,
                "kind": if mkcert { "mkcert" } else { "devlanes" },
                "path": path,
                "name_constrained": certificate.is_name_constrained(),
                "hint": recreate.then_some(crate::local::UNCONSTRAINED_CA),
                "fix": recreate.then(|| crate::local::recreate_ca_command(&paths.home)),
            })
        }
        // The daemon creates its CA when it first starts.
        Ok(None) => {
            return json!({"present": false, "pending": true, "path": path});
        }
        Err(error) => json!({
            "present": false,
            "error": error.code,
            "message": error.data.get("message"),
        }),
    };
    ca["trust"] = crate::local::trust_report().await;
    ca
}

/// `{helper}` as in the daemon's hosts status, when the hosts service answers
/// (or refuses) on its pipe; `null` when it is not installed.
#[cfg(windows)]
async fn hosts_helper() -> Value {
    use devlanes_resolver_hosts::HostsClient;
    use devlanes_resolver_hosts::pipe::PipeHostsClient;
    let Ok(client) = HostsClient::service(PipeHostsClient::system(), None) else {
        return Value::Null;
    };
    let version = client
        .with_timeout(std::time::Duration::from_secs(5))
        .version()
        .await;
    if matches!(&version, Err(error) if error.is_missing()) {
        return Value::Null;
    }
    let check = devlanes_daemon::hosts::helper_check(&version, env!("CARGO_PKG_VERSION"));
    json!({"helper": check})
}

/// `{helper}` as in the daemon's hosts status, when the /etc/hosts helper is
/// installed; `null` otherwise (most machines never need it).
#[cfg(unix)]
async fn hosts_helper() -> Value {
    use devlanes_resolver_hosts::{
        FileKind, FsProbe, HostsClient, INSTALL_PATH, SystemExecutor, SystemFs,
    };
    let installed = matches!(
        SystemFs.lstat(Path::new(INSTALL_PATH)),
        Ok(Some(info)) if info.kind == FileKind::File
    );
    if !installed {
        return Value::Null;
    }
    let Ok(client) = HostsClient::new(
        std::sync::Arc::new(SystemExecutor),
        std::sync::Arc::new(SystemFs),
        None,
    ) else {
        return Value::Null;
    };
    let check =
        devlanes_daemon::hosts::helper_check(&client.version().await, env!("CARGO_PKG_VERSION"));
    json!({"helper": check})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home others can read is a finding; a missing one is not (the first
    /// command creates it).
    #[cfg(unix)]
    #[test]
    fn home_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("h");
        assert_eq!(home_check(&home)["exists"], false);
        std::fs::create_dir(&home).expect("home");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(home_check(&home)["problem"], "mode");
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).expect("chmod");
        assert_eq!(home_check(&home)["problem"], Value::Null);
    }

    /// The user config is checked on disk: an invalid key is an error with its line.
    #[test]
    fn user_config_on_disk() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("config.yaml");
        let (missing, checked) = user_config(&path);
        assert_eq!(checked["valid"], true);
        assert_eq!(proxy_ports(&missing), [("http", 80), ("https", 443)]);
        std::fs::write(&path, "proxy:\n  http_port: nope\n").expect("config");
        let (_, checked) = user_config(&path);
        assert_eq!(checked["valid"], false, "{checked}");
        assert!(checked["error"]["line"].is_u64(), "{checked}");
        std::fs::write(&path, "proxy:\n  https_port: 9443\n").expect("config");
        let (config, checked) = user_config(&path);
        assert_eq!(checked["valid"], true, "{checked}");
        assert_eq!(proxy_ports(&config), [("http", 80), ("https", 9443)]);
    }

    /// Facts about a fresh home: not running, nothing created.
    #[tokio::test]
    async fn facts_without_a_daemon_create_nothing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("h");
        let paths = Paths::new(&home).expect("paths");
        let facts = facts(&paths).await;
        assert_eq!(facts["running"], false);
        assert_eq!(facts["ca"]["present"], false, "{facts}");
        assert!(!home.exists(), "doctor created {}", home.display());
    }
}

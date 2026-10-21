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
    let home = paths.home.clone();
    let config_path = home.join("config.yaml");
    let (user_config, proxy_ports) =
        tokio::task::spawn_blocking(move || (user_config(&config_path), proxy_ports(&config_path)))
            .await
            .unwrap_or((Value::Null, Vec::new()));
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
    json!({
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
    })
}

/// `{path, private, mode?}` for the devlanes home: the client refuses another
/// user's or a symlinked home, and the daemon tightens an open one to 0700.
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

/// The user config as the daemon's doctor reports it: `{path, valid, error,
/// errors, warnings}`; a missing file is valid. Blocking.
fn user_config(path: &Path) -> Value {
    if !path.exists() {
        return json!({"path": path, "valid": true, "error": null, "errors": [], "warnings": []});
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
    json!({
        "path": path,
        "valid": errors.is_empty(),
        "error": errors.first(),
        "errors": errors,
        "warnings": config.warnings(),
    })
}

/// `[(listener, port)]` the proxy will bind, from the user config. Blocking.
fn proxy_ports(path: &Path) -> Vec<(&'static str, u16)> {
    let config = if path.exists() {
        devlanes_config::load_user_lenient(path).0
    } else {
        devlanes_config::UserConfig::default()
    };
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

/// `{helper}` as in the daemon's hosts status, when the /etc/hosts helper is
/// installed; `null` otherwise (most machines never need it).
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
    use std::os::unix::fs::PermissionsExt;

    /// A home others can read is a finding; a missing one is not (the first
    /// command creates it).
    #[test]
    fn home_permissions() {
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
        assert_eq!(user_config(&path)["valid"], true);
        std::fs::write(&path, "proxy:\n  http_port: nope\n").expect("config");
        let checked = user_config(&path);
        assert_eq!(checked["valid"], false, "{checked}");
        assert!(checked["error"]["line"].is_u64(), "{checked}");
        assert_eq!(
            proxy_ports(&temp.path().join("missing.yaml")),
            [("http", 80), ("https", 443)]
        );
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

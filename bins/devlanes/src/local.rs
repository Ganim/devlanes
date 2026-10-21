//! Actions that must run in the user's terminal, never in the daemon: CA trust, the
//! hosts helper and the OS resolver file (they prompt for a password) and
//! `secrets unlock` (fnox asks for the vault there).

use std::{collections::BTreeSet, net::SocketAddr, str::FromStr, sync::Arc};

use devlanes_cli::{Action, Rendered, Runtime};
use devlanes_client::{Client, Paths};
use devlanes_proto::{Context, Method, RpcError};
use devlanes_resolver_dns::ResolverFile;
use devlanes_resolver_hosts::CommandSpec;
use devlanes_trust::{CaCertificate, TrustStatus, TrustStore, TrustTarget};
use serde_json::json;

use crate::{driver::rpc_error, output::emit};

/// Name shown in keychains and NSS databases.
const CA_NAME: &str = "devlanes development CA";

/// Routes the terminal-bound actions.
pub async fn run(runtime: &Runtime, action: Action) -> Result<u8, RpcError> {
    match action {
        Action::Trust { hosts, resolver } => {
            let mut code = trust(runtime, true).await?;
            if hosts {
                code = code.max(crate::hosts::install(runtime).await?);
            }
            if resolver {
                code = code.max(resolver_install(runtime, true).await?);
            }
            Ok(code)
        }
        // Mirrors `certs trust --hosts`: the helper and resolver file go first, so a CA
        // removal failure (keychain prompt cancelled) still leaves them uninstalled.
        Action::Untrust {
            hosts,
            resolver,
            keep_ca,
        } => {
            let mut code = 0;
            if hosts {
                code = crate::hosts::uninstall(runtime).await?;
            }
            if resolver {
                code = code.max(resolver_install(runtime, false).await?);
            }
            if keep_ca {
                return Ok(code);
            }
            Ok(code.max(trust(runtime, false).await?))
        }
        Action::Unlock(context) => unlock(context).await,
        action @ (Action::Tunnel | Action::Askpass(_)) => crate::remote::run(action).await,
        _ => Err(RpcError::new(
            "usage",
            json!({"message": "unexpected action"}),
            2,
        )),
    }
}

/// Loads the same CA the daemon uses (its own, or mkcert's when configured).
async fn certificate(paths: &Paths) -> Result<CaCertificate, RpcError> {
    let config = devlanes_config::load_user(paths.home.join("config.yaml")).unwrap_or_default();
    let mkcert = matches!(config.tls.ca, devlanes_config::CaKind::Mkcert);
    let home = paths.home.clone();
    let pem = tokio::task::spawn_blocking(move || -> Result<String, String> {
        let ca = if mkcert {
            let output = std::process::Command::new("mkcert")
                .arg("-CAROOT")
                .output()
                .map_err(|error| format!("mkcert: {error}"))?;
            let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            devlanes_pki::CertificateAuthority::from_mkcert(std::path::Path::new(&root))
        } else {
            devlanes_pki::CertificateAuthority::load_or_create(&home.join("ca"))
        }
        .map_err(|error| error.to_string())?;
        Ok(ca.certificate_pem().to_owned())
    })
    .await
    .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
    .map_err(|message| RpcError::new("ca_unavailable", json!({"message": message}), 1))?;
    CaCertificate::from_pem(&pem, CA_NAME)
        .map_err(|error| RpcError::new("ca_invalid", json!({"message": error.to_string()}), 1))
}

/// Configured trust targets, defaulting to the system store; mobile targets are refused.
fn targets(paths: &Paths) -> Vec<TrustTarget> {
    let config = devlanes_config::load_user(paths.home.join("config.yaml")).unwrap_or_default();
    let configured: Vec<TrustTarget> = config
        .tls
        .trust
        .iter()
        .filter_map(|target| serde_json::to_value(target).ok())
        .filter_map(|value| {
            value
                .as_str()
                .and_then(|name| TrustTarget::from_str(name).ok())
        })
        .collect();
    if configured.is_empty() {
        vec![TrustTarget::System]
    } else {
        configured
    }
}

/// The OS decides the store implementation at compile time.
fn store(target: TrustTarget) -> Result<Box<dyn TrustStore>, devlanes_trust::TrustError> {
    #[cfg(target_os = "macos")]
    {
        devlanes_trust_macos::MacosTrust::from_env()?.store(target)
    }
    #[cfg(not(target_os = "macos"))]
    {
        devlanes_trust_linux::LinuxTrust::from_env()?.store(target)
    }
}

/// Installs or removes the CA in every configured store; one failure doesn't skip the rest.
async fn trust(runtime: &Runtime, install: bool) -> Result<u8, RpcError> {
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    let certificate = certificate(&paths).await?;
    let mut done = BTreeSet::new();
    let mut failed = false;
    let mut lines = String::new();
    for target in targets(&paths) {
        let result = async {
            target.ensure_supported()?;
            let store: Arc<dyn TrustStore> = Arc::from(store(target)?);
            // System and Chrome share the macOS login keychain; touch it once.
            if !done.insert(store.id()) {
                return Ok(None);
            }
            if install {
                store.install(&certificate).await?;
            } else {
                store.uninstall(&certificate).await?;
            }
            store.status(&certificate).await.map(Some)
        }
        .await;
        match result {
            Ok(None) => {}
            Ok(Some(status)) => {
                let word = match status {
                    TrustStatus::Installed => "trusted",
                    TrustStatus::Missing => "untrusted",
                    TrustStatus::Unknown { .. } => "unknown",
                };
                lines.push_str(&format!("{word} {target}\n"));
            }
            Err(error) => {
                failed = true;
                lines.push_str(&format!("failed {target}: {error}\n"));
            }
        }
    }
    let _ = runtime;
    emit(&Rendered {
        stdout: lines,
        ..Rendered::default()
    });
    Ok(u8::from(failed))
}

/// The OS resolver file that sends `tld` lookups to the daemon's DNS server at
/// `server`. Blocking: on Linux it asks `systemctl` for the systemd version.
pub fn resolver_file(tld: &str, server: SocketAddr) -> Result<ResolverFile, String> {
    let zone = devlanes_resolver_dns::Zone::new(tld).map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    {
        Ok(devlanes_resolver_dns::macos_resolver(&zone, server))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let output = std::process::Command::new("systemctl")
            .arg("--version")
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| format!("the resolver strategy needs systemd-resolved: {error}"))?;
        let version = systemd_version(&String::from_utf8_lossy(&output.stdout))
            .ok_or("cannot read the systemd version from `systemctl --version`")?;
        devlanes_resolver_dns::systemd_resolver(&zone, server, version)
            .map_err(|error| error.to_string())
    }
}

/// The version from `systemctl --version`'s first line, `systemd 255 (255.4-1)`.
#[cfg(any(not(target_os = "macos"), test))]
fn systemd_version(output: &str) -> Option<u32> {
    let mut words = output.lines().next()?.split_whitespace();
    (words.next()? == "systemd").then_some(())?;
    words.next()?.parse().ok()
}

/// `certs trust --resolver` writes the OS resolver file for `hostnames.tld` with
/// sudo; `certs untrust --resolver` removes it. Either way the OS is told to reload.
async fn resolver_install(runtime: &Runtime, install: bool) -> Result<u8, RpcError> {
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    let config = devlanes_config::load_user(paths.home.join("config.yaml")).unwrap_or_default();
    let hostnames = config.hostnames;
    let tld = (hostnames.resolver == devlanes_config::Resolver::Resolver)
        .then_some(hostnames.tld)
        .flatten()
        .filter(|tld| !tld.is_empty())
        .ok_or_else(|| {
            RpcError::new(
                "resolver_not_configured",
                json!({"message": "set hostnames.resolver: resolver and hostnames.tld in the user config first"}),
                2,
            )
        })?;
    // The daemon listens on the default address; there is no setting for it.
    let server = devlanes_resolver_dns::default_listen();
    let lookup = tld.clone();
    let file = tokio::task::spawn_blocking(move || resolver_file(&lookup, server))
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
        .map_err(|message| RpcError::new("resolver_unsupported", json!({"message": message}), 2))?;
    let command = if install {
        "devlanes certs trust --resolver"
    } else {
        "devlanes certs untrust --resolver"
    };
    let executor = crate::hosts::privileged(runtime, command).await?;
    let path = file.path.display().to_string();
    if runtime.can_prompt() {
        emit(&Rendered {
            stderr: format!("updating {path}; sudo may ask for your password\n"),
            ..Rendered::default()
        });
    }
    let sudo = || CommandSpec::new("sudo");
    let mut steps = Vec::new();
    if install {
        if let Some(parent) = file.path.parent() {
            steps.push(
                sudo()
                    .args(["mkdir", "-p"])
                    .arg(parent.display().to_string()),
            );
        }
        steps.push(
            sudo()
                .args(["tee", path.as_str()])
                .stdin(file.contents.into_bytes()),
        );
        steps.push(sudo().args(["chmod", "644", path.as_str()]));
    } else {
        steps.push(sudo().args(["rm", "-f", path.as_str()]));
    }
    steps.extend(file.activate.map(|activate| sudo().args(activate)));
    for step in steps {
        let failure = match executor.run(&step).await {
            Ok(output) if output.success() => continue,
            Ok(output) => output.stderr.trim().to_owned(),
            Err(error) => error.to_string(),
        };
        return Err(RpcError::new(
            "resolver_install_failed",
            json!({"message": format!("`{step}` failed: {failure}"), "command": step.to_string(), "stderr": failure}),
            1,
        ));
    }
    let stdout = if install {
        format!("wrote {path}: *.{tld} resolves through the devlanes DNS server at {server}\n")
    } else {
        format!("removed {path}\n")
    };
    emit(&Rendered {
        stdout,
        ..Rendered::default()
    });
    Ok(0)
}

/// Runs fnox in this terminal so it can prompt, then hands the values to the daemon,
/// which keeps them in memory only.
async fn unlock(context: Context) -> Result<u8, RpcError> {
    let output = tokio::process::Command::new("fnox")
        .args(["env", "--json"])
        .current_dir(&context.cwd)
        .stdin(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|error| {
            RpcError::new("fnox_unavailable", json!({"message": error.to_string()}), 1)
        })?;
    if !output.status.success() {
        return Err(RpcError::new(
            "fnox_failed",
            json!({"code": output.status.code()}),
            1,
        ));
    }
    let secrets = devlanes_secrets_fnox::parse_document(&output.stdout)
        .map_err(|error| RpcError::new("fnox_output", json!({"message": error.to_string()}), 1))?;
    let paths = Paths::discover()
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    let mut client = Client::connect_or_spawn(&paths).await.map_err(rpc_error)?;
    let count = secrets.set.len();
    client
        .request(
            Method::Unlock,
            json!({"env": secrets.set, "remove": secrets.remove, "profile": null}),
            context,
        )
        .await
        .map_err(rpc_error)?;
    emit(&Rendered {
        stdout: format!("unlocked {count} secrets\n"),
        ..Rendered::default()
    });
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// The systemd version picks the resolved file form.
    fn systemd_version_from_systemctl() {
        assert_eq!(
            systemd_version("systemd 255 (255.4-1ubuntu8)\n+PAM +AUDIT"),
            Some(255)
        );
        assert_eq!(systemd_version("systemd 258\n"), Some(258));
        assert_eq!(systemd_version("elogind 252\n"), None);
        assert_eq!(systemd_version(""), None);
    }

    #[cfg(target_os = "macos")]
    #[test]
    /// macOS gets `/etc/resolver/<tld>` pointing at the daemon's server.
    fn macos_resolver_file() {
        let file = resolver_file("test", devlanes_resolver_dns::default_listen()).expect("file");
        assert_eq!(file.path, std::path::Path::new("/etc/resolver/test"));
        assert!(file.contents.contains("port 15353"));
    }
}

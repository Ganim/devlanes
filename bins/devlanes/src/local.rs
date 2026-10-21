//! Actions that must run in the user's terminal, never in the daemon: CA trust (it prompts
//! for a password) and `unlock` (fnox asks for the vault there).

use std::{collections::BTreeSet, str::FromStr, sync::Arc};

use devlanes_cli::{Action, Rendered, Runtime};
use devlanes_client::{Client, Paths};
use devlanes_proto::{Context, Method, RpcError};
use devlanes_trust::{CaCertificate, TrustStatus, TrustStore, TrustTarget};
use serde_json::json;

use crate::{driver::rpc_error, output::emit};

/// Name shown in keychains and NSS databases.
const CA_NAME: &str = "devlanes development CA";

/// Routes the terminal-bound actions.
pub async fn run(runtime: &Runtime, action: Action) -> Result<u8, RpcError> {
    match action {
        Action::Trust { hosts: true } => Err(RpcError::new(
            "not_supported_yet",
            json!({"feature": "hosts helper", "reason": "awaiting design approval (plan step 9.5)"}),
            2,
        )),
        Action::Trust { hosts: false } => trust(runtime, true).await,
        Action::Untrust => trust(runtime, false).await,
        Action::Unlock(context) => unlock(context).await,
        Action::Tunnel | Action::Askpass(_) => Err(RpcError::new(
            "not_supported_yet",
            json!({"milestone": "M14"}),
            2,
        )),
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

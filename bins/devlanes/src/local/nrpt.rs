//! `certs trust --resolver` and `certs untrust --resolver` on Windows: one NRPT
//! rule from `devlanes-resolver-nrpt` replaces the resolver file, written by
//! `reg.exe` after the UAC prompt. The registry itself records what was
//! installed (`devlanes-<tld>` keys), so there is no record file.

use std::net::SocketAddr;

use devlanes_cli::{Rendered, Runtime};
use devlanes_client::Paths;
use devlanes_proto::RpcError;
use devlanes_resolver_dns::Zone;
use devlanes_resolver_nrpt::{Nrpt, NrptError, RuleState};
use serde_json::{Value, json};

use crate::output::emit;

/// `certs trust --resolver` writes the rule for `hostnames.tld` (and removes
/// devlanes rules for an earlier TLD); `certs untrust --resolver` removes every
/// devlanes rule. Nothing is prompted when nothing changes, and a session that
/// cannot answer the UAC prompt gets `terminal_required` before it.
pub(super) async fn resolver_install(
    runtime: &Runtime,
    paths: &Paths,
    install: bool,
) -> Result<u8, RpcError> {
    let (tld, config_error) = match crate::user_config::load(paths).await {
        Ok(config) => {
            let hostnames = config.hostnames;
            let tld = (hostnames.resolver == devlanes_config::Resolver::Resolver)
                .then_some(hostnames.tld)
                .flatten();
            (tld, None)
        }
        Err(error) if install => return Err(error),
        // Removal deletes every devlanes rule, whatever the config says.
        Err(error) => (None, Some(error)),
    };
    let zone = match (install, tld) {
        (false, _) => None,
        (true, None) => return Err(super::not_configured()),
        (true, Some(tld)) => Some(Zone::new(&tld).map_err(|error| {
            RpcError::new(
                "resolver_unsupported",
                json!({"message": error.to_string()}),
                2,
            )
        })?),
    };
    let nrpt = Nrpt::default();
    let plan = {
        let (nrpt, zone) = (nrpt.clone(), zone.clone());
        blocking(move || match &zone {
            Some(zone) => nrpt.plan_install(zone),
            None => nrpt.plan_removal(),
        })
        .await?
        .map_err(nrpt_error)?
    };
    let server = devlanes_resolver_dns::default_listen();
    // As on Unix, a broken config is only passed over when there is a rule to remove.
    if let (None, Some(error)) = (&plan, config_error) {
        return Err(error);
    }
    let Some(change) = plan else {
        let stdout = match &zone {
            Some(zone) => format!(
                "{} already sends *.{} lookups to the devlanes DNS server at {server}\n",
                nrpt.key_path(zone),
                zone.name()
            ),
            None => "no devlanes NRPT rule to remove\n".into(),
        };
        if !runtime.quiet {
            emit(&Rendered {
                stdout,
                ..Rendered::default()
            });
        }
        return Ok(0);
    };
    let command = if install {
        "devlanes certs trust --resolver"
    } else {
        "devlanes certs untrust --resolver"
    };
    // An elevated process gets no prompt, the counterpart of `sudo -n` working.
    let elevated = devlanes_sys_windows::token::elevated().unwrap_or(false);
    if !runtime.can_prompt() && !elevated {
        return Err(terminal_required(command));
    }
    let keys: Vec<String> = change
        .removes()
        .iter()
        .chain(change.installs())
        .map(|zone| nrpt.key_path(zone))
        .collect();
    if runtime.can_prompt() {
        emit(&Rendered {
            stderr: format!(
                "updating {}; Windows asks for administrator approval\n",
                keys.join(", ")
            ),
            ..Rendered::default()
        });
    }
    let home = paths.home.clone();
    let applied = {
        let (nrpt, change) = (nrpt.clone(), change.clone());
        blocking(move || {
            private_home(&home)?;
            nrpt.apply(&change, &home).map_err(|error| match error {
                NrptError::NoDesktop => terminal_required(command),
                error => nrpt_error(error),
            })
        })
        .await?
    };
    applied?;
    let mut stdout: String = change
        .removes()
        .iter()
        .map(|zone| format!("removed {}\n", nrpt.key_path(zone)))
        .collect();
    if let Some(zone) = change.installs() {
        stdout.push_str(&format!(
            "wrote {}: *.{} resolves through the devlanes DNS server at {server}\n",
            nrpt.key_path(zone),
            zone.name()
        ));
    }
    if runtime.quiet {
        stdout.clear();
    }
    emit(&Rendered {
        stdout,
        ..Rendered::default()
    });
    Ok(0)
}

/// `doctor`'s resolver fact on Windows, in the resolver file's shape plus
/// `kind: nrpt`: `{kind, path, state, installed, matches, foreign,
/// group_policy}`, or `{kind, error}` when the registry cannot be read.
/// `matches` also needs the server on the address the rule names. Registry
/// reads only, so it is safe on doctor's polled path.
pub fn doctor_status(tld: &str, server: SocketAddr) -> Value {
    let zone = match Zone::new(tld) {
        Ok(zone) => zone,
        Err(error) => return json!({"kind": "nrpt", "error": error.to_string()}),
    };
    match Nrpt::default().status(&zone) {
        Ok(status) => json!({
            "kind": "nrpt",
            "path": status.key,
            "state": status.state.as_str(),
            "installed": status.state != RuleState::Missing,
            "matches": status.state == RuleState::Installed
                && server == devlanes_resolver_dns::default_listen(),
            "foreign": status.foreign,
            "group_policy": status.group_policy,
        }),
        Err(error) => json!({
            "kind": "nrpt",
            "error": format!("cannot read the NRPT: {error}"),
        }),
    }
}

/// Runs registry and file work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, RpcError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))
}

/// Creates the devlanes home private when it is missing, since the `.reg`
/// file is written there; a home others can reach is refused as the client
/// refuses it.
fn private_home(home: &std::path::Path) -> Result<(), RpcError> {
    let io = |error: std::io::Error| {
        RpcError::new(
            "io",
            json!({"message": format!("{}: {error}", home.display())}),
            1,
        )
    };
    if let Some(parent) = home.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    match devlanes_sys_windows::security::ensure_private_dir(home).map_err(io)? {
        Ok(_) => Ok(()),
        Err(_) => Err(crate::driver::unsafe_directory(home)),
    }
}

/// The error for a session that cannot answer the UAC prompt.
fn terminal_required(command: &str) -> RpcError {
    RpcError::new(
        "terminal_required",
        json!({
            "command": command,
            "reason": "Windows asks for administrator approval and this session cannot answer it",
        }),
        2,
    )
}

/// An NRPT failure in the CLI's error shape, keeping its stable code.
fn nrpt_error(error: NrptError) -> RpcError {
    let exit = match error {
        NrptError::InvalidTld(_) => 2,
        _ => 1,
    };
    RpcError::new(error.code(), json!({"message": error.to_string()}), exit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Doctor's fact for a TLD without a rule: missing, never an error, read
    /// from the registry alone.
    #[test]
    fn doctor_reads_a_missing_rule() {
        let status = doctor_status(
            "devlanes-doctor-check.test",
            devlanes_resolver_dns::default_listen(),
        );
        assert_eq!(status["kind"], "nrpt", "{status}");
        assert_eq!(status["state"], "missing", "{status}");
        assert_eq!(status["installed"], false);
        assert_eq!(status["matches"], false);
        assert!(
            status["path"].as_str().is_some_and(
                |path| path.ends_with(r"DnsPolicyConfig\devlanes-devlanes-doctor-check.test")
            ),
            "{status}"
        );
        assert!(
            doctor_status("example.com", devlanes_resolver_dns::default_listen())["error"]
                .is_string()
        );
    }

    #[test]
    fn errors_keep_their_codes() {
        let error = nrpt_error(NrptError::Cancelled);
        assert_eq!(
            (error.code.as_str(), error.exit_code),
            ("elevation_cancelled", 1)
        );
        let error = terminal_required("devlanes certs trust --resolver");
        assert_eq!(error.code, "terminal_required");
        assert_eq!(error.exit_code, 2);
    }
}

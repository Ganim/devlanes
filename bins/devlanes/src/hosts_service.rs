//! Windows `devlanes certs trust --hosts` and `certs untrust --hosts`: install or
//! remove the `devlanes-hosts` service through one UAC prompt, then make the
//! daemon retry at once.
//!
//! The prompt starts this same binary elevated and hidden with a hidden
//! subcommand ([`INSTALL_ARG`], [`UNINSTALL_ARG`]), which `main` dispatches to
//! [`elevated_main`] before any argument parsing. The elevated child does the
//! work in `devlanes_hosts_service::install`; its console is hidden, so a
//! failure comes back as its exit code, which this side turns into a message.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use devlanes_cli::{Rendered, Runtime};
use devlanes_hosts_service::config::{SERVICE_BINARY, USER_SID_ARG};
use devlanes_hosts_service::install::{self, InstallError};
use devlanes_proto::RpcError;
use devlanes_resolver_hosts::HostsClient;
use devlanes_resolver_hosts::pipe::PipeHostsClient;
use devlanes_sys::windows::token::{self, Sid};
use serde_json::{Value, json};

use crate::output::emit;

/// Hidden subcommand: install or update the service (elevated).
pub const INSTALL_ARG: &str = "__install-hosts-service";

/// Hidden subcommand: remove the service (elevated).
pub const UNINSTALL_ARG: &str = "__uninstall-hosts-service";

/// Bound on the hosts service's answers seen from the CLI.
const SERVICE_TIMEOUT: Duration = Duration::from_secs(10);

/// The service binary to install: `devlanes-hosts-service.exe` next to this
/// binary, as `devlanesw.exe` is.
pub fn service_source(current_exe: std::io::Result<PathBuf>) -> Result<PathBuf, RpcError> {
    let missing = |looked: String| {
        RpcError::new(
            "service_source_missing",
            json!({"message": format!(
                "cannot find {SERVICE_BINARY} (looked in {looked}); it ships next to devlanes.exe, or build it with `cargo build -p devlanes-hosts-service` next to devlanes"
            )}),
            1,
        )
    };
    let exe =
        current_exe.map_err(|error| missing(format!("the directory of devlanes.exe: {error}")))?;
    let source = exe
        .parent()
        .map(|directory| directory.join(SERVICE_BINARY))
        .ok_or_else(|| missing(exe.display().to_string()))?;
    if source.is_file() {
        Ok(source)
    } else {
        Err(missing(source.display().to_string()))
    }
}

/// The UAC prompt needs a terminal the user watches and a visible desktop;
/// agents, CI, SSH sessions and services have neither. `command` is what to
/// run from a terminal instead. Shared with the NRPT resolver install.
pub(crate) fn require_prompt(runtime: &Runtime, command: &str) -> Result<(), RpcError> {
    // An unreadable window station counts as visible, as the trust store does.
    let desktop = devlanes_sys::windows::cert::interactive_desktop().unwrap_or(true);
    if runtime.can_prompt() && desktop {
        return Ok(());
    }
    Err(RpcError::new(
        "terminal_required",
        json!({
            "command": command,
            "reason": "Windows asks for administrator approval and this session cannot show the prompt",
        }),
        2,
    ))
}

/// Installs or updates the service for this user, then asks a running daemon
/// to reapply.
pub async fn install(runtime: &Runtime) -> Result<u8, RpcError> {
    require_prompt(runtime, "devlanes certs trust --hosts")?;
    // Checked here too, so a missing binary fails before the prompt.
    service_source(std::env::current_exe())?;
    let user = user_sid()?;
    emit(&Rendered {
        stderr: "installing the devlanes hosts service; Windows asks for administrator approval\n"
            .into(),
        ..Rendered::default()
    });
    // A SID is `S-1-` and digits and dashes, so it needs no quoting.
    elevate(format!("{INSTALL_ARG} {USER_SID_ARG} {user}")).await?;
    // The approving administrator may be another account, so the check that the
    // service answers this user happens here, unelevated.
    let version = service_client()?.version().await.map_err(|error| {
        RpcError::new(
            "hosts_install_unverified",
            json!({"message": format!("the service was installed but does not answer this user: {error}")}),
            1,
        )
    })?;
    let mut stdout = format!("installed the devlanes hosts service {}\n", version.version);
    let mut code = 0;
    if let Some(hosts) = crate::hosts::retry_daemon().await {
        match hosts.get("state").and_then(Value::as_str) {
            Some("applied") => stdout.push_str(&format!(
                "the daemon wrote {} names to the hosts file\n",
                hosts.get("names").and_then(Value::as_u64).unwrap_or(0)
            )),
            Some("failed") => {
                code = 1;
                let message = hosts
                    .pointer("/last_error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                stdout.push_str(&format!(
                    "the daemon could not update the hosts file: {message}\n"
                ));
            }
            _ => {}
        }
    }
    emit(&Rendered {
        stdout,
        ..Rendered::default()
    });
    Ok(code)
}

/// Clears the block (as this user, over the pipe), then removes the service and
/// its files through the UAC prompt.
pub async fn uninstall(runtime: &Runtime) -> Result<u8, RpcError> {
    let installed = tokio::task::spawn_blocking(install::service_state)
        .await
        .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
        .map_err(|error: InstallError| {
            RpcError::new(error.code(), json!({"message": error.to_string()}), 1)
        })?
        .is_some();
    let directory = devlanes_hosts_service::config::install_dir().ok();
    let files = directory.as_deref().is_some_and(Path::exists);
    if !installed && !files {
        emit(&Rendered {
            stdout: "the devlanes hosts service is not installed\n".into(),
            ..Rendered::default()
        });
        return Ok(0);
    }
    require_prompt(runtime, "devlanes certs untrust --hosts")?;
    let mut stderr = String::new();
    if let Err(error) = service_client()?.clear().await {
        stderr.push_str(&format!(
            "warning: could not clear the devlanes block through the service ({error}); delete the lines from `# devlanes begin` to `# devlanes end` in the hosts file by hand (as administrator)\n"
        ));
    }
    elevate(UNINSTALL_ARG.to_owned()).await?;
    if let Some(directory) = directory.filter(|directory| directory.exists()) {
        stderr.push_str(&format!(
            "warning: {} was left in place; remove it as administrator\n",
            directory.display()
        ));
    }
    // The daemon's next attempt reports `service_missing` instead of a stale success.
    let _ = crate::hosts::retry_daemon().await;
    emit(&Rendered {
        stdout: "removed the devlanes hosts service\n".into(),
        stderr,
        ..Rendered::default()
    });
    Ok(0)
}

/// The client of the real service, as the daemon uses it.
fn service_client() -> Result<HostsClient, RpcError> {
    HostsClient::service(PipeHostsClient::system(), None)
        .map(|client| client.with_timeout(SERVICE_TIMEOUT))
        .map_err(|error| RpcError::new(error.code(), json!({"message": error.to_string()}), 1))
}

/// This user's SID, the account the service will answer.
fn user_sid() -> Result<String, RpcError> {
    token::current_user()
        .and_then(|user| user.to_sddl())
        .map_err(|error| {
            RpcError::new(
                "io",
                json!({"message": format!("reading this user's SID: {error}")}),
                1,
            )
        })
}

/// Runs this binary elevated with `parameters` and waits for it. A failure
/// exit code becomes the error it stands for
/// ([`install::exit_failure`]).
async fn elevate(parameters: String) -> Result<(), RpcError> {
    let failed = |message: String| {
        RpcError::new("hosts_install_step_failed", json!({"message": message}), 1)
    };
    let exe = std::env::current_exe()
        .map_err(|error| failed(format!("finding devlanes.exe: {error}")))?;
    let exit = tokio::task::spawn_blocking(move || {
        devlanes_sys::windows::shell::run_elevated(&exe, &parameters)
    })
    .await
    .map_err(|error| failed(error.to_string()))?;
    match exit {
        Ok(0) => Ok(()),
        Ok(code) => Err(match install::exit_failure(code) {
            Some((code, message)) => RpcError::new(code, json!({"message": message}), 1),
            None => failed(format!("the elevated installer exited with {code}")),
        }),
        Err(error)
            if error.raw_os_error() == Some(devlanes_sys::windows::shell::ERROR_CANCELLED) =>
        {
            Err(RpcError::new(
                "elevation_cancelled",
                json!({"message": "the administrator prompt was declined; nothing was changed"}),
                1,
            ))
        }
        Err(error) => Err(failed(format!("running the elevated installer: {error}"))),
    }
}

/// The user of `__install-hosts-service --user-sid <SID>`.
fn install_user(rest: &[OsString]) -> Option<Sid> {
    match rest {
        [flag, sid] if flag == USER_SID_ARG => Sid::parse(sid.to_str()?).ok(),
        _ => None,
    }
}

/// Entry point of the elevated child (`devlanes.exe __install-hosts-service
/// --user-sid <SID>` or `__uninstall-hosts-service`). Exits 0 on success,
/// 2 on bad arguments, and [`InstallError::exit_code`] on failure.
pub fn elevated_main(subcommand: &OsStr, rest: &[OsString]) -> ExitCode {
    let result = if subcommand == INSTALL_ARG {
        let Some(user) = install_user(rest) else {
            eprintln!("devlanes {INSTALL_ARG}: takes {USER_SID_ARG} <SID>");
            return ExitCode::from(2);
        };
        // The service binary next to this one; nothing a caller points it at.
        let Ok(exe) = std::env::current_exe() else {
            return ExitCode::from(1);
        };
        install::install(&exe.with_file_name(SERVICE_BINARY), &user).map(drop)
    } else if rest.is_empty() {
        install::uninstall()
    } else {
        eprintln!("devlanes {UNINSTALL_ARG}: takes no arguments");
        return ExitCode::from(2);
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("devlanes: {error}");
            ExitCode::from(error.exit_code())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_cli::clap::Parser as _;

    fn os(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn the_installer_takes_only_a_user_sid() {
        assert_eq!(
            install_user(&os(&["--user-sid", "S-1-5-21-1-2-3-1001"])),
            Some(Sid::parse("S-1-5-21-1-2-3-1001").expect("sid"))
        );
        for args in [
            &[][..],
            &["--user-sid"],
            &["--user-sid", "SY"],
            &["--source", "x"],
            &["--user-sid", "S-1-5-21-1-2-3-1001", "--source", "x"],
        ] {
            assert_eq!(install_user(&os(args)), None, "{args:?}");
        }
    }

    #[test]
    fn the_service_binary_is_looked_for_next_to_devlanes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("devlanes.exe");
        let error = service_source(Ok(exe.clone())).expect_err("missing");
        assert_eq!(error.code, "service_source_missing");
        assert!(
            error.data["message"]
                .as_str()
                .is_some_and(|message| message.contains(SERVICE_BINARY)),
            "{error:?}"
        );
        std::fs::write(dir.path().join(SERVICE_BINARY), b"MZ").expect("binary");
        assert_eq!(
            service_source(Ok(exe)).expect("found"),
            dir.path().join(SERVICE_BINARY)
        );
    }

    /// Agents, CI and anything without a terminal get `terminal_required`
    /// before anything else happens: no prompt, no elevation.
    #[tokio::test]
    async fn sessions_that_cannot_prompt_need_a_terminal() {
        let cli = devlanes_cli::Cli::parse_from(["devlanes", "--output", "plain"]);
        let runtime = Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime");
        assert!(!runtime.can_prompt());
        let error = install(&runtime).await.expect_err("terminal required");
        assert_eq!(error.code, "terminal_required");
        assert_eq!(error.exit_code, 2);
    }
}

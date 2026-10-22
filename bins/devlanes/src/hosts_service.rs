//! Windows `devlanes certs trust --hosts` and `certs untrust --hosts`: install or
//! remove the `devlanes-hosts` service through one UAC prompt, then make the
//! daemon retry at once.
//!
//! The prompt starts this same binary elevated and hidden with a hidden
//! subcommand ([`INSTALL_ARG`], [`UNINSTALL_ARG`]), which `main` dispatches to
//! [`elevated_main`] before any argument parsing. The elevated child does the
//! work in `devlanes_hosts_service::install` and reports back as JSON over a pipe
//! only this user can open, since its own console is hidden.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use devlanes_cli::{Rendered, Runtime};
use devlanes_hosts_service::config::SERVICE_BINARY;
use devlanes_hosts_service::install::{self, InstallError};
use devlanes_proto::RpcError;
use devlanes_resolver_hosts::HostsClient;
use devlanes_resolver_hosts::pipe::PipeHostsClient;
use devlanes_sys::windows::token::{self, Sid};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use crate::output::emit;

/// Hidden subcommand: install or update the service (elevated).
pub const INSTALL_ARG: &str = "__install-hosts-service";

/// Hidden subcommand: remove the service (elevated).
pub const UNINSTALL_ARG: &str = "__uninstall-hosts-service";

/// Prefix of the report pipes, so the elevated child writes to nothing else.
const REPORT_PREFIX: &str = r"\\.\pipe\devlanes-install-";

/// Largest report read back from the elevated child.
const MAX_REPORT_BYTES: u64 = 64 * 1024;

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
    let source = service_source(std::env::current_exe())?;
    let user = user_sid()?;
    emit(&Rendered {
        stderr: "installing the devlanes hosts service; Windows asks for administrator approval\n"
            .into(),
        ..Rendered::default()
    });
    let arguments = vec![
        OsString::from("--user-sid"),
        user.clone().into(),
        "--source".into(),
        source.into_os_string(),
    ];
    let report = elevate(INSTALL_ARG, arguments).await?;
    let mut stdout = format!(
        "installed the devlanes hosts service {}\n",
        report["version"].as_str().unwrap_or("")
    );
    // The child checks the pipe as the same user only when the approving
    // administrator is this user; checking here covers the other case too.
    let client = service_client()?;
    if let Err(error) = client.version().await {
        return Err(RpcError::new(
            "hosts_install_unverified",
            json!({"message": format!("the service was installed but does not answer this user: {error}")}),
            1,
        ));
    }
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
        .map_err(install_error)?
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
    let client = service_client()?;
    if let Err(error) = client.clear().await {
        stderr.push_str(&format!(
            "warning: could not clear the devlanes block through the service ({error}); delete the lines from `# devlanes begin` to `# devlanes end` in the hosts file by hand (as administrator)\n"
        ));
    }
    let report = elevate(UNINSTALL_ARG, Vec::new()).await?;
    for warning in report["warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            stderr.push_str(&format!("warning: {warning}\n"));
        }
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

/// Maps an installer failure to the CLI's error shape, keeping its code.
fn install_error(error: InstallError) -> RpcError {
    RpcError::new(error.code(), json!({"message": error.to_string()}), 1)
}

/// Starts this binary elevated with `subcommand`, `arguments` and a report
/// pipe, waits for it, and returns its report.
async fn elevate(subcommand: &str, mut arguments: Vec<OsString>) -> Result<Value, RpcError> {
    let failed = |message: String| {
        RpcError::new("hosts_install_step_failed", json!({"message": message}), 1)
    };
    let exe = std::env::current_exe()
        .map_err(|error| failed(format!("finding devlanes.exe: {error}")))?;
    let name = report_pipe_name();
    let server = devlanes_sys::windows::pipe::create_single_server(OsStr::new(&name))
        .map_err(|error| failed(format!("creating {name}: {error}")))?;
    arguments.push("--report".into());
    arguments.push(name.into());
    let parameters = command_line(subcommand, &arguments);
    let reader = tokio::spawn(read_report(server));
    let exit = tokio::task::spawn_blocking(move || {
        devlanes_sys::windows::shell::run_elevated_child(&exe, &parameters)
    })
    .await
    .map_err(|error| failed(error.to_string()))?;
    let exit = match exit {
        Ok(exit) => exit,
        Err(error)
            if error.raw_os_error() == Some(devlanes_sys::windows::shell::ERROR_CANCELLED) =>
        {
            reader.abort();
            return Err(RpcError::new(
                "elevation_cancelled",
                json!({"message": "the administrator prompt was declined; nothing was changed"}),
                1,
            ));
        }
        Err(error) => {
            reader.abort();
            return Err(failed(format!("running the elevated installer: {error}")));
        }
    };
    // The child wrote its report before exiting; a child that never connected
    // (it could not open the pipe) leaves only its exit code. A report from any
    // other process (one that found the name first) is ignored.
    let report = match tokio::time::timeout(Duration::from_secs(2), reader).await {
        Ok(Ok(Some((pid, report)))) if pid == exit.pid => report,
        _ => Value::Null,
    };
    let exit = exit.code;
    if exit == 0 {
        return Ok(report);
    }
    let code = report["code"]
        .as_str()
        .unwrap_or("hosts_install_step_failed");
    let message = report["message"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| format!("the elevated installer exited with {exit}"));
    Err(RpcError::new(code, json!({"message": message}), 1))
}

/// A fresh report pipe name for this process.
fn report_pipe_name() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or_default();
    format!("{REPORT_PREFIX}{}-{nanos}", std::process::id())
}

/// Waits for the elevated child and reads its JSON report, with the PID of the
/// process that sent it, for the caller to compare with the child's.
async fn read_report(
    mut server: tokio::net::windows::named_pipe::NamedPipeServer,
) -> Option<(u32, Value)> {
    server.connect().await.ok()?;
    let pid = devlanes_sys::windows::pipe::client_process_id(&server).ok()?;
    let mut bytes = Vec::new();
    (&mut server)
        .take(MAX_REPORT_BYTES)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    Some((pid, serde_json::from_slice(&bytes).ok()?))
}

/// The command line for `subcommand` and `arguments`, quoted the way the Rust
/// runtime (and every MSVC program) splits it.
fn command_line(subcommand: &str, arguments: &[OsString]) -> String {
    let mut line = subcommand.to_owned();
    for argument in arguments {
        line.push(' ');
        line.push_str(&quote(&argument.to_string_lossy()));
    }
    line
}

/// One argument quoted for `CommandLineToArgvW` rules: quotes around it when it
/// has spaces, tabs or quotes; backslashes doubled before a quote.
fn quote(argument: &str) -> String {
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        return argument.to_owned();
    }
    let mut quoted = String::from('"');
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            other => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(other);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

/// Parsed arguments of the elevated child.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Elevated {
    Install {
        user: String,
        source: PathBuf,
        report: Option<String>,
    },
    Uninstall {
        report: Option<String>,
    },
}

/// Parses the arguments after the hidden subcommand. Unknown or repeated
/// arguments are refused; the report pipe must be one this binary names.
fn parse_elevated(subcommand: &OsStr, rest: &[OsString]) -> Result<Elevated, String> {
    let mut user = None;
    let mut source = None;
    let mut report = None;
    let mut iter = rest.iter();
    while let Some(flag) = iter.next() {
        let flag = flag.to_str().ok_or("arguments must be Unicode")?;
        let value = iter
            .next()
            .ok_or_else(|| format!("{flag} needs a value"))?
            .clone();
        let slot = match flag {
            "--user-sid" => &mut user,
            "--source" => &mut source,
            "--report" => &mut report,
            other => return Err(format!("unexpected argument `{other}`")),
        };
        if slot.replace(value).is_some() {
            return Err(format!("{flag} given twice"));
        }
    }
    let report = report
        .map(|report| {
            report
                .into_string()
                .ok()
                .filter(|report| report.starts_with(REPORT_PREFIX))
                .ok_or("--report must name a devlanes install pipe")
        })
        .transpose()?;
    if subcommand == INSTALL_ARG {
        let user = user
            .and_then(|user| user.into_string().ok())
            .ok_or("--user-sid is required")?;
        let source = PathBuf::from(source.ok_or("--source is required")?);
        Ok(Elevated::Install {
            user,
            source,
            report,
        })
    } else if user.is_none() && source.is_none() {
        Ok(Elevated::Uninstall { report })
    } else {
        Err("uninstall takes only --report".into())
    }
}

/// Entry point of the elevated child (`devlanes.exe __install-hosts-service ...`
/// or `__uninstall-hosts-service ...`). Exits 0 on success, 1 on failure (the
/// report says why), 2 on bad arguments.
pub fn elevated_main(subcommand: &OsStr, rest: &[OsString]) -> ExitCode {
    let parsed = match parse_elevated(subcommand, rest) {
        Ok(parsed) => parsed,
        Err(message) => {
            eprintln!("devlanes {}: {message}", subcommand.to_string_lossy());
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("devlanes: {error}");
            return ExitCode::from(1);
        }
    };
    let (report, value) = runtime.block_on(async {
        match parsed {
            Elevated::Install {
                user,
                source,
                report,
            } => (report, elevated_install(user, source).await),
            Elevated::Uninstall { report } => (report, elevated_uninstall().await),
        }
    });
    let ok = value.get("ok").and_then(Value::as_bool) == Some(true);
    if let Some(report) = report {
        runtime.block_on(send_report(&report, &value));
    }
    runtime.shutdown_timeout(Duration::from_secs(5));
    ExitCode::from(if ok { 0 } else { 1 })
}

/// Installs for `user` from `source`, which must be the service binary next
/// to this one: the elevated child installs nothing a caller points it at.
async fn elevated_install(user: String, source: PathBuf) -> Value {
    let expected = match service_source(std::env::current_exe()) {
        Ok(expected) => expected,
        Err(error) => return failure(&error.code, error.data["message"].as_str().unwrap_or("")),
    };
    if !same_file_name(&source, &expected) {
        return failure(
            "service_source_rejected",
            &format!(
                "{} is not the service binary next to devlanes.exe ({})",
                source.display(),
                expected.display()
            ),
        );
    }
    let user = match Sid::parse(&user) {
        Ok(user) => user,
        Err(error) => return failure("usage", &error.to_string()),
    };
    // The pipe check works when the approving administrator is this same user
    // (the usual UAC consent); otherwise the service rightly refuses this
    // account, and the unelevated CLI checks the pipe afterwards.
    let same_user = token::current_user().is_ok_and(|me| me == user);
    let handle = tokio::runtime::Handle::current();
    let result = tokio::task::spawn_blocking(move || {
        let verify = || {
            if !same_user {
                return Ok(());
            }
            let client = service_client().map_err(|error| format!("{}", error.data["message"]))?;
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                match handle.block_on(client.version()) {
                    Ok(_) => return Ok(()),
                    // The pipe appears a moment after the service reports running.
                    Err(error) if error.is_missing() && std::time::Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(200));
                    }
                    Err(error) => return Err(error.to_string()),
                }
            }
        };
        install::install(&expected, &user, &verify)
    })
    .await;
    match result {
        Ok(Ok(installed)) => json!({
            "ok": true,
            "path": installed.path,
            "version": installed.version.version,
        }),
        Ok(Err(error)) => failure(error.code(), &error.to_string()),
        Err(error) => failure("hosts_install_step_failed", &error.to_string()),
    }
}

/// Removes the service and its files.
async fn elevated_uninstall() -> Value {
    match tokio::task::spawn_blocking(install::uninstall).await {
        Ok(Ok(warnings)) => json!({"ok": true, "warnings": warnings}),
        Ok(Err(error)) => failure(error.code(), &error.to_string()),
        Err(error) => failure("hosts_install_step_failed", &error.to_string()),
    }
}

/// A failure report.
fn failure(code: &str, message: &str) -> Value {
    json!({"ok": false, "code": code, "message": message})
}

/// Whether two paths name the same file, ignoring case and `\\?\`.
fn same_file_name(a: &Path, b: &Path) -> bool {
    let normal = |path: &Path| {
        let text = path.to_string_lossy().to_lowercase();
        text.strip_prefix(r"\\?\")
            .map(str::to_owned)
            .unwrap_or(text)
    };
    normal(a) == normal(b)
}

/// Writes `value` to the report pipe, checking that this user (unelevated)
/// serves it. The pipe grants only the client rights, so it is opened with those.
/// A report that cannot be sent leaves the exit code.
async fn send_report(name: &str, value: &Value) {
    let Ok(mut client) = devlanes_sys::windows::pipe::open_client(OsStr::new(name)) else {
        return;
    };
    if !devlanes_sys::windows::pipe::server_is_current_user(&client).unwrap_or(false) {
        return;
    }
    let _ = client.write_all(value.to_string().as_bytes()).await;
    let _ = client.flush().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_cli::clap::Parser as _;

    fn os(list: &[&str]) -> Vec<OsString> {
        list.iter().map(OsString::from).collect()
    }

    #[test]
    fn elevated_arguments_parse_strictly() {
        let report = format!("{REPORT_PREFIX}1-2");
        assert_eq!(
            parse_elevated(
                OsStr::new(INSTALL_ARG),
                &os(&[
                    "--user-sid",
                    "S-1-5-21-1",
                    "--source",
                    r"C:\a b\x.exe",
                    "--report",
                    &report
                ])
            ),
            Ok(Elevated::Install {
                user: "S-1-5-21-1".into(),
                source: PathBuf::from(r"C:\a b\x.exe"),
                report: Some(report.clone()),
            })
        );
        assert_eq!(
            parse_elevated(OsStr::new(UNINSTALL_ARG), &os(&["--report", &report])),
            Ok(Elevated::Uninstall {
                report: Some(report.clone())
            })
        );
        for (subcommand, args) in [
            (INSTALL_ARG, &["--source", "x"][..]),
            (INSTALL_ARG, &["--user-sid", "S-1-5-18"]),
            (
                INSTALL_ARG,
                &["--user-sid", "a", "--user-sid", "b", "--source", "x"],
            ),
            (
                INSTALL_ARG,
                &["--user-sid", "a", "--source", "x", "--hosts-file", "y"],
            ),
            (
                INSTALL_ARG,
                &[
                    "--user-sid",
                    "a",
                    "--source",
                    "x",
                    "--report",
                    r"\\.\pipe\other",
                ],
            ),
            (UNINSTALL_ARG, &["--source", "x"]),
            (UNINSTALL_ARG, &["--report"]),
        ] {
            assert!(
                parse_elevated(OsStr::new(subcommand), &os(args)).is_err(),
                "{subcommand} {args:?}"
            );
        }
    }

    #[test]
    fn command_lines_round_trip_through_windows_quoting() {
        assert_eq!(quote("plain"), "plain");
        assert_eq!(
            quote(r"C:\Program Files\x.exe"),
            r#""C:\Program Files\x.exe""#
        );
        assert_eq!(quote(r"C:\a b\"), r#""C:\a b\\""#);
        assert_eq!(quote(r#"a"b"#), r#""a\"b""#);
        assert_eq!(quote(""), r#""""#);
        assert_eq!(
            command_line(INSTALL_ARG, &os(&["--source", r"C:\a b\x.exe"])),
            r#"__install-hosts-service --source "C:\a b\x.exe""#
        );
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

    /// The report pipe carries the child's JSON to the parent, with the
    /// sender's PID; nobody can add a second instance of it.
    #[tokio::test]
    async fn reports_reach_the_parent() {
        let name = report_pipe_name();
        let server =
            devlanes_sys::windows::pipe::create_single_server(OsStr::new(&name)).expect("server");
        assert!(
            devlanes_sys::windows::pipe::create_server(OsStr::new(&name), false).is_err(),
            "a second instance"
        );
        let reader = tokio::spawn(read_report(server));
        let report = json!({"ok": false, "code": "hosts_unsafe_directory", "message": "m"});
        send_report(&name, &report).await;
        let read = tokio::time::timeout(Duration::from_secs(10), reader)
            .await
            .expect("in time")
            .expect("reader");
        assert_eq!(read, Some((std::process::id(), report)));
    }
}

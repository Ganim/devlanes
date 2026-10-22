//! Windows `devlanes certs trust --resolver` and `certs untrust --resolver`: an
//! NRPT rule that sends `hostnames.tld` lookups to the daemon's DNS server, and
//! its status for `doctor`.
//!
//! The scripts come from `devlanes_resolver_dns::nrpt`. Changes run in
//! `powershell.exe` started elevated through the UAC prompt (one prompt per
//! change, none when the rule is already in place); reading the rules needs no
//! admin. The elevated PowerShell reports through its exit code only, since its
//! hidden console cannot be read.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use devlanes_cli::{Rendered, Runtime};
use devlanes_client::Paths;
use devlanes_proto::RpcError;
use devlanes_resolver_dns::Zone;
use devlanes_resolver_dns::nrpt::{self, NrptRules, ScriptFailure};
use devlanes_sys::NoWindow;

use crate::hosts_service::require_prompt;
use serde_json::{Value, json};

use crate::output::emit;

/// Bound on reading the rules; PowerShell usually answers in about a second.
const STATUS_TIMEOUT: Duration = Duration::from_secs(20);

/// `powershell.exe` from the system directory, never `PATH`: the elevated run
/// must not pick up a program the user's `PATH` puts first.
fn powershell() -> Result<PathBuf, String> {
    devlanes_sys::windows::dirs::system_directory()
        .map(|system| system.join(r"WindowsPowerShell\v1.0\powershell.exe"))
        .map_err(|error| format!("finding the system directory: {error}"))
}

/// Reads the local NRPT rules and whether Group Policy overrides them, without
/// admin rights. PowerShell runs without a window, is killed when the timeout
/// or the caller drops it, and never blocks the executor.
pub(crate) async fn read_rules() -> Result<NrptRules, String> {
    let program = powershell()?;
    let mut command = tokio::process::Command::new(&program);
    command
        .no_window()
        .args(nrpt::POWERSHELL_ARGS)
        .arg(nrpt::status_script())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(STATUS_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "Get-DnsClientNrptRule did not answer within {} s",
                STATUS_TIMEOUT.as_secs()
            )
        })?
        .map_err(|error| format!("running {}: {error}", program.display()))?;
    if !output.status.success() {
        return Err(format!(
            "Get-DnsClientNrptRule failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    NrptRules::parse(&String::from_utf8_lossy(&output.stdout)).map_err(|error| error.to_string())
}

/// `doctor`'s view of the rule for `tld`:
/// `{namespace, server, installed, matches, foreign, group_policy}`, or
/// `{namespace?, error}`. `matches` also needs the server on port 53, the only
/// port an NRPT rule reaches. Runs PowerShell on the current runtime, so call it
/// from a blocking thread inside one.
pub(crate) fn status(tld: &str, server: SocketAddr) -> Value {
    let zone = match Zone::new(tld) {
        Ok(zone) => zone,
        Err(error) => return json!({"error": error.to_string()}),
    };
    let namespace = nrpt::namespace(&zone);
    let rules = match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.block_on(read_rules()),
        Err(error) => Err(format!("no runtime to run PowerShell on: {error}")),
    };
    match rules {
        Ok(rules) => {
            let found = rules.status(&zone, server.ip());
            json!({
                "namespace": namespace,
                "server": server.ip(),
                "installed": found.installed,
                "matches": found.matches && server.port() == 53,
                "foreign": found.foreign,
                "group_policy": rules.group_policy,
            })
        }
        Err(error) => json!({"namespace": namespace, "error": error}),
    }
}

/// Where `certs trust --resolver` records the namespace it routed, so `untrust`
/// can explain a foreign rule after `hostnames.tld` changes.
fn record_path(paths: &Paths) -> PathBuf {
    paths.home.join("resolver-rule")
}

/// The zone of a recorded namespace (`.test`); `None` for anything a devlanes
/// install would not have written, since the record is user-writable.
fn parse_record(text: &str) -> Option<Zone> {
    let name = text.trim().strip_prefix('.')?;
    Zone::new(name).ok().filter(|zone| zone.name() == name)
}

/// The error for another program's rule on `namespace`.
fn foreign(namespace: &str) -> RpcError {
    RpcError::new(
        "resolver_rule_foreign",
        json!({
            "namespace": namespace,
            "message": format!(
                "an NRPT rule for {namespace} was not added by devlanes (another program or the administrator owns it; see Get-DnsClientNrptRule); remove it yourself or choose another hostnames.tld"
            ),
        }),
        1,
    )
}

/// A failed change, keeping the step in the message.
fn failed(message: String) -> RpcError {
    RpcError::new("resolver_install_failed", json!({"message": message}), 1)
}

/// Runs `script` in `powershell.exe` elevated through the UAC prompt and waits
/// for it. `namespace` names the rule in a foreign-rule error.
async fn run_elevated(script: String, namespace: &str) -> Result<(), RpcError> {
    let program = powershell().map_err(failed)?;
    let line = nrpt::powershell_command_line(&script)
        .ok_or_else(|| failed("the NRPT script cannot be passed to PowerShell".into()))?;
    let exit = tokio::task::spawn_blocking(move || {
        devlanes_sys::windows::shell::run_elevated(&program, &line)
    })
    .await
    .map_err(|error| failed(error.to_string()))?;
    match exit {
        Ok(0) => Ok(()),
        Ok(code) => Err(match ScriptFailure::from_exit_code(code) {
            Some(ScriptFailure::Foreign) => foreign(namespace),
            Some(failure) => failed(failure.message().to_owned()),
            None => failed(format!("the elevated PowerShell exited with {code}")),
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
        Err(error) => Err(failed(format!("starting the elevated PowerShell: {error}"))),
    }
}

/// Why Windows ignores the local rule, when Group Policy NRPT rules exist.
fn group_policy_warning(rules: &NrptRules, namespace: &str, server: SocketAddr) -> String {
    if !rules.group_policy {
        return String::new();
    }
    format!(
        "warning: this machine has Group Policy NRPT rules, so Windows ignores local rules like this one; ask the administrator to add {namespace} -> {} to the policy\n",
        server.ip()
    )
}

/// `certs trust --resolver` adds the NRPT rule for `hostnames.tld` (replacing
/// devlanes rules for an older TLD); `certs untrust --resolver` removes every
/// devlanes rule. Rules without the devlanes comment are never touched: one for
/// the configured namespace stops the install.
pub async fn apply(runtime: &Runtime, install: bool) -> Result<u8, RpcError> {
    let io_error = |message: String| RpcError::new("io", json!({"message": message}), 1);
    let paths = Paths::discover().map_err(|error| io_error(error.to_string()))?;
    let configured = crate::local::resolver_tld(&paths).map(|tld| Zone::new(&tld));
    let zone = match configured {
        Some(Ok(zone)) => Some(zone),
        Some(Err(error)) if install => {
            return Err(RpcError::new(
                "resolver_unsupported",
                json!({"message": error.to_string()}),
                2,
            ));
        }
        // Removal goes on without a usable TLD: it drops every devlanes rule.
        Some(Err(_)) | None => None,
    };
    // The daemon listens on the default address; there is no setting for it.
    let server = devlanes_resolver_dns::default_listen();
    let record = record_path(&paths);
    let recorded = tokio::fs::read_to_string(&record)
        .await
        .ok()
        .and_then(|text| parse_record(&text));
    let rules = read_rules().await.map_err(|message| {
        RpcError::new("resolver_status_failed", json!({"message": message}), 1)
    })?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    if install {
        let zone = zone.ok_or_else(crate::local::not_configured)?;
        let namespace = nrpt::namespace(&zone);
        let found = rules.status(&zone, server.ip());
        if found.foreign {
            return Err(foreign(&namespace));
        }
        stderr.push_str(&group_policy_warning(&rules, &namespace, server));
        if found.installed && found.matches && rules.managed().count() == 1 {
            stdout.push_str(&format!(
                "the NRPT rule for {namespace} is in place: *.{} resolves through the devlanes DNS server at {server}\n",
                zone.name()
            ));
        } else {
            require_prompt(runtime, "devlanes certs trust --resolver")?;
            emit(&Rendered {
                stderr: format!(
                    "adding an NRPT rule for {namespace}; Windows asks for administrator approval\n"
                ),
                ..Rendered::default()
            });
            run_elevated(nrpt::install_script(&zone, server.ip()), &namespace).await?;
            let after = read_rules().await.map_err(failed)?;
            let found = after.status(&zone, server.ip());
            if !(found.installed && found.matches) {
                return Err(failed(format!(
                    "PowerShell finished, but no devlanes NRPT rule sends {namespace} to {}",
                    server.ip()
                )));
            }
            stdout.push_str(&format!(
                "added an NRPT rule: *.{} resolves through the devlanes DNS server at {server}\n",
                zone.name()
            ));
        }
        if let Err(error) = tokio::fs::write(&record, format!("{namespace}\n")).await {
            stderr.push_str(&format!(
                "warning: could not record the installed NRPT rule: {error}\n"
            ));
        }
    } else {
        let namespaces: Vec<String> = rules
            .managed()
            .flat_map(|rule| rule.namespaces.iter().cloned())
            .collect();
        if namespaces.is_empty() {
            let zone = recorded.or(zone).ok_or_else(crate::local::not_configured)?;
            let namespace = nrpt::namespace(&zone);
            // Names under it still resolve elsewhere; say whose rule does that.
            if rules.status(&zone, server.ip()).foreign {
                return Err(foreign(&namespace));
            }
            stdout.push_str("no devlanes NRPT rule to remove\n");
        } else {
            require_prompt(runtime, "devlanes certs untrust --resolver")?;
            let list = namespaces.join(", ");
            emit(&Rendered {
                stderr: format!(
                    "removing the devlanes NRPT rule for {list}; Windows asks for administrator approval\n"
                ),
                ..Rendered::default()
            });
            run_elevated(nrpt::remove_script(), &list).await?;
            let left = read_rules().await.map_err(failed)?.managed().count();
            if left > 0 {
                return Err(failed(format!(
                    "PowerShell finished, but {left} devlanes NRPT rules are left"
                )));
            }
            stdout.push_str(&format!("removed the devlanes NRPT rule for {list}\n"));
        }
        match tokio::fs::remove_file(&record).await {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => stderr.push_str(
                &format!("warning: could not remove the NRPT rule record: {error}\n"),
            ),
            _ => {}
        }
    }
    if runtime.quiet {
        stdout.clear();
    }
    emit(&Rendered {
        stdout,
        stderr,
        ..Rendered::default()
    });
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_cli::clap::Parser as _;

    #[test]
    fn records_hold_a_devlanes_namespace() {
        assert_eq!(
            parse_record(".test\n").map(|zone| zone.name().to_owned()),
            Some("test".into())
        );
        assert_eq!(
            parse_record(".dev.internal").map(|zone| zone.name().to_owned()),
            Some("dev.internal".into())
        );
        for refused in ["test", ".com", ".TEST", "", ".", r"..\x", ".test extra"] {
            assert!(parse_record(refused).is_none(), "{refused:?}");
        }
    }

    /// Agents, CI and anything without a terminal get `terminal_required`
    /// before any prompt.
    #[test]
    fn sessions_that_cannot_prompt_need_a_terminal() {
        let cli = devlanes_cli::Cli::parse_from(["devlanes", "--output", "plain"]);
        let runtime = Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime");
        assert!(!runtime.can_prompt());
        let error = require_prompt(&runtime, "devlanes certs trust --resolver")
            .expect_err("terminal required");
        assert_eq!(error.code, "terminal_required");
        assert_eq!(error.exit_code, 2);
        assert_eq!(error.data["command"], "devlanes certs trust --resolver");
    }

    /// Reading the rules works without admin rights and parses whatever this
    /// machine has (often no rules at all).
    #[tokio::test]
    async fn rules_are_readable_without_admin() {
        let rules = read_rules().await.expect("Get-DnsClientNrptRule");
        for rule in &rules.rules {
            assert!(!rule.name.is_empty(), "{rule:?}");
        }
    }

    /// `doctor` reports the namespace and state, or a TLD error.
    #[tokio::test(flavor = "multi_thread")]
    async fn doctor_status_names_the_namespace() {
        let server = devlanes_resolver_dns::default_listen();
        let report = tokio::task::spawn_blocking(move || status("test", server))
            .await
            .expect("status");
        assert_eq!(report["namespace"], ".test", "{report}");
        assert!(report["installed"].is_boolean(), "{report}");
        assert!(report["foreign"].is_boolean(), "{report}");
        assert!(report["group_policy"].is_boolean(), "{report}");
        let other = tokio::task::spawn_blocking(move || status("example", server))
            .await
            .expect("status");
        assert!(other["error"].is_string(), "{other}");
        // NRPT rules cannot name a port, so another port never matches.
        let elsewhere: SocketAddr = "127.53.0.1:5353".parse().expect("addr");
        let report = tokio::task::spawn_blocking(move || status("test", elsewhere))
            .await
            .expect("status");
        assert_eq!(report["matches"], false, "{report}");
    }

    /// Runs `script` in Windows PowerShell, unelevated, with the test timeout.
    async fn run_powershell(script: String) -> std::process::Output {
        tokio::time::timeout(
            STATUS_TIMEOUT,
            tokio::process::Command::new(powershell().expect("powershell"))
                .no_window()
                .args(nrpt::POWERSHELL_ARGS)
                .arg(script)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("in time")
        .expect("powershell")
    }

    /// The generated scripts parse in Windows PowerShell 5.1 (parsed only,
    /// never run).
    #[tokio::test]
    async fn scripts_parse_in_windows_powershell() {
        let zone = Zone::new("dev.internal").expect("zone");
        for script in [
            nrpt::install_script(&zone, devlanes_resolver_dns::default_listen().ip()),
            nrpt::remove_script(),
            nrpt::status_script(),
        ] {
            let check = format!(
                "$errors = $null; [void][System.Management.Automation.Language.Parser]::ParseInput({}, [ref]$null, [ref]$errors); $errors.Count",
                nrpt::powershell_literal(&script)
            );
            let output = run_powershell(check).await;
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                "0",
                "{script}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    /// Cmdlet names in the scripts and the stand-ins the logic tests call, so
    /// those tests never reach the real NRPT or DNS cache.
    const STAND_INS: [(&str, &str); 4] = [
        ("Get-DnsClientNrptRule", "Get-StandInRule"),
        ("Remove-DnsClientNrptRule", "Remove-StandInRule"),
        ("Add-DnsClientNrptRule", "Add-StandInRule"),
        ("Clear-DnsClientCache", "Clear-StandInCache"),
    ];

    /// Runs `script` with every NRPT cmdlet replaced by a stand-in that prints
    /// what it was asked to do. `rules` is a PowerShell expression for the
    /// existing rules; `add_fails` makes the add step throw. Returns the printed
    /// steps and the exit code.
    async fn run_with_stand_ins(script: &str, rules: &str, add_fails: bool) -> (String, i32) {
        let mut script = script.to_owned();
        for (cmdlet, stand_in) in STAND_INS {
            script = script.replace(cmdlet, stand_in);
            assert!(!script.contains(cmdlet), "{cmdlet} left in {script}");
        }
        let add = if add_fails {
            "throw 'access denied'"
        } else {
            "[Console]::Out.WriteLine(\"add $Namespace $NameServers $Comment\")"
        };
        let stand_ins = format!(
            "function Get-StandInRule {{ {rules} }}; \
             function Remove-StandInRule {{ param([string]$Name, [switch]$Force) [Console]::Out.WriteLine(\"remove $Name\") }}; \
             function Add-StandInRule {{ param($Namespace, $NameServers, $Comment) {add} }}; \
             function Clear-StandInCache {{ [Console]::Out.WriteLine('flush') }}; "
        );
        let output = run_powershell(stand_ins + &script).await;
        (
            String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n"),
            output.status.code().unwrap_or(-1),
        )
    }

    /// The install script replaces devlanes rules, keeps the others, and stops
    /// before any change when another rule routes the namespace.
    #[tokio::test]
    async fn install_script_logic() {
        let zone = Zone::new("test").expect("zone");
        let script = nrpt::install_script(&zone, devlanes_resolver_dns::default_listen().ip());
        let ours = "[pscustomobject]@{ Name = '{A}'; Namespace = @('.old'); Comment = '# Managed by devlanes: x' }";
        let theirs = "[pscustomobject]@{ Name = '{B}'; Namespace = @('.corp'); Comment = $null }";
        let (steps, code) = run_with_stand_ins(&script, &format!("{ours}; {theirs}"), false).await;
        assert_eq!(code, 0, "{steps}");
        assert_eq!(
            steps,
            "remove {A}\n\
             add .test 127.53.0.1 # Managed by devlanes: sends *.test lookups to the devlanes DNS server\n\
             flush\n"
        );

        let foreign =
            "[pscustomobject]@{ Name = '{C}'; Namespace = @('.TEST'); Comment = 'Acme dev DNS' }";
        let (steps, code) = run_with_stand_ins(&script, &format!("{ours}; {foreign}"), false).await;
        assert_eq!(
            u32::try_from(code)
                .ok()
                .and_then(ScriptFailure::from_exit_code),
            Some(ScriptFailure::Foreign),
            "{steps}"
        );
        assert_eq!(steps, "");

        let (steps, code) = run_with_stand_ins(&script, ours, true).await;
        assert_eq!(
            u32::try_from(code)
                .ok()
                .and_then(ScriptFailure::from_exit_code),
            Some(ScriptFailure::Add),
            "{steps}"
        );
        assert_eq!(steps, "remove {A}\n");

        // No rules at all: the cmdlet prints nothing.
        let (steps, code) = run_with_stand_ins(&script, "", false).await;
        assert_eq!(code, 0, "{steps}");
        assert!(steps.starts_with("add .test "), "{steps}");
    }

    /// The removal script drops devlanes rules only.
    #[tokio::test]
    async fn remove_script_logic() {
        let rules = "[pscustomobject]@{ Name = '{A}'; Namespace = @('.test'); Comment = '# Managed by devlanes: x' }; \
                     [pscustomobject]@{ Name = '{B}'; Namespace = @('.test'); Comment = 'not # Managed by devlanes' }; \
                     [pscustomobject]@{ Name = '{C}'; Namespace = @('.corp') }";
        let (steps, code) = run_with_stand_ins(&nrpt::remove_script(), rules, false).await;
        assert_eq!(code, 0, "{steps}");
        assert_eq!(steps, "remove {A}\nflush\n");
    }

    /// Adds the rule for `test`, resolves `web.acme-2.test` through the system
    /// resolver against the real DNS server, and removes every devlanes rule.
    /// Needs `127.53.0.1:53` free (no devlanes daemon with the resolver strategy).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "needs admin: adds and removes a real NRPT rule through the UAC prompt"]
    async fn nrpt_rule_sends_the_tld_to_the_server() {
        use std::net::ToSocketAddrs as _;

        let zone = Zone::new("test").expect("zone");
        let listen = devlanes_resolver_dns::default_listen();
        let server = devlanes_resolver_dns::DnsServer::bind(listen, zone.clone())
            .await
            .expect("bind 127.53.0.1:53");
        let token = tokio_util::sync::CancellationToken::new();
        let _stop = token.clone().drop_guard();
        tokio::spawn(server.serve(token));
        let before = read_rules().await.expect("rules");
        assert!(
            !before.status(&zone, listen.ip()).foreign,
            "another program's .test rule is in place"
        );
        run_elevated(nrpt::install_script(&zone, listen.ip()), ".test")
            .await
            .expect("install");
        let resolved = tokio::task::spawn_blocking(|| {
            let deadline = std::time::Instant::now() + Duration::from_secs(15);
            loop {
                match ("web.acme-2.test", 80).to_socket_addrs() {
                    Ok(addresses) => return Ok(addresses.collect::<Vec<_>>()),
                    Err(error) if std::time::Instant::now() > deadline => return Err(error),
                    Err(_) => std::thread::sleep(Duration::from_millis(250)),
                }
            }
        })
        .await
        .expect("lookup thread");
        // Removed before asserting, so a failed lookup leaves no rule behind.
        let removed = run_elevated(nrpt::remove_script(), ".test").await;
        let after = read_rules().await.expect("rules");
        let addresses = resolved.expect("web.acme-2.test resolves");
        assert!(
            addresses
                .iter()
                .any(|address| address.ip() == std::net::Ipv4Addr::LOCALHOST),
            "{addresses:?}"
        );
        removed.expect("remove");
        assert_eq!(after.managed().count(), 0);
    }
}

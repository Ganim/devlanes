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
use serde_json::{Value, json};

use crate::hosts_service::require_prompt;
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

/// `doctor`'s view of the rule for `tld`: `{namespace, server, installed,
/// matches, foreign, group_policy}`, or `{namespace?, error}`. `matches` also
/// needs the server on port 53, the only port an NRPT rule reaches. Runs
/// PowerShell on the current runtime, so call it from a blocking thread inside
/// one.
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
/// for it. `namespace` names the rule in a foreign-rule error. Returns a
/// warning for stderr when the change went through but something after it
/// failed.
async fn run_elevated(script: String, namespace: &str) -> Result<Option<String>, RpcError> {
    let program = powershell().map_err(failed)?;
    let line = nrpt::powershell_command_line(&script)
        .ok_or_else(|| failed("the NRPT script cannot be passed to PowerShell".into()))?;
    let exit = tokio::task::spawn_blocking(move || {
        devlanes_sys::windows::shell::run_elevated(&program, &line)
    })
    .await
    .map_err(|error| failed(error.to_string()))?;
    match exit {
        Ok(code) => script_outcome(code, namespace),
        Err(error)
            if error.raw_os_error() == Some(devlanes_sys::windows::shell::ERROR_CANCELLED) =>
        {
            Err(RpcError::new(
                "elevation_cancelled",
                json!({"message": "the administrator prompt was declined; nothing was changed"}),
                1,
            ))
        }
        Err(error) => Err(failed(format!("running the elevated PowerShell: {error}"))),
    }
}

/// What an elevated script's exit code means. A failed cache flush is the
/// last step, so the rules already changed: the caller goes on to verify and
/// record them, and the user gets a warning instead of an error.
fn script_outcome(code: u32, namespace: &str) -> Result<Option<String>, RpcError> {
    if code == 0 {
        return Ok(None);
    }
    match ScriptFailure::from_exit_code(code) {
        Some(ScriptFailure::Flush) => Ok(Some(format!(
            "warning: {}; run Clear-DnsClientCache as administrator or wait for the cache TTL\n",
            ScriptFailure::Flush.message()
        ))),
        Some(ScriptFailure::Foreign) => Err(foreign(namespace)),
        Some(failure) => Err(failed(failure.message().to_owned())),
        None => Err(failed(format!(
            "the elevated PowerShell exited with {code}"
        ))),
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

/// The result line for a rule for `zone` that is in place (`added` when this
/// run added it). With Group Policy NRPT rules Windows ignores it, so the line
/// does not claim that names resolve through devlanes.
fn routed_line(zone: &Zone, server: SocketAddr, added: bool, group_policy: bool) -> String {
    let namespace = nrpt::namespace(zone);
    match (added, group_policy) {
        (true, false) => format!(
            "added an NRPT rule: *.{} resolves through the devlanes DNS server at {server}\n",
            zone.name()
        ),
        (false, false) => format!(
            "the NRPT rule for {namespace} is in place: *.{} resolves through the devlanes DNS server at {server}\n",
            zone.name()
        ),
        (true, true) => format!(
            "added an NRPT rule for {namespace}, which Windows ignores while Group Policy NRPT rules exist\n"
        ),
        (false, true) => format!(
            "the NRPT rule for {namespace} is in place, but Windows ignores it while Group Policy NRPT rules exist\n"
        ),
    }
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
            stdout.push_str(&routed_line(&zone, server, false, rules.group_policy));
        } else {
            require_prompt(runtime, "devlanes certs trust --resolver")?;
            emit(&Rendered {
                stderr: format!(
                    "adding an NRPT rule for {namespace}; Windows asks for administrator approval\n"
                ),
                ..Rendered::default()
            });
            let script = nrpt::install_script(&zone, server.ip());
            if let Some(warning) = run_elevated(script, &namespace).await? {
                stderr.push_str(&warning);
            }
            let after = read_rules().await.map_err(failed)?;
            let found = after.status(&zone, server.ip());
            if !(found.installed && found.matches) {
                return Err(failed(format!(
                    "PowerShell finished, but no devlanes NRPT rule sends {namespace} to {}",
                    server.ip()
                )));
            }
            stdout.push_str(&routed_line(&zone, server, true, after.group_policy));
        }
    } else {
        let namespaces: Vec<String> = rules
            .managed()
            .flat_map(|rule| rule.namespaces.iter().cloned())
            .collect();
        if namespaces.is_empty() {
            let zone = zone.ok_or_else(crate::local::not_configured)?;
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
            if let Some(warning) = run_elevated(nrpt::remove_script(), &list).await? {
                stderr.push_str(&warning);
            }
            let left = read_rules().await.map_err(failed)?.managed().count();
            if left > 0 {
                return Err(failed(format!(
                    "PowerShell finished, but {left} devlanes NRPT rules are left"
                )));
            }
            stdout.push_str(&format!("removed the devlanes NRPT rule for {list}\n"));
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

    /// Runs `script` in Windows PowerShell, unelevated, with the test timeout,
    /// on the command line [`nrpt::powershell_command_line`] builds for the UAC
    /// prompt, appended as is the way `ShellExecuteEx` passes it.
    /// `module_path` replaces `PSModulePath` for the run.
    async fn run_command_line(script: &str, module_path: Option<&str>) -> std::process::Output {
        use std::os::windows::process::CommandExt as _;

        let line = nrpt::powershell_command_line(script).expect("no double quotes");
        let mut command = std::process::Command::new(powershell().expect("powershell"));
        command.raw_arg(line);
        if let Some(path) = module_path {
            command.env("PSModulePath", path);
        }
        let mut command = tokio::process::Command::from(command);
        command
            .no_window()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        tokio::time::timeout(STATUS_TIMEOUT, command.output())
            .await
            .expect("in time")
            .expect("powershell")
    }

    /// Standard output with LF line ends.
    fn stdout(output: &std::process::Output) -> String {
        String::from_utf8_lossy(&output.stdout).replace("\r\n", "\n")
    }

    /// A module planted first in `PSModulePath`, where Windows PowerShell 5.1
    /// puts the user's Documents folder, is not used once the elevated
    /// scripts' pin ran: neither one named DnsClient nor another exporting a
    /// DnsClient cmdlet.
    #[tokio::test]
    async fn the_module_pin_ignores_planted_modules() {
        let planted = tempfile::tempdir().expect("tempdir");
        for (module, cmdlet) in [
            ("DnsClient", "Get-DnsClientNrptRule"),
            ("Acme", "Clear-DnsClientCache"),
        ] {
            let directory = planted.path().join(module);
            std::fs::create_dir(&directory).expect("module directory");
            std::fs::write(
                directory.join(format!("{module}.psm1")),
                format!(
                    "function {cmdlet} {{ 'planted' }}\nExport-ModuleMember -Function {cmdlet}\n"
                ),
            )
            .expect("module");
        }
        let module_path = format!(
            "{};{}",
            planted.path().display(),
            std::env::var("PSModulePath").unwrap_or_default()
        );
        let probe = r"foreach ($cmdlet in 'Get-DnsClientNrptRule', 'Clear-DnsClientCache') { [Console]::Out.WriteLine((Get-Command $cmdlet).Module.ModuleBase -eq ($PSHOME + '\Modules\DnsClient')) }";
        // Without the pin the planted modules win, so they sit where 5.1 looks.
        let output = run_command_line(probe, Some(&module_path)).await;
        assert_eq!(stdout(&output), "False\nFalse\n", "{output:?}");
        // The prefix the elevated scripts start with.
        let zone = Zone::new("test").expect("zone");
        let install = nrpt::install_script(&zone, devlanes_resolver_dns::default_listen().ip());
        assert!(install.starts_with(&format!("{}; ", nrpt::MODULE_PIN)));
        let output = run_command_line(
            &format!("{}; {probe}", nrpt::MODULE_PIN),
            Some(&module_path),
        )
        .await;
        assert_eq!(stdout(&output), "True\nTrue\n", "{output:?}");
    }

    /// Group Policy counts only with rules under its key: no key and an empty
    /// one leave local rules in effect. A directory stands in for the key, which
    /// `Get-ChildItem` reads the same way.
    #[tokio::test]
    async fn group_policy_needs_rules_under_the_key() {
        let root = tempfile::tempdir().expect("tempdir");
        let empty = root.path().join("empty");
        std::fs::create_dir(&empty).expect("empty");
        let policy = root.path().join("policy");
        std::fs::create_dir_all(policy.join("{3E5A1F3C-6B2D-4C1E-9F0A-2B7D8C4E5F61}"))
            .expect("rule");
        let key = nrpt::powershell_literal(nrpt::GROUP_POLICY_KEY);
        for (path, expected) in [
            (root.path().join("missing"), false),
            (empty, false),
            (policy, true),
        ] {
            let script = nrpt::status_script()
                .replace(&key, &nrpt::powershell_literal(&path.display().to_string()));
            assert_ne!(script, nrpt::status_script());
            let output = run_command_line(&script, None).await;
            let rules = NrptRules::parse(&stdout(&output)).expect("status");
            assert_eq!(
                rules.group_policy,
                expected,
                "{} {output:?}",
                path.display()
            );
        }
    }

    /// The status read works in Constrained Language mode, which refuses its
    /// UTF-8 output call. The modules load first: a session switched by hand,
    /// unlike one under an application control policy, refuses to import
    /// Microsoft-signed modules into it.
    #[tokio::test]
    async fn status_reads_in_constrained_language_mode() {
        let script = format!(
            "Import-Module DnsClient, Microsoft.PowerShell.Utility, Microsoft.PowerShell.Management; \
             $ExecutionContext.SessionState.LanguageMode = 'ConstrainedLanguage'; {}",
            nrpt::status_script()
        );
        let output = run_command_line(&script, None).await;
        assert!(output.status.success(), "{output:?}");
        NrptRules::parse(&stdout(&output)).expect("status");
    }

    /// Cmdlet calls in the scripts and the stand-ins the logic tests call, so
    /// those tests never reach the real NRPT or DNS cache.
    const STAND_INS: [(&str, &str); 4] = [
        (r"DnsClient\Get-DnsClientNrptRule", "Get-StandInRule"),
        (r"DnsClient\Remove-DnsClientNrptRule", "Remove-StandInRule"),
        (r"DnsClient\Add-DnsClientNrptRule", "Add-StandInRule"),
        (r"DnsClient\Clear-DnsClientCache", "Clear-StandInCache"),
    ];

    /// A step of the scripts the stand-ins make throw.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Fail {
        Nothing,
        Add,
        Flush,
    }

    /// Runs `script` with every NRPT cmdlet replaced by a stand-in that prints
    /// what it was asked to do, through the UAC prompt's command line (the pin
    /// included, which only imports DnsClient). `rules` is a PowerShell
    /// expression for the existing rules; the added rule is named `{N}`.
    /// Returns the printed steps and the script's failure, if any.
    async fn run_with_stand_ins(
        script: &str,
        rules: &str,
        fail: Fail,
    ) -> (String, Option<ScriptFailure>) {
        let mut script = script.to_owned();
        for (cmdlet, stand_in) in STAND_INS {
            script = script.replace(cmdlet, stand_in);
            let name = cmdlet.trim_start_matches(r"DnsClient\");
            assert!(!script.contains(name), "{name} left in {script}");
        }
        let add = if fail == Fail::Add {
            "throw 'access denied'"
        } else {
            "[Console]::Out.WriteLine('add ' + $Namespace + ' ' + $NameServers + ' ' + $Comment); [pscustomobject]@{ Name = '{N}' }"
        };
        let flush = if fail == Fail::Flush {
            "throw 'cache busy'"
        } else {
            "[Console]::Out.WriteLine('flush')"
        };
        let stand_ins = format!(
            "function Get-StandInRule {{ {rules} }}; \
             function Remove-StandInRule {{ param([string]$Name, [switch]$Force) [Console]::Out.WriteLine('remove ' + $Name) }}; \
             function Add-StandInRule {{ param($Namespace, $NameServers, $Comment, [switch]$PassThru) {add} }}; \
             function Clear-StandInCache {{ {flush} }}; "
        );
        let output = run_command_line(&(stand_ins + &script), None).await;
        let code = output.status.code().expect("exit code");
        let failure = u32::try_from(code)
            .ok()
            .and_then(ScriptFailure::from_exit_code);
        assert!(code == 0 || failure.is_some(), "exit {code}: {output:?}");
        (stdout(&output), failure)
    }

    /// The install script adds the rule first, then replaces devlanes rules and
    /// keeps the others; it stops before any change when another rule routes
    /// the namespace, and a failed add keeps the old rule.
    #[tokio::test]
    async fn install_script_logic() {
        let zone = Zone::new("test").expect("zone");
        let script = nrpt::install_script(&zone, devlanes_resolver_dns::default_listen().ip());
        let ours = "[pscustomobject]@{ Name = '{A}'; Namespace = @('.old'); Comment = '# Managed by devlanes: x' }";
        let theirs = "[pscustomobject]@{ Name = '{B}'; Namespace = @('.corp'); Comment = $null }";
        let added = "add .test 127.53.0.1 # Managed by devlanes: sends *.test lookups to the devlanes DNS server\n";
        let (steps, failure) =
            run_with_stand_ins(&script, &format!("{ours}; {theirs}"), Fail::Nothing).await;
        assert_eq!(failure, None, "{steps}");
        assert_eq!(steps, format!("{added}remove {{A}}\nflush\n"));

        // Exact, by case, and with a trailing dot.
        for namespace in [".test", ".TEST", ".test."] {
            let foreign = format!(
                "[pscustomobject]@{{ Name = '{{C}}'; Namespace = @('{namespace}'); Comment = 'Acme dev DNS' }}"
            );
            let (steps, failure) =
                run_with_stand_ins(&script, &format!("{ours}; {foreign}"), Fail::Nothing).await;
            assert_eq!(
                failure,
                Some(ScriptFailure::Foreign),
                "{namespace}: {steps}"
            );
            assert_eq!(steps, "");
        }
        // A rule for names under the namespace is not a conflict.
        let under =
            "[pscustomobject]@{ Name = '{C}'; Namespace = @('.api.test'); Comment = 'Acme VPN' }";
        let (steps, failure) = run_with_stand_ins(&script, under, Fail::Nothing).await;
        assert_eq!(failure, None, "{steps}");

        let (steps, failure) = run_with_stand_ins(&script, ours, Fail::Add).await;
        assert_eq!(failure, Some(ScriptFailure::Add), "{steps}");
        assert_eq!(steps, "");

        // The rules changed before the flush failed.
        let (steps, failure) = run_with_stand_ins(&script, ours, Fail::Flush).await;
        assert_eq!(failure, Some(ScriptFailure::Flush), "{steps}");
        assert_eq!(steps, format!("{added}remove {{A}}\n"));

        // The added rule is never removed, even if listed before the add.
        let listed = "[pscustomobject]@{ Name = '{N}'; Namespace = @('.test'); Comment = '# Managed by devlanes: x' }";
        let (steps, failure) =
            run_with_stand_ins(&script, &format!("{ours}; {listed}"), Fail::Nothing).await;
        assert_eq!(failure, None, "{steps}");
        assert_eq!(steps, format!("{added}remove {{A}}\nflush\n"));

        // No rules at all: the cmdlet prints nothing.
        let (steps, failure) = run_with_stand_ins(&script, "", Fail::Nothing).await;
        assert_eq!(failure, None, "{steps}");
        assert_eq!(steps, format!("{added}flush\n"));
    }

    /// The removal script drops devlanes rules only.
    #[tokio::test]
    async fn remove_script_logic() {
        let rules = "[pscustomobject]@{ Name = '{A}'; Namespace = @('.test'); Comment = '# Managed by devlanes: x' }; \
                     [pscustomobject]@{ Name = '{B}'; Namespace = @('.test'); Comment = 'not # Managed by devlanes' }; \
                     [pscustomobject]@{ Name = '{C}'; Namespace = @('.corp') }";
        let (steps, failure) =
            run_with_stand_ins(&nrpt::remove_script(), rules, Fail::Nothing).await;
        assert_eq!(failure, None, "{steps}");
        assert_eq!(steps, "remove {A}\nflush\n");
        let (steps, failure) = run_with_stand_ins(&nrpt::remove_script(), rules, Fail::Flush).await;
        assert_eq!(failure, Some(ScriptFailure::Flush), "{steps}");
        assert_eq!(steps, "remove {A}\n");
    }

    /// A failed cache flush comes after the rules changed: a warning, so the
    /// CLI still verifies and records the change. Other failures stay errors.
    #[test]
    fn only_a_failed_flush_is_a_warning() {
        assert!(matches!(script_outcome(0, ".test"), Ok(None)));
        let warning = script_outcome(ScriptFailure::Flush.exit_code(), ".test")
            .expect("warning")
            .expect("text");
        assert!(warning.starts_with("warning: "), "{warning}");
        assert!(warning.contains("Clear-DnsClientCache"), "{warning}");
        let foreign =
            script_outcome(ScriptFailure::Foreign.exit_code(), ".test").expect_err("foreign");
        assert_eq!(foreign.code, "resolver_rule_foreign");
        for code in [
            ScriptFailure::Read.exit_code(),
            ScriptFailure::Remove.exit_code(),
            ScriptFailure::Add.exit_code(),
            1,
        ] {
            let error = script_outcome(code, ".test").expect_err("failure");
            assert_eq!(error.code, "resolver_install_failed", "{code}");
        }
    }

    /// Result lines claim resolution through devlanes only without Group
    /// Policy NRPT rules, which make Windows ignore the local rule.
    #[test]
    fn group_policy_lines_do_not_claim_resolution() {
        let zone = Zone::new("test").expect("zone");
        let server = devlanes_resolver_dns::default_listen();
        for added in [true, false] {
            assert!(routed_line(&zone, server, added, false).contains("resolves through"));
            let line = routed_line(&zone, server, added, true);
            assert!(!line.contains("resolves through"), "{line}");
            assert!(line.contains("Windows ignores"), "{line}");
        }
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

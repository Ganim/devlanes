//! Windows Name Resolution Policy Table (NRPT) rules that send the TLD to the
//! embedded server.
//!
//! Only the PowerShell scripts and the parsing of their output live here. The
//! CLI runs the install and removal scripts elevated, since NRPT rules are
//! machine-wide policy, and runs the status script unelevated for `doctor`.
//! NRPT rules name servers by address and never by port, which is why the
//! Windows server listens on port 53 of an address of its own.
//!
//! A rule is devlanes' when its comment starts with [`MANAGED_MARKER`], like the
//! generated files on macOS and Linux. Any other rule for the same namespace
//! belongs to another program or the admin and is never changed.

use std::net::IpAddr;

use serde_json::Value;

use crate::{DnsError, MANAGED_MARKER, Zone, is_managed};

/// Registry key, as a PowerShell path, that holds Group Policy NRPT rules.
/// While it exists the DNS Client ignores every local rule, devlanes' included.
pub const GROUP_POLICY_KEY: &str =
    r"HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\DNSClient\DnsPolicyConfig";

/// `powershell.exe` arguments before the script: no profile, which could prompt
/// or redefine cmdlets, and never a prompt.
pub const POWERSHELL_ARGS: [&str; 3] = ["-NoProfile", "-NonInteractive", "-Command"];

/// A step of the install or removal script that failed, read from its exit
/// code. Any other non-zero exit is a failure PowerShell itself reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptFailure {
    /// Reading the current rules failed.
    Read,
    /// Another program's rule already routes the namespace (install only).
    Foreign,
    /// Removing a devlanes rule failed.
    Remove,
    /// Adding the rule failed.
    Add,
    /// Clearing the DNS Client cache failed after the rules changed.
    Flush,
}

impl ScriptFailure {
    /// Every failure, for decoding exit codes.
    const ALL: [Self; 5] = [
        Self::Read,
        Self::Foreign,
        Self::Remove,
        Self::Add,
        Self::Flush,
    ];

    /// The script's exit code. Values from 10 keep clear of PowerShell's own 1.
    pub const fn exit_code(self) -> u32 {
        match self {
            Self::Read => 10,
            Self::Foreign => 11,
            Self::Remove => 12,
            Self::Add => 13,
            Self::Flush => 14,
        }
    }

    /// The failure a script exit code stands for, if it is one of ours.
    pub fn from_exit_code(code: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|failure| failure.exit_code() == code)
    }

    /// What went wrong, for the user.
    pub fn message(self) -> &'static str {
        match self {
            Self::Read => "PowerShell could not read the NRPT rules",
            Self::Foreign => "another program's NRPT rule already routes this namespace",
            Self::Remove => "PowerShell could not remove the devlanes NRPT rule",
            Self::Add => "PowerShell could not add the NRPT rule",
            Self::Flush => {
                "the NRPT rules changed, but PowerShell could not clear the DNS Client cache"
            }
        }
    }
}

/// The NRPT namespace for `zone`, `.test` for `test`: the leading dot makes the
/// rule match every name under the zone.
pub fn namespace(zone: &Zone) -> String {
    format!(".{}", zone.name())
}

/// The comment of a devlanes rule. It starts with [`MANAGED_MARKER`], so
/// [`is_managed`] recognises it.
pub fn rule_comment(zone: &Zone) -> String {
    format!(
        "{MANAGED_MARKER}: sends *.{} lookups to the devlanes DNS server",
        zone.name()
    )
}

/// `text` as a PowerShell single-quoted string, where nothing is expanded.
///
/// PowerShell ends such a string at an ASCII apostrophe or any typographic
/// single quote, and reads two of them in a row as the second one, so each is
/// doubled.
pub fn powershell_literal(text: &str) -> String {
    let mut literal = String::with_capacity(text.len() + 2);
    literal.push('\'');
    for character in text.chars() {
        if matches!(
            character,
            '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}'
        ) {
            literal.push(character);
        }
        literal.push(character);
    }
    literal.push('\'');
    literal
}

/// The first statements of the elevated scripts: Windows PowerShell 5.1 lists
/// the user's own `Documents\WindowsPowerShell\Modules` first in
/// `PSModulePath`, so autoloading the DnsClient cmdlets could import a module
/// the user (or anything running as the user) planted there, and run it as
/// administrator. Only modules under `$PSHOME` are searched afterwards, and
/// DnsClient is imported from there by path. Both statements work in
/// Constrained Language mode.
pub const MODULE_PIN: &str = r"$env:PSModulePath = $PSHOME + '\Modules'; Import-Module -Name ($PSHOME + '\Modules\DnsClient\DnsClient.psd1')";

/// PowerShell condition on `$_`: the rule's comment starts with the marker,
/// matching [`is_managed`] (a missing comment is an empty string).
fn managed_condition() -> String {
    format!(
        "([string]$_.Comment).StartsWith({}, [StringComparison]::Ordinal)",
        powershell_literal(MANAGED_MARKER)
    )
}

/// Reads the rules into `$rules`, or exits with [`ScriptFailure::Read`].
fn read_rules() -> String {
    format!(
        "try {{ $rules = @(DnsClient\\Get-DnsClientNrptRule) }} catch {{ exit {} }}",
        ScriptFailure::Read.exit_code()
    )
}

/// Removes the devlanes rules in `$rules` for which `keep` (a condition on `$_`)
/// is false, or exits with [`ScriptFailure::Remove`].
fn remove_managed(keep: &str) -> String {
    format!(
        "try {{ $rules | Where-Object {{ ({}) -and -not ({keep}) }} | ForEach-Object {{ DnsClient\\Remove-DnsClientNrptRule -Name $_.Name -Force }} }} catch {{ exit {} }}",
        managed_condition(),
        ScriptFailure::Remove.exit_code()
    )
}

/// Drops cached answers, including negative ones from before the change, or
/// exits with [`ScriptFailure::Flush`].
fn flush() -> String {
    format!(
        "try {{ DnsClient\\Clear-DnsClientCache }} catch {{ exit {} }}",
        ScriptFailure::Flush.exit_code()
    )
}

/// The elevated script for `certs trust --resolver`: refuses when another
/// program's rule routes the namespace, adds one rule sending the namespace to
/// `server`, then removes every other devlanes rule (an older TLD's included)
/// and clears the DNS Client cache. Exits 0 or with a [`ScriptFailure`] code.
///
/// Adding first keeps the old rule when the add fails. The CLI checks for a
/// foreign rule before asking for elevation too; checking again here covers a
/// rule added while the prompt was open. Namespaces compare without a trailing
/// dot and without case, as the DNS Client matches them.
pub fn install_script(zone: &Zone, server: IpAddr) -> String {
    [
        MODULE_PIN.to_owned(),
        "$ErrorActionPreference = 'Stop'".to_owned(),
        format!("$namespace = {}", powershell_literal(&namespace(zone))),
        read_rules(),
        format!(
            "if (@($rules | Where-Object {{ ((@($_.Namespace) -replace '\\.$', '') -contains $namespace) -and -not ({}) }}).Count -gt 0) {{ exit {} }}",
            managed_condition(),
            ScriptFailure::Foreign.exit_code()
        ),
        format!(
            "try {{ $added = DnsClient\\Add-DnsClientNrptRule -Namespace $namespace -NameServers {} -Comment {} -PassThru }} catch {{ exit {} }}",
            powershell_literal(&server.to_string()),
            powershell_literal(&rule_comment(zone)),
            ScriptFailure::Add.exit_code()
        ),
        remove_managed("$_.Name -eq $added.Name"),
        flush(),
        "exit 0".to_owned(),
    ]
    .join("; ")
}

/// The elevated script for `certs untrust --resolver`: removes every devlanes
/// rule, whatever its namespace, and clears the DNS Client cache. Other rules
/// stay. Exits 0 or with a [`ScriptFailure`] code.
pub fn remove_script() -> String {
    [
        MODULE_PIN.to_owned(),
        "$ErrorActionPreference = 'Stop'".to_owned(),
        read_rules(),
        remove_managed("$false"),
        flush(),
        "exit 0".to_owned(),
    ]
    .join("; ")
}

/// The unelevated script that prints the local rules, whether Group Policy
/// rules override them, and the effective policy as one JSON object for
/// [`NrptRules::parse`]: `{"rules": [...], "group_policy": false, "effective":
/// [...]}`. Reading needs no admin.
///
/// Output is UTF-8, since redirected output otherwise uses the OEM code page;
/// Constrained Language mode refuses that .NET call, and the script then goes
/// on with the code page. Group Policy counts only with rules under its key, as
/// Chromium decides (`net/dns/public/win_dns_system_settings.cc`): an empty key
/// left after a policy was removed does not override local rules. The effective
/// policy also holds rules from VPN profiles and is `null` when unreadable.
pub fn status_script() -> String {
    format!(
        "$ErrorActionPreference = 'Stop'; \
         try {{ [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false) }} catch {{ }}; \
         $rules = @(Get-DnsClientNrptRule | Select-Object Name, Namespace, NameServers, Comment); \
         $policy = @(Get-ChildItem -LiteralPath {} -ErrorAction SilentlyContinue).Count -gt 0; \
         try {{ $effective = @(Get-DnsClientNrptPolicy -Effective | Select-Object Namespace, NameServers) }} catch {{ $effective = $null }}; \
         ConvertTo-Json -Compress -Depth 4 -InputObject @{{ rules = $rules; group_policy = $policy; effective = $effective }}",
        powershell_literal(GROUP_POLICY_KEY)
    )
}

/// The `powershell.exe` command line that runs `script`, for launchers that take
/// one string, such as the UAC prompt's. `None` when the script holds a double
/// quote or ends with a backslash, which that line cannot carry unescaped;
/// scripts from this module never do.
pub fn powershell_command_line(script: &str) -> Option<String> {
    (!script.contains('"') && !script.ends_with('\\'))
        .then(|| format!("{} \"{script}\"", POWERSHELL_ARGS.join(" ")))
}

/// `namespace` without one trailing dot: the DNS Client treats `.test.` and
/// `.test` alike.
fn normalized(namespace: &str) -> &str {
    namespace
        .strip_suffix('.')
        .filter(|rest| !rest.is_empty())
        .unwrap_or(namespace)
}

/// Whether `routed` names something strictly under `namespace` (`.a.test` or
/// `host.test` under `.test`), without case.
fn is_under(routed: &str, namespace: &str) -> bool {
    let routed = normalized(routed);
    routed.len() > namespace.len()
        && routed
            .get(routed.len() - namespace.len()..)
            .is_some_and(|tail| tail.eq_ignore_ascii_case(namespace))
}

/// One NRPT rule as `Get-DnsClientNrptRule` reports it, or one entry of the
/// effective policy (`Get-DnsClientNrptPolicy -Effective`), which has no name
/// or comment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NrptRule {
    /// Identifier (a GUID in braces), which removal takes.
    pub name: String,
    /// Namespaces it routes, such as `.test`, without a trailing dot.
    pub namespaces: Vec<String>,
    /// DNS servers it sends them to.
    pub name_servers: Vec<String>,
    /// Free text; devlanes rules start with [`MANAGED_MARKER`].
    pub comment: String,
}

impl NrptRule {
    /// Whether devlanes added this rule.
    pub fn is_managed(&self) -> bool {
        is_managed(&self.comment)
    }

    /// Whether the rule routes `namespace` (NRPT compares names without case
    /// and without a trailing dot).
    pub fn routes(&self, namespace: &str) -> bool {
        self.namespaces
            .iter()
            .any(|routed| normalized(routed).eq_ignore_ascii_case(normalized(namespace)))
    }

    /// Builds a rule from one object of `ConvertTo-Json` output.
    fn from_json(value: &Value) -> Result<Self, DnsError> {
        let object = value
            .as_object()
            .ok_or_else(|| output_error(format!("expected a rule object, found {value}")))?;
        let text = |key: &str| {
            object
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        Ok(Self {
            name: text("Name"),
            namespaces: strings(object.get("Namespace"))
                .into_iter()
                .map(|namespace| normalized(&namespace).to_owned())
                .collect(),
            name_servers: strings(object.get("NameServers")),
            comment: text("Comment"),
        })
    }
}

/// A namespace some other NRPT rule routes, and the servers it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// The namespace, such as `.api.test` or `host.test`.
    pub namespace: String,
    /// Its DNS servers; empty for rules that set other options only.
    pub name_servers: Vec<String>,
}

/// How the local rules route one namespace.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RuleStatus {
    /// A devlanes rule routes it.
    pub installed: bool,
    /// A devlanes rule routes it to the expected server and nowhere else.
    pub matches: bool,
    /// Another program's rule routes it too.
    pub foreign: bool,
}

/// The local NRPT rules, and whether Group Policy rules override them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NrptRules {
    /// Every local rule, devlanes' or not.
    pub rules: Vec<NrptRule>,
    /// Group Policy NRPT rules exist (subkeys of [`GROUP_POLICY_KEY`]), so
    /// Windows ignores the local rules.
    pub group_policy: bool,
    /// The policy the DNS Client applies (local, Group Policy and VPN rules),
    /// without names or comments; `None` when it could not be read.
    pub effective: Option<Vec<NrptRule>>,
}

impl NrptRules {
    /// Parses PowerShell's JSON: the [`status_script`] object, or the plain
    /// output of `Get-DnsClientNrptRule | ConvertTo-Json`, which is empty
    /// without rules, one object for one rule and an array for more.
    ///
    /// Windows PowerShell 5.1 sometimes writes an array as
    /// `{"value": [...], "Count": n}`; both forms are read.
    pub fn parse(output: &str) -> Result<Self, DnsError> {
        let output = output.trim_start_matches('\u{feff}').trim();
        if output.is_empty() {
            return Ok(Self::default());
        }
        let value: Value = serde_json::from_str(output)
            .map_err(|error| output_error(format!("invalid JSON: {error}")))?;
        match value.as_object() {
            Some(object) if object.contains_key("rules") => Ok(Self {
                rules: rules(object.get("rules"))?,
                group_policy: object
                    .get("group_policy")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                effective: match object.get("effective") {
                    None | Some(Value::Null) => None,
                    effective => Some(rules(effective)?),
                },
            }),
            _ => Ok(Self {
                rules: rules(Some(&value))?,
                ..Self::default()
            }),
        }
    }

    /// Namespaces strictly under `zone`'s that another rule routes, local or
    /// effective (a VPN's included), once each. The DNS Client follows the most
    /// specific rule, so names under them never reach devlanes. devlanes rules
    /// route the zone's namespace only, so none of these is one.
    pub fn more_specific(&self, zone: &Zone) -> Vec<Route> {
        let namespace = namespace(zone);
        let local = self.rules.iter().filter(|rule| !rule.is_managed());
        let effective = self.effective.iter().flatten();
        let mut routes: Vec<Route> = Vec::new();
        for rule in local.chain(effective) {
            for routed in &rule.namespaces {
                let seen = routes
                    .iter()
                    .any(|route| route.namespace.eq_ignore_ascii_case(routed));
                if is_under(routed, &namespace) && !seen {
                    routes.push(Route {
                        namespace: routed.clone(),
                        name_servers: rule.name_servers.clone(),
                    });
                }
            }
        }
        routes
    }

    /// The servers the effective policy sends `zone`'s namespace to: empty when
    /// no applied rule routes it, `None` when the policy could not be read.
    pub fn effective_servers(&self, zone: &Zone) -> Option<Vec<String>> {
        let namespace = namespace(zone);
        self.effective.as_ref().map(|effective| {
            effective
                .iter()
                .filter(|rule| rule.routes(&namespace))
                .flat_map(|rule| rule.name_servers.iter().cloned())
                .collect()
        })
    }

    /// How the rules route `zone`'s namespace, `server` being the address
    /// devlanes rules must name.
    pub fn status(&self, zone: &Zone, server: IpAddr) -> RuleStatus {
        let namespace = namespace(zone);
        let mut status = RuleStatus::default();
        for rule in self.rules.iter().filter(|rule| rule.routes(&namespace)) {
            if !rule.is_managed() {
                status.foreign = true;
                continue;
            }
            status.installed = true;
            status.matches |= !rule.name_servers.is_empty()
                && rule
                    .name_servers
                    .iter()
                    .all(|named| named.trim().parse::<IpAddr>() == Ok(server));
        }
        status
    }

    /// The devlanes rules, any namespace.
    pub fn managed(&self) -> impl Iterator<Item = &NrptRule> {
        self.rules.iter().filter(|rule| rule.is_managed())
    }
}

/// The rules in `value`: none for `null`, one for an object, each element of an
/// array (or of a 5.1 `{"value": [...]}` wrapper).
fn rules(value: Option<&Value>) -> Result<Vec<NrptRule>, DnsError> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items.iter().map(NrptRule::from_json).collect(),
        Some(value) => match value.get("value") {
            Some(inner @ Value::Array(_)) => rules(Some(inner)),
            _ => Ok(vec![NrptRule::from_json(value)?]),
        },
    }
}

/// A string list property: `null`, one string, an array of strings, or a 5.1
/// `{"value": [...]}` wrapper. Empty strings are dropped.
fn strings(value: Option<&Value>) -> Vec<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => vec![text.clone()],
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(Value::as_str)
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
            .collect(),
        Some(Value::Object(object)) => strings(object.get("value")),
        _ => Vec::new(),
    }
}

/// An unreadable status output.
fn output_error(reason: String) -> DnsError {
    DnsError::NrptOutput { reason }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(name: &str) -> Zone {
        Zone::new(name).expect("zone")
    }

    fn server() -> IpAddr {
        crate::default_listen().ip()
    }

    /// `Get-DnsClientNrptRule | ConvertTo-Json` for one rule devlanes added,
    /// with the CIM properties PowerShell adds (ignored by the parser).
    const ONE_RULE: &str = r##"{
        "Comment": "# Managed by devlanes: sends *.test lookups to the devlanes DNS server",
        "DirectAccessDnsServers": null,
        "DirectAccessEnabled": false,
        "DisplayName": null,
        "DnsSecEnabled": false,
        "Name": "{3E5A1F3C-6B2D-4C1E-9F0A-2B7D8C4E5F61}",
        "NameEncoding": "Utf8WithoutMapping",
        "NameServers": ["127.53.0.1"],
        "Namespace": [".test"],
        "Version": 2,
        "PSComputerName": null,
        "CimClass": "root/Microsoft/Windows/DNS:DnsClientNrptRule",
        "CimInstanceProperties": ["Comment", "Name", "NameServers", "Namespace"],
        "CimSystemProperties": "Microsoft.Management.Infrastructure.CimSystemProperties"
    }"##;

    #[test]
    fn literals_cannot_end_early() {
        assert_eq!(powershell_literal(".test"), "'.test'");
        assert_eq!(powershell_literal("it's"), "'it''s'");
        assert_eq!(powershell_literal(""), "''");
        // PowerShell also ends single-quoted strings at typographic quotes.
        assert_eq!(
            powershell_literal("a\u{2019}b\u{2018}c"),
            "'a\u{2019}\u{2019}b\u{2018}\u{2018}c'"
        );
        // Nothing else is special inside single quotes.
        assert_eq!(powershell_literal("$x `n \"q\""), "'$x `n \"q\"'");
    }

    #[test]
    fn install_script_adds_one_marked_rule() {
        let script = install_script(&zone("dev.internal"), server());
        for part in [
            "$ErrorActionPreference = 'Stop'",
            "$namespace = '.dev.internal'",
            "DnsClient\\Get-DnsClientNrptRule",
            "((@($_.Namespace) -replace '\\.$', '') -contains $namespace) -and -not (([string]$_.Comment).StartsWith('# Managed by devlanes', [StringComparison]::Ordinal))",
            "{ exit 11 }",
            "$added = DnsClient\\Add-DnsClientNrptRule -Namespace $namespace -NameServers '127.53.0.1' -Comment '# Managed by devlanes: sends *.dev.internal lookups to the devlanes DNS server' -PassThru",
            "-not ($_.Name -eq $added.Name)",
            "DnsClient\\Remove-DnsClientNrptRule -Name $_.Name -Force",
            "DnsClient\\Clear-DnsClientCache",
        ] {
            assert!(script.contains(part), "missing {part:?} in {script}");
        }
        // Foreign rules are checked before anything changes, and the new rule
        // is in place before the old ones go.
        let foreign = script.find("exit 11").expect("foreign check");
        let add = script.find("Add-DnsClientNrptRule").expect("add");
        assert!(foreign < add);
        assert!(add < script.find("Remove-DnsClientNrptRule").expect("remove"));
        assert!(script.ends_with("exit 0"));
    }

    #[test]
    fn remove_script_only_touches_marked_rules() {
        let script = remove_script();
        assert!(script.contains(
            "$rules | Where-Object { (([string]$_.Comment).StartsWith('# Managed by devlanes', [StringComparison]::Ordinal)) -and -not ($false) } | ForEach-Object { DnsClient\\Remove-DnsClientNrptRule -Name $_.Name -Force }"
        ));
        assert!(script.contains("DnsClient\\Clear-DnsClientCache"));
        assert!(!script.contains("Add-DnsClientNrptRule"));
    }

    /// The elevated scripts pin the module path and import DnsClient from
    /// `$PSHOME` before anything else runs, and call its cmdlets by module.
    #[test]
    fn elevated_scripts_start_with_the_module_pin() {
        assert_eq!(
            MODULE_PIN,
            r"$env:PSModulePath = $PSHOME + '\Modules'; Import-Module -Name ($PSHOME + '\Modules\DnsClient\DnsClient.psd1')"
        );
        assert!(!MODULE_PIN.contains('"'));
        for script in [install_script(&zone("test"), server()), remove_script()] {
            assert!(script.starts_with(&format!("{MODULE_PIN}; ")), "{script}");
            for cmdlet in [
                "Get-DnsClientNrptRule",
                "Add-DnsClientNrptRule",
                "Remove-DnsClientNrptRule",
                "Clear-DnsClientCache",
            ] {
                let calls = script.matches(cmdlet).count();
                let qualified = script.matches(&format!("DnsClient\\{cmdlet}")).count();
                assert_eq!(calls, qualified, "{cmdlet} unqualified in {script}");
            }
        }
    }

    #[test]
    fn scripts_fit_one_command_line() {
        for script in [
            install_script(&zone("test"), server()),
            remove_script(),
            status_script(),
        ] {
            let line = powershell_command_line(&script).expect("no double quotes");
            assert_eq!(
                line,
                format!("-NoProfile -NonInteractive -Command \"{script}\"")
            );
            assert!(!script.contains(['\n', '\r']));
        }
        assert_eq!(powershell_command_line("Write-Output \"x\""), None);
        assert_eq!(powershell_command_line(r"Get-Item C:\"), None);
    }

    #[test]
    fn status_script_reads_rules_and_group_policy() {
        let script = status_script();
        assert!(script.contains(
            "Get-DnsClientNrptRule | Select-Object Name, Namespace, NameServers, Comment"
        ));
        // Rules under the key count, not the key itself.
        assert!(script.contains(
            r"$policy = @(Get-ChildItem -LiteralPath 'HKLM:\SOFTWARE\Policies\Microsoft\Windows NT\DNSClient\DnsPolicyConfig' -ErrorAction SilentlyContinue).Count -gt 0"
        ));
        assert!(!script.contains("Test-Path"));
        // Constrained Language mode refuses the encoding call; the read goes on.
        assert!(script.contains(
            "try { [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false) } catch { }"
        ));
        assert!(script.contains(
            "try { $effective = @(Get-DnsClientNrptPolicy -Effective | Select-Object Namespace, NameServers) } catch { $effective = $null }"
        ));
        assert!(script.contains("ConvertTo-Json -Compress"));
    }

    #[test]
    fn exit_codes_name_the_failed_step() {
        for failure in ScriptFailure::ALL {
            assert_eq!(
                ScriptFailure::from_exit_code(failure.exit_code()),
                Some(failure)
            );
        }
        assert_eq!(ScriptFailure::from_exit_code(0), None);
        assert_eq!(ScriptFailure::from_exit_code(1), None);
    }

    #[test]
    fn empty_output_means_no_rules() {
        // Captured on Windows 11 (26300) without rules: the status script prints
        // the first line, the plain cmdlet nothing at all.
        for output in [r#"{"group_policy":false,"rules":[]}"#, "", "\r\n", "null"] {
            let parsed = NrptRules::parse(output).expect(output);
            assert!(parsed.rules.is_empty(), "{output:?}");
            assert!(!parsed.group_policy);
        }
    }

    #[test]
    fn one_rule_is_an_object_and_more_are_an_array() {
        let one = NrptRules::parse(ONE_RULE).expect("one");
        assert_eq!(
            one.rules,
            [NrptRule {
                name: "{3E5A1F3C-6B2D-4C1E-9F0A-2B7D8C4E5F61}".into(),
                namespaces: vec![".test".into()],
                name_servers: vec!["127.53.0.1".into()],
                comment: "# Managed by devlanes: sends *.test lookups to the devlanes DNS server"
                    .into(),
            }]
        );
        let foreign = r#"{"Name": "{B}", "Namespace": ".corp.example.invalid", "NameServers": "10.0.0.53", "Comment": null}"#;
        let many = NrptRules::parse(&format!("[{ONE_RULE}, {foreign}]")).expect("many");
        assert_eq!(many.rules.len(), 2);
        assert_eq!(many.rules[1].namespaces, [".corp.example.invalid"]);
        assert_eq!(many.rules[1].name_servers, ["10.0.0.53"]);
        assert_eq!(many.rules[1].comment, "");
        assert_eq!(many.managed().count(), 1);

        // The status script's object, as captured with one crafted rule.
        let wrapped = NrptRules::parse(
            r##"{"group_policy":true,"rules":[{"Name":"{A}","Namespace":[".test"],"NameServers":["127.53.0.1"],"Comment":"# Managed by devlanes: x"}]}"##,
        )
        .expect("wrapped");
        assert!(wrapped.group_policy);
        assert_eq!(wrapped.rules[0].name, "{A}");
        // Windows PowerShell 5.1 array wrappers.
        let quirk = NrptRules::parse(
            r#"{"rules":{"value":[{"Name":"{A}","Namespace":{"value":[".test"],"Count":1}}],"Count":1},"group_policy":false}"#,
        )
        .expect("quirk");
        assert_eq!(quirk.rules[0].namespaces, [".test"]);
    }

    #[test]
    fn unreadable_output_is_an_error() {
        for output in ["not json", "[1, 2]", r#"{"rules": ["x"]}"#] {
            let error = NrptRules::parse(output).expect_err(output);
            assert!(matches!(error, DnsError::NrptOutput { .. }), "{error}");
        }
    }

    #[test]
    fn status_tells_ours_from_foreign_rules() {
        let rule = |namespace: &str, servers: &[&str], comment: &str| NrptRule {
            name: "{X}".into(),
            namespaces: vec![namespace.into()],
            name_servers: servers.iter().map(|s| (*s).to_owned()).collect(),
            comment: comment.into(),
        };
        let ours = "# Managed by devlanes: sends *.test lookups to the devlanes DNS server";
        let test = zone("test");
        let status = |rules: Vec<NrptRule>| {
            NrptRules {
                rules,
                ..NrptRules::default()
            }
            .status(&test, server())
        };
        assert_eq!(status(Vec::new()), RuleStatus::default());
        assert_eq!(
            status(vec![rule(".TEST", &["127.53.0.1"], ours)]),
            RuleStatus {
                installed: true,
                matches: true,
                foreign: false
            }
        );
        // A rule from an older install that named another server.
        assert_eq!(
            status(vec![rule(".test", &["127.0.0.1"], ours)]),
            RuleStatus {
                installed: true,
                matches: false,
                foreign: false
            }
        );
        assert!(!status(vec![rule(".test", &["127.53.0.1", "10.0.0.1"], ours)]).matches);
        // Another tool's rule for the same namespace, and an unrelated rule.
        assert_eq!(
            status(vec![
                rule(".test", &["127.0.0.1"], "Acme dev DNS"),
                rule(".internal", &["10.0.0.53"], ""),
            ]),
            RuleStatus {
                installed: false,
                matches: false,
                foreign: true
            }
        );
        // The marker must come first, as in resolver files.
        assert!(!rule(".test", &[], "copied from # Managed by devlanes").is_managed());
        // The DNS Client reads `.test.` as `.test`.
        assert!(status(vec![rule(".test.", &["127.0.0.1"], "Acme dev DNS")]).foreign);
        assert!(status(vec![rule(".test.", &["127.53.0.1"], ours)]).matches);
    }

    #[test]
    fn status_output_carries_group_policy_and_the_effective_policy() {
        // Captured on Windows 11 (26300) without rules or policy.
        let none =
            NrptRules::parse(r#"{"effective":[],"group_policy":false,"rules":[]}"#).expect("none");
        assert!(!none.group_policy);
        assert_eq!(none.effective, Some(Vec::new()));
        assert_eq!(none.effective_servers(&zone("test")), Some(Vec::new()));
        // Shaped like the capture, with the policy and VPN entries a managed
        // machine has; effective entries carry a string namespace and no name.
        let managed = NrptRules::parse(
            r##"{"effective":[{"Namespace":".test.","NameServers":["10.0.0.53"]},{"Namespace":".api.test","NameServers":"10.8.0.1"},{"Namespace":".corp","NameServers":null}],"group_policy":true,"rules":[{"Name":"{A}","Namespace":[".test"],"NameServers":["127.53.0.1"],"Comment":"# Managed by devlanes: x"},{"Name":"{B}","Namespace":["HOST.test."],"NameServers":["10.0.0.1"],"Comment":"Acme"}]}"##,
        )
        .expect("managed");
        assert!(managed.group_policy);
        assert_eq!(managed.rules[1].namespaces, ["HOST.test"]);
        let test = zone("test");
        assert_eq!(
            managed.effective_servers(&test),
            Some(vec!["10.0.0.53".to_owned()])
        );
        assert_eq!(
            managed.more_specific(&test),
            [
                Route {
                    namespace: "HOST.test".into(),
                    name_servers: vec!["10.0.0.1".into()],
                },
                Route {
                    namespace: ".api.test".into(),
                    name_servers: vec!["10.8.0.1".into()],
                },
            ]
        );
        // Older output without the field, and a failed effective read.
        for output in [
            r#"{"group_policy":false,"rules":[]}"#,
            r#"{"effective":null,"group_policy":false,"rules":[]}"#,
        ] {
            let parsed = NrptRules::parse(output).expect(output);
            assert_eq!(parsed.effective, None);
            assert_eq!(parsed.effective_servers(&test), None);
        }
    }

    #[test]
    fn more_specific_rules_are_strictly_under_the_zone() {
        assert!(is_under(".api.test", ".test"));
        assert!(is_under("host.TEST.", ".test"));
        assert!(!is_under(".test", ".test"));
        assert!(!is_under(".test.", ".test"));
        assert!(!is_under("contest", ".test"));
        assert!(!is_under(".internal", ".test"));
        // devlanes rules never count, wherever they point.
        let rules = NrptRules {
            rules: vec![NrptRule {
                namespaces: vec![".api.test".into()],
                comment: "# Managed by devlanes: x".into(),
                ..NrptRule::default()
            }],
            ..NrptRules::default()
        };
        assert!(rules.more_specific(&zone("test")).is_empty());
    }
}

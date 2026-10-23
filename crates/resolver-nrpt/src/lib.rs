//! The Windows side of the `resolver` hostname strategy: one Name Resolution
//! Policy Table (NRPT) rule that sends lookups under the devlanes TLD to the
//! daemon's DNS server on `127.53.0.1`, the counterpart of `/etc/resolver/<tld>`
//! on macOS.
//!
//! The rule is a registry key, `devlanes-<tld>`, under
//! `HKLM\SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig`.
//! The DNS client applies a change at once, without PowerShell, a cache flush
//! or a service restart. Reading needs no elevation, so `doctor` and the
//! daemon only read the registry. Writing needs an administrator: the CLI
//! generates a `.reg` file and runs `<System32>\reg.exe import` on it after
//! the UAC prompt, then reads the rule back.
//!
//! Windows already answers every `*.localhost` name itself, so only the
//! configured TLD needs a rule. Group Policy NRPT rules, when present, make
//! Windows ignore the local ones; [`Status::group_policy`] reports that.
#![cfg(windows)]
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{self, Read as _, Write as _};
use std::net::IpAddr;
use std::os::windows::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};

use devlanes_resolver_dns::Zone;
use devlanes_sys_windows::registry::{self, Hive, RegValue};
use devlanes_sys_windows::shell_execute::{self, ElevateError};

/// Where the DNS client reads local NRPT rules, under `HKEY_LOCAL_MACHINE`.
pub const RULES_KEY: &str =
    r"SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig";

/// Where Group Policy puts NRPT rules, under `HKEY_LOCAL_MACHINE`. Any rule
/// there makes the DNS client ignore every local one.
pub const POLICY_KEY: &str = r"SOFTWARE\Policies\Microsoft\Windows NT\DNSClient\DnsPolicyConfig";

/// The start of every rule key devlanes writes; the TLD follows.
const KEY_PREFIX: &str = "devlanes-";

/// The rule format `Add-DnsClientNrptRule` writes.
const VERSION: u32 = 2;

/// Only `GenericDNSServers` is set (no DNSSEC, DirectAccess or proxy options).
const CONFIG_OPTIONS: u32 = 8;

/// `FILE_SHARE_READ`: others may read the file while devlanes holds it.
const FILE_SHARE_READ: u32 = 1;

/// The address the rule sends lookups to: the daemon's DNS server. NRPT has
/// no port, so the server listens on port 53 there.
pub fn server() -> IpAddr {
    devlanes_resolver_dns::default_listen().ip()
}

/// The key name of the rule for `zone`: `devlanes-<tld>`.
pub fn rule_key(zone: &Zone) -> String {
    format!("{KEY_PREFIX}{}", zone.name())
}

/// `tld` as a zone, only when it is already in the form [`Zone::new`] gives
/// back (lowercase, no trailing dot). Every character of the result is a
/// lowercase letter, a digit, `-` or `.`, so nothing can break out of the
/// `.reg` text it is written into.
pub fn zone(tld: &str) -> Result<Zone, NrptError> {
    let zone = Zone::new(tld).map_err(|error| NrptError::InvalidTld(error.to_string()))?;
    if zone.name() != tld {
        return Err(NrptError::InvalidTld(format!(
            "`{tld}` is not written the way devlanes stores it (`{}`)",
            zone.name()
        )));
    }
    Ok(zone)
}

/// Failures to plan or apply a rule change.
#[derive(Debug, thiserror::Error)]
pub enum NrptError {
    /// The TLD cannot name a rule.
    #[error("{0}")]
    InvalidTld(String),
    /// Group Policy NRPT rules exist, so a local rule would be ignored.
    #[error(
        "Group Policy sets NRPT rules on this machine, so Windows ignores local ones; ask an administrator to add a rule sending .{tld} to {server}"
    )]
    GroupPolicy {
        /// The TLD the rule was for.
        tld: String,
        /// Where the rule should point.
        server: IpAddr,
    },
    /// Another program's rule covers the TLD or a name under it.
    #[error(
        "another NRPT rule already covers .{tld} ({keys}); remove it or choose another hostnames.tld",
        keys = .keys.join(", ")
    )]
    Foreign {
        /// The TLD the rule was for.
        tld: String,
        /// The other rules' key names.
        keys: Vec<String>,
    },
    /// The user answered No in the UAC prompt.
    #[error("administrator approval was declined, so the NRPT rule was not changed")]
    Cancelled,
    /// The session cannot show the UAC prompt.
    #[error("Windows needs to ask for administrator approval, but this session has no desktop")]
    NoDesktop,
    /// The generated file changed before `reg.exe` read it.
    #[error("{} changed before reg.exe could import it", .0.display())]
    FileChanged(PathBuf),
    /// `reg.exe import` failed.
    #[error("reg.exe import exited with {0}")]
    ImportFailed(u32),
    /// The registry does not show the change after a successful import.
    #[error("{key} is {state} after reg.exe import")]
    NotApplied {
        /// The rule's full key path.
        key: String,
        /// What the registry shows instead.
        state: &'static str,
    },
    /// Reading the registry or writing the file failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done.
        context: String,
        /// The failure.
        #[source]
        source: io::Error,
    },
}

impl NrptError {
    /// A stable code for the CLI's error output.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidTld(_) => "resolver_unsupported",
            Self::GroupPolicy { .. } => "resolver_group_policy",
            Self::Foreign { .. } => "resolver_rule_foreign",
            Self::Cancelled => "elevation_cancelled",
            Self::NoDesktop => "terminal_required",
            Self::FileChanged(_) | Self::ImportFailed(_) | Self::NotApplied { .. } => {
                "resolver_install_failed"
            }
            Self::Io { .. } => "io",
        }
    }

    /// An I/O failure while doing `context`.
    fn io(context: impl Into<String>) -> impl FnOnce(io::Error) -> Self {
        let context = context.into();
        move |source| Self::Io { context, source }
    }
}

/// Whether the rule for a TLD is in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleState {
    /// The key holds the values devlanes writes.
    Installed,
    /// The key exists with other values (another server, a hand edit).
    Outdated,
    /// No key.
    Missing,
}

impl RuleState {
    /// `installed`, `outdated` or `missing`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Installed => "installed",
            Self::Outdated => "outdated",
            Self::Missing => "missing",
        }
    }
}

/// The NRPT as it concerns one TLD.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The rule's full key path, for messages.
    pub key: String,
    /// Whether the rule is in place.
    pub state: RuleState,
    /// Key names of other programs' rules covering the TLD or a name under it.
    pub foreign: Vec<String>,
    /// devlanes rules for other TLDs, left by an earlier `hostnames.tld`.
    pub stale: Vec<Zone>,
    /// Group Policy rules exist, so Windows ignores every local rule.
    pub group_policy: bool,
}

/// One `reg.exe import` worth of changes: the rule to write and the rules to
/// delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    install: Option<Zone>,
    remove: Vec<Zone>,
    file: String,
}

impl Change {
    /// The `.reg` text.
    pub fn file(&self) -> &str {
        &self.file
    }

    /// The TLD whose rule is written.
    pub fn installs(&self) -> Option<&Zone> {
        self.install.as_ref()
    }

    /// The TLDs whose rules are deleted.
    pub fn removes(&self) -> &[Zone] {
        &self.remove
    }
}

/// Where the rules live. [`Nrpt::default`] is the machine's NRPT; tests point
/// [`Nrpt::under`] at scratch keys of the current user, which need no
/// elevation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nrpt {
    hive: Hive,
    rules: String,
    policies: String,
}

impl Default for Nrpt {
    fn default() -> Self {
        Self {
            hive: Hive::LocalMachine,
            rules: RULES_KEY.into(),
            policies: POLICY_KEY.into(),
        }
    }
}

impl Nrpt {
    /// Rules under `<hive>\<rules>` and Group Policy rules under
    /// `<hive>\<policies>`. Key paths may hold letters, digits, spaces and
    /// `\._-`, so they cannot break the `.reg` text.
    pub fn under(hive: Hive, rules: &str, policies: &str) -> io::Result<Self> {
        let plain = |path: &str| {
            !path.is_empty()
                && path
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || " \\._-".contains(c))
        };
        if !plain(rules) || !plain(policies) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "NRPT key paths may only hold letters, digits, spaces and \\._-",
            ));
        }
        Ok(Self {
            hive,
            rules: rules.into(),
            policies: policies.into(),
        })
    }

    /// The full path of the rule key for `zone`, as `.reg` files and
    /// messages name it.
    pub fn key_path(&self, zone: &Zone) -> String {
        let hive = match self.hive {
            Hive::LocalMachine => "HKEY_LOCAL_MACHINE",
            Hive::CurrentUser => "HKEY_CURRENT_USER",
        };
        format!(r"{hive}\{}\{}", self.rules, rule_key(zone))
    }

    /// Reads the rule for `zone`, other rules covering it, devlanes rules for
    /// other TLDs and whether Group Policy rules exist. Registry reads only.
    pub fn status(&self, zone: &Zone) -> io::Result<Status> {
        let own = rule_key(zone);
        let mut foreign = Vec::new();
        let mut stale = Vec::new();
        for name in registry::subkeys(self.hive, &self.rules)?.unwrap_or_default() {
            if name.eq_ignore_ascii_case(&own) {
                continue;
            }
            if let Some(other) = own_zone(&name) {
                stale.push(other);
                continue;
            }
            let entries = match registry::read_value(
                self.hive,
                &format!(r"{}\{name}", self.rules),
                "Name",
            )? {
                Some(RegValue::MultiString(entries)) => entries,
                Some(RegValue::String(entry)) => vec![entry],
                _ => continue,
            };
            if entries.iter().any(|entry| covers(entry, zone)) {
                foreign.push(name);
            }
        }
        let group_policy =
            registry::subkeys(self.hive, &self.policies)?.is_some_and(|rules| !rules.is_empty());
        Ok(Status {
            key: self.key_path(zone),
            state: self.rule_state(zone)?,
            foreign,
            stale,
            group_policy,
        })
    }

    /// The TLDs of every rule devlanes wrote, current and stale.
    pub fn installed(&self) -> io::Result<Vec<Zone>> {
        Ok(registry::subkeys(self.hive, &self.rules)?
            .unwrap_or_default()
            .iter()
            .filter_map(|name| own_zone(name))
            .collect())
    }

    /// What `certs trust --resolver` changes for `zone`: the rule, written
    /// fresh, and devlanes rules for other TLDs, removed. `None` when the rule
    /// is already in place and nothing is stale. Refused when Group Policy
    /// rules make a local one useless, or another rule covers the TLD.
    pub fn plan_install(&self, zone: &Zone) -> Result<Option<Change>, NrptError> {
        let status = self
            .status(zone)
            .map_err(NrptError::io("reading the NRPT"))?;
        if status.group_policy {
            return Err(NrptError::GroupPolicy {
                tld: zone.name().into(),
                server: server(),
            });
        }
        if !status.foreign.is_empty() {
            return Err(NrptError::Foreign {
                tld: zone.name().into(),
                keys: status.foreign,
            });
        }
        if status.state == RuleState::Installed && status.stale.is_empty() {
            return Ok(None);
        }
        Ok(Some(Change {
            file: self.install_file(zone, &status.stale),
            install: Some(zone.clone()),
            remove: status.stale,
        }))
    }

    /// What `certs untrust --resolver` changes: every rule devlanes wrote,
    /// whatever TLD it was for, removed. `None` when there is none.
    pub fn plan_removal(&self) -> Result<Option<Change>, NrptError> {
        let zones = self
            .installed()
            .map_err(NrptError::io("reading the NRPT"))?;
        if zones.is_empty() {
            return Ok(None);
        }
        Ok(Some(Change {
            file: self.remove_file(&zones),
            install: None,
            remove: zones,
        }))
    }

    /// The `.reg` text that deletes the rules of `replace` and writes the
    /// rule for `zone`. The rule's key is deleted first too, so values left
    /// by a hand edit do not survive.
    pub fn install_file(&self, zone: &Zone, replace: &[Zone]) -> String {
        let mut text = self.remove_file(replace);
        let key = self.key_path(zone);
        let tld = zone.name();
        let _ = write!(text, "[-{key}]\r\n\r\n[{key}]\r\n");
        push_dword(&mut text, "Version", VERSION);
        push_multi_string(&mut text, "Name", &[&format!(".{tld}")]);
        push_string(&mut text, "GenericDNSServers", &server().to_string());
        push_dword(&mut text, "ConfigOptions", CONFIG_OPTIONS);
        push_string(&mut text, "IPSECCARestriction", "");
        push_string(&mut text, "DisplayName", &format!("devlanes .{tld}"));
        push_string(
            &mut text,
            "Comment",
            &format!(
                "Sends *.{tld} lookups to the devlanes DNS server. Remove with: devlanes certs untrust --resolver"
            ),
        );
        text.push_str("\r\n");
        text
    }

    /// The `.reg` text that deletes the rules of `zones`.
    pub fn remove_file(&self, zones: &[Zone]) -> String {
        let mut text = String::from("Windows Registry Editor Version 5.00\r\n\r\n");
        for zone in zones {
            let _ = write!(text, "[-{}]\r\n\r\n", self.key_path(zone));
        }
        text
    }

    /// Imports `change` with `<System32>\reg.exe` as administrator, after the
    /// UAC prompt, then reads the rules back to confirm it.
    ///
    /// The file goes into `dir` (the private devlanes home) and is deleted
    /// afterwards. While `reg.exe` runs, devlanes holds the file open in a
    /// mode that lets others only read it, and checks its bytes first, so no
    /// other program can change what the elevated import reads. Blocks until
    /// the import ends.
    pub fn apply(&self, change: &Change, dir: &Path) -> Result<(), NrptError> {
        let reg = devlanes_sys_windows::file::system_directory()
            .map_err(NrptError::io("finding System32"))?
            .join("reg.exe");
        let path = dir.join(format!(".nrpt-{}.reg", std::process::id()));
        let imported = import_with(&path, &encode(change.file()), |file| {
            shell_execute::run_elevated(&reg, &[OsStr::new("import"), file.as_os_str()]).map_err(
                |error| match error {
                    ElevateError::Cancelled => NrptError::Cancelled,
                    ElevateError::NoDesktop => NrptError::NoDesktop,
                    ElevateError::Io(source) => NrptError::Io {
                        context: format!("running {} as administrator", reg.display()),
                        source,
                    },
                },
            )
        });
        // A leftover file holds only this change, inside the private home.
        let _ = std::fs::remove_file(&path);
        imported?;
        self.confirm(change)
    }

    /// Checks that the registry shows `change`.
    fn confirm(&self, change: &Change) -> Result<(), NrptError> {
        let expected = change
            .install
            .iter()
            .map(|zone| (zone, RuleState::Installed))
            .chain(change.remove.iter().map(|zone| (zone, RuleState::Missing)));
        for (zone, wanted) in expected {
            let state = self
                .rule_state(zone)
                .map_err(NrptError::io("reading the NRPT"))?;
            if state != wanted {
                return Err(NrptError::NotApplied {
                    key: self.key_path(zone),
                    state: state.as_str(),
                });
            }
        }
        Ok(())
    }

    /// Whether the rule for `zone` holds the values devlanes writes. Display
    /// name and comment are not compared.
    fn rule_state(&self, zone: &Zone) -> io::Result<RuleState> {
        let key = format!(r"{}\{}", self.rules, rule_key(zone));
        // A key without values exists too; listing its subkeys tells.
        if registry::subkeys(self.hive, &key)?.is_none() {
            return Ok(RuleState::Missing);
        }
        let read = |name: &str| registry::read_value(self.hive, &key, name);
        let namespace = format!(".{}", zone.name());
        let name_matches = matches!(
            read("Name")?,
            Some(RegValue::MultiString(names))
                if names.len() == 1 && names[0].eq_ignore_ascii_case(&namespace)
        );
        let matches = name_matches
            && read("Version")? == Some(RegValue::Dword(VERSION))
            && read("GenericDNSServers")? == Some(RegValue::String(server().to_string()))
            && read("ConfigOptions")? == Some(RegValue::Dword(CONFIG_OPTIONS));
        Ok(if matches {
            RuleState::Installed
        } else {
            RuleState::Outdated
        })
    }
}

/// The TLD of a key devlanes wrote (`devlanes-<tld>` with a valid TLD).
fn own_zone(key: &str) -> Option<Zone> {
    zone(key.strip_prefix(KEY_PREFIX)?).ok()
}

/// Whether an NRPT namespace entry (`.test`, `api.test`) is `zone` or a name
/// under it.
fn covers(entry: &str, zone: &Zone) -> bool {
    let entry = entry.trim().trim_matches('.').to_ascii_lowercase();
    entry == zone.name()
        || entry
            .strip_suffix(zone.name())
            .is_some_and(|rest| rest.ends_with('.'))
}

/// `"name"=dword:xxxxxxxx`.
fn push_dword(text: &mut String, name: &str, value: u32) {
    let _ = write!(text, "\"{}\"=dword:{value:08x}\r\n", escape(name));
}

/// `"name"="value"`.
fn push_string(text: &mut String, name: &str, value: &str) {
    let _ = write!(text, "\"{}\"=\"{}\"\r\n", escape(name), escape(value));
}

/// `"name"=hex(7):...`: the UTF-16LE bytes of each entry and its NUL, then
/// the NUL that ends the list.
fn push_multi_string(text: &mut String, name: &str, values: &[&str]) {
    let bytes: Vec<String> = values
        .iter()
        .flat_map(|value| value.encode_utf16().chain([0]))
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let _ = write!(text, "\"{}\"=hex(7):{}\r\n", escape(name), bytes.join(","));
}

/// A `.reg` string body: backslashes and quotes escaped.
fn escape(value: &str) -> String {
    value.replace('\\', r"\\").replace('"', "\\\"")
}

/// The file as `reg.exe` reads it: UTF-16LE with a byte order mark.
pub fn encode(text: &str) -> Vec<u8> {
    [0xFF, 0xFE]
        .into_iter()
        .chain(text.encode_utf16().flat_map(u16::to_le_bytes))
        .collect()
}

/// Writes `bytes` to `path`, holds it read-only while `run` imports it, and
/// maps a non-zero exit code to [`NrptError::ImportFailed`].
fn import_with(
    path: &Path,
    bytes: &[u8],
    run: impl FnOnce(&Path) -> Result<u32, NrptError>,
) -> Result<(), NrptError> {
    let context = |what: &str| format!("{what} {}", path.display());
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => {
            return Err(NrptError::io(context("removing"))(error));
        }
        _ => {}
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(NrptError::io(context("creating")))?;
    file.write_all(bytes)
        .map_err(NrptError::io(context("writing")))?;
    drop(file);
    // Read access with readers-only sharing: opening fails while anyone holds
    // write access, and from here on nobody can get it.
    let mut held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ)
        .open(path)
        .map_err(NrptError::io(context("opening")))?;
    let mut written = Vec::new();
    held.read_to_end(&mut written)
        .map_err(NrptError::io(context("reading")))?;
    if written != bytes {
        return Err(NrptError::FileChanged(path.to_owned()));
    }
    let code = run(path)?;
    drop(held);
    if code != 0 {
        return Err(NrptError::ImportFailed(code));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    /// Scratch rule and policy keys under `HKCU\Software`, removed on drop.
    struct Scratch {
        root: String,
        nrpt: Nrpt,
    }

    impl Scratch {
        fn new(tag: &str) -> Self {
            let root = format!(
                r"Software\devlanes-resolver-nrpt-test-{}-{tag}",
                std::process::id()
            );
            let nrpt = Nrpt::under(
                Hive::CurrentUser,
                &format!(r"{root}\DnsPolicyConfig"),
                &format!(r"{root}\Policies\DnsPolicyConfig"),
            )
            .expect("scratch keys");
            Self { root, nrpt }
        }

        /// A rule key named `name` holding `values` as its namespaces.
        fn rule(&self, name: &str, values: &[&str]) {
            registry::write_multi_string(&format!(r"{}\{name}", self.nrpt.rules), "Name", values)
                .expect("rule");
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = registry::delete_tree(&self.root);
        }
    }

    fn test_zone() -> Zone {
        zone("test").expect("zone")
    }

    /// Runs `<System32>\reg.exe import <file>` unelevated, which is enough
    /// for keys of the current user, with a 30 second limit.
    fn reg_import(file: &Path) -> Result<u32, NrptError> {
        let reg = devlanes_sys_windows::file::system_directory()
            .expect("System32")
            .join("reg.exe");
        let mut child = Command::new(reg)
            .arg("import")
            .arg(file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("reg.exe");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(status) = child.try_wait().expect("wait") {
                return Ok(status.code().map_or(1, |code| code.cast_unsigned()));
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("reg.exe import did not finish in 30 s");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Imports `text` the way [`Nrpt::apply`] does, without elevation.
    fn import(text: &str) -> Result<(), NrptError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("change.reg");
        import_with(&path, &encode(text), reg_import)
    }

    #[test]
    fn the_install_file_is_exact() {
        let nrpt = Nrpt::default();
        let text = nrpt.install_file(&test_zone(), &[zone("dev.internal").expect("zone")]);
        let key = r"HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\Dnscache\Parameters\DnsPolicyConfig";
        assert_eq!(
            text,
            format!(
                "Windows Registry Editor Version 5.00\r\n\
                 \r\n\
                 [-{key}\\devlanes-dev.internal]\r\n\
                 \r\n\
                 [-{key}\\devlanes-test]\r\n\
                 \r\n\
                 [{key}\\devlanes-test]\r\n\
                 \"Version\"=dword:00000002\r\n\
                 \"Name\"=hex(7):2e,00,74,00,65,00,73,00,74,00,00,00,00,00\r\n\
                 \"GenericDNSServers\"=\"127.53.0.1\"\r\n\
                 \"ConfigOptions\"=dword:00000008\r\n\
                 \"IPSECCARestriction\"=\"\"\r\n\
                 \"DisplayName\"=\"devlanes .test\"\r\n\
                 \"Comment\"=\"Sends *.test lookups to the devlanes DNS server. Remove with: devlanes certs untrust --resolver\"\r\n\
                 \r\n"
            )
        );
        let bytes = encode(&text);
        assert_eq!(&bytes[..6], &[0xFF, 0xFE, b'W', 0, b'i', 0]);
        assert_eq!(
            bytes.len(),
            2 + text.len() * 2,
            "ASCII text, two bytes each"
        );
    }

    #[test]
    fn the_removal_file_is_exact() {
        let nrpt = Nrpt::default();
        assert_eq!(
            nrpt.remove_file(&[test_zone()]),
            "Windows Registry Editor Version 5.00\r\n\
             \r\n\
             [-HKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Services\\Dnscache\\Parameters\\DnsPolicyConfig\\devlanes-test]\r\n\
             \r\n"
        );
    }

    #[test]
    fn tlds_that_could_inject_are_refused() {
        for bad in [
            "test]\r\n[HKEY_LOCAL_MACHINE\\SOFTWARE\\x",
            "test\"=\"x",
            "a\\b.test",
            "x.test\r\n",
            "TEST",
            "test.",
            "example.com",
            "",
        ] {
            let error = zone(bad).expect_err(bad);
            assert_eq!(error.code(), "resolver_unsupported", "{bad:?}");
        }
        assert_eq!(zone("dev.internal").expect("zone").name(), "dev.internal");
        assert_eq!(rule_key(&test_zone()), "devlanes-test");
        assert!(Nrpt::under(Hive::CurrentUser, "a]\r\n[b", "c").is_err());
        assert!(Nrpt::under(Hive::CurrentUser, "a\"b", "c").is_err());
        assert_eq!(escape(r#"a\"b"#), r#"a\\\"b"#);
    }

    #[test]
    fn namespace_entries_cover_the_tld_and_names_under_it() {
        let zone = zone("dev.internal").expect("zone");
        for covering in [
            ".dev.internal",
            "dev.internal",
            ".API.dev.internal",
            "x.dev.internal.",
        ] {
            assert!(covers(covering, &zone), "{covering}");
        }
        for other in [
            ".internal",
            ".xdev.internal",
            ".dev.internal.com",
            ".test",
            "",
        ] {
            assert!(!covers(other, &zone), "{other}");
        }
    }

    /// The generated files go through the real `reg.exe`: the rule reads back
    /// as installed, a reinstall over a stale rule works, and removal leaves
    /// nothing. Run against scratch keys of the current user.
    #[test]
    fn generated_files_round_trip_through_reg_exe() {
        let scratch = Scratch::new("round-trip");
        let nrpt = &scratch.nrpt;
        let test = test_zone();
        assert_eq!(
            nrpt.status(&test).expect("status").state,
            RuleState::Missing
        );
        let old = zone("dev.internal").expect("zone");
        import(&nrpt.install_file(&old, &[])).expect("install old");

        let change = nrpt.plan_install(&test).expect("plan").expect("a change");
        assert_eq!(change.removes(), std::slice::from_ref(&old));
        import(change.file()).expect("install");
        nrpt.confirm(&change).expect("confirmed");
        let status = nrpt.status(&test).expect("status");
        assert_eq!(status.state, RuleState::Installed, "{status:?}");
        assert!(status.stale.is_empty() && status.foreign.is_empty() && !status.group_policy);
        assert_eq!(
            nrpt.plan_install(&test).expect("plan"),
            None,
            "nothing to do"
        );

        let removal = nrpt.plan_removal().expect("plan").expect("a removal");
        assert_eq!(removal.removes(), std::slice::from_ref(&test));
        import(removal.file()).expect("remove");
        nrpt.confirm(&removal).expect("confirmed");
        assert_eq!(
            nrpt.status(&test).expect("status").state,
            RuleState::Missing
        );
        assert_eq!(nrpt.plan_removal().expect("plan"), None);
    }

    #[test]
    fn edited_rules_are_outdated() {
        let scratch = Scratch::new("outdated");
        let test = test_zone();
        import(&scratch.nrpt.install_file(&test, &[])).expect("install");
        let key = format!(r"{}\devlanes-test", scratch.nrpt.rules);
        registry::write_string(&key, "GenericDNSServers", "127.0.0.1").expect("edit");
        let status = scratch.nrpt.status(&test).expect("status");
        assert_eq!(status.state, RuleState::Outdated);
        let change = scratch.nrpt.plan_install(&test).expect("plan");
        assert!(change.is_some(), "an outdated rule is rewritten");
        assert!(matches!(
            scratch.nrpt.confirm(&change.expect("change")),
            Err(NrptError::NotApplied {
                state: "outdated",
                ..
            })
        ));
    }

    #[test]
    fn foreign_rules_refuse_the_install() {
        let scratch = Scratch::new("foreign");
        let test = test_zone();
        scratch.rule(
            "{6a1f0c5e-guid-of-another-tool}",
            &[".corp.example", "api.test"],
        );
        scratch.rule("unrelated", &[".internal"]);
        let status = scratch.nrpt.status(&test).expect("status");
        assert_eq!(status.foreign, ["{6a1f0c5e-guid-of-another-tool}"]);
        assert_eq!(status.state, RuleState::Missing);
        let error = scratch.nrpt.plan_install(&test).expect_err("refused");
        assert_eq!(error.code(), "resolver_rule_foreign");
        assert!(error.to_string().contains("6a1f0c5e"), "{error}");
        assert_eq!(
            scratch.nrpt.plan_removal().expect("plan"),
            None,
            "removal never touches other rules"
        );
    }

    #[test]
    fn group_policy_rules_refuse_the_install() {
        let scratch = Scratch::new("policy");
        let test = test_zone();
        assert!(!scratch.nrpt.status(&test).expect("status").group_policy);
        // An empty policy key holds no rules.
        registry::write_string(&scratch.nrpt.policies, "x", "").expect("policy key");
        assert!(!scratch.nrpt.status(&test).expect("status").group_policy);
        registry::write_multi_string(
            &format!(r"{}\{{policy-rule}}", scratch.nrpt.policies),
            "Name",
            &[".corp.example"],
        )
        .expect("policy rule");
        assert!(scratch.nrpt.status(&test).expect("status").group_policy);
        let error = scratch.nrpt.plan_install(&test).expect_err("refused");
        assert_eq!(error.code(), "resolver_group_policy");
        assert!(error.to_string().contains("127.53.0.1"), "{error}");
    }

    #[test]
    fn a_failed_import_and_a_held_file_are_reported() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("change.reg");
        let error = import_with(&path, b"x", |_| Ok(1)).expect_err("exit 1");
        assert!(matches!(error, NrptError::ImportFailed(1)), "{error}");
        // While the import runs, nobody can write or replace the file.
        import_with(&path, b"x", |file| {
            assert!(std::fs::OpenOptions::new().write(true).open(file).is_err());
            assert!(std::fs::remove_file(file).is_err());
            assert_eq!(std::fs::read(file).expect("readers may read"), b"x");
            Ok(0)
        })
        .expect("import");
    }
}

//! The `/etc/hosts` fallback (plan 9.5): which routed names the root helper writes.
//!
//! The composition root builds a [`HostsFile`] around the helper client and registers
//! it in [`crate::integrations::Integrations::hosts`]. Every route snapshot passes
//! through [`Inner::sync_hosts`], which recomputes the exact names and hands them to
//! [`HostsSync`]. Entries are never cleared when the daemon stops: they point at
//! loopback and only matter while the proxy listens there (helper README).

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use devlanes_config::{Resolver, UserConfig};
use devlanes_proto::RpcError;
use devlanes_resolver_hosts::{
    DesiredHosts, HelperVersion, HostsClient, HostsError, HostsFileMode, HostsFilePolicy,
    HostsSync, LocalhostResolution, Suffixes, SyncConfig, SyncState,
};
use serde_json::{Value, json};

use crate::{engine::Inner, integrations::RouteEntry, model::lock};

/// Longest `hosts.retry` waits for the attempt it triggered, covering the debounce
/// and one helper run; past it the reply carries the state seen so far.
const RETRY_WAIT: Duration = Duration::from_secs(10);

/// The project setting `lanes.hosts_file` (or the user's) as the policy's mode.
pub fn mode(value: devlanes_config::HostsFile) -> HostsFileMode {
    match value {
        devlanes_config::HostsFile::Auto => HostsFileMode::Auto,
        devlanes_config::HostsFile::Never => HostsFileMode::Never,
        devlanes_config::HostsFile::Always => HostsFileMode::Always,
    }
}

/// The user's policy and the TLD the helper may write besides `.localhost`.
///
/// `hostnames.resolver: hosts_file` behaves like `hosts_file: always`. The TLD is
/// only passed with the `resolver` strategy, where names under it are routed.
pub fn user_policy(user: &UserConfig) -> (HostsFilePolicy, Option<String>) {
    let hostnames = &user.hostnames;
    let mode = if hostnames.resolver == Resolver::HostsFile {
        Some(HostsFileMode::Always)
    } else {
        hostnames.hosts_file.map(mode)
    };
    let tld = (hostnames.resolver == Resolver::Resolver)
        .then(|| hostnames.tld.clone())
        .flatten();
    (HostsFilePolicy::new(mode), tld)
}

/// Exact names of `routes` whose lane needs the hosts file, filtered the way the
/// helper validates them. `hints` maps a lane reference to its project's
/// `lanes.hosts_file`; lanes it does not know (mirrored boxes) use the user's mode.
pub fn desired_names(
    policy: HostsFilePolicy,
    resolution: LocalhostResolution,
    suffixes: &Suffixes,
    routes: &[RouteEntry],
    hints: &BTreeMap<String, Option<HostsFileMode>>,
) -> DesiredHosts {
    policy.desired(
        resolution,
        suffixes,
        routes.iter().map(|route| {
            let hint = hints.get(&route.lane).copied().flatten();
            (hint, std::iter::once(route.host.as_str()))
        }),
    )
}

/// Spelling of a probe result in `doctor`.
fn probe_word(resolution: LocalhostResolution) -> &'static str {
    match resolution {
        LocalhostResolution::Loopback => "loopback",
        LocalhostResolution::NotLoopback => "not_loopback",
        LocalhostResolution::Fails => "fails",
    }
}

/// `{code, message}` of a helper failure.
fn error_value(error: &HostsError) -> Value {
    json!({"code": error.code(), "message": error.to_string()})
}

/// Inputs of [`HostsFile::spawn`] besides the client.
#[derive(Debug, Clone)]
pub struct HostsOptions {
    /// From [`user_policy`].
    pub policy: HostsFilePolicy,
    /// The startup probe of the system resolver; `None` when it was skipped because
    /// the user's `never` makes it irrelevant.
    pub resolution: Option<LocalhostResolution>,
    /// Whether the helper was installed at startup. A block left by an earlier run
    /// is then replaced (or removed) even when no project needs names now.
    pub installed: bool,
    /// Why the configured TLD's names are not written, when the helper refuses it.
    pub tld_error: Option<HostsError>,
    /// Debounce and backoff timing.
    pub sync: SyncConfig,
    /// The running devlanes version; the installed helper should match it.
    pub expected_version: String,
}

/// The fix for an outdated, missing or broken helper.
const REINSTALL: &str = "devlanes certs trust --hosts";

/// `doctor`'s `helper` object from the installed helper's `version`:
/// `{state, level, installed, expected, message?, fix?}` with `state` one of
/// `ok`, `mismatch` (other version, same protocol), `incompatible` (another
/// protocol: updates fail), `missing` or `unrunnable`; `level` is `ok` or `warning`.
pub fn helper_check(result: &Result<HelperVersion, HostsError>, expected: &str) -> Value {
    let warn = |state: &str, installed: Option<&str>, message: String| {
        json!({
            "state": state,
            "level": "warning",
            "installed": installed,
            "expected": expected,
            "message": message,
            "fix": REINSTALL,
        })
    };
    match result {
        Ok(found) if found.version == expected => json!({
            "state": "ok",
            "level": "ok",
            "installed": found.version,
            "expected": expected,
        }),
        Ok(found) => warn(
            "mismatch",
            Some(&found.version),
            format!(
                "The installed hosts helper is version {} and devlanes is {expected}. Run `{REINSTALL}` to reinstall it.",
                found.version
            ),
        ),
        Err(HostsError::Outdated { found }) => warn(
            "incompatible",
            None,
            format!(
                "The installed hosts helper does not match devlanes {expected} (it reports {found}), so /etc/hosts updates fail. Run `{REINSTALL}` to reinstall it."
            ),
        ),
        Err(HostsError::HelperMissing { path }) => warn(
            "missing",
            None,
            format!(
                "The hosts helper is not installed at {}. Run `{REINSTALL}` to install it.",
                path.display()
            ),
        ),
        Err(error) => warn(
            "unrunnable",
            None,
            format!(
                "Cannot run the installed hosts helper to read its version: {error}. Run `{REINSTALL}` to reinstall it."
            ),
        ),
    }
}

/// The hosts status on Windows, which resolves every `*.localhost` name by
/// itself, so devlanes never writes the hosts file there whatever
/// `hosts_file` says. Names under a `resolver` TLD go through the NRPT rule,
/// which `doctor`'s DNS check reports.
#[cfg(windows)]
#[derive(Debug, Clone)]
pub struct NativeHosts {
    policy: HostsFilePolicy,
    resolution: Option<LocalhostResolution>,
}

#[cfg(windows)]
impl NativeHosts {
    /// The user's policy and the startup probe, `None` when `never` skipped it.
    pub fn new(policy: HostsFilePolicy, resolution: Option<LocalhostResolution>) -> Self {
        Self { policy, resolution }
    }

    /// `doctor`'s `hosts` object in [`HostsFile::status`]'s shape, with
    /// `state: native`: nothing is written and no helper is involved.
    pub fn status(&self) -> Value {
        json!({
            "mode": self.policy.user_mode().to_string(),
            "probe": self.resolution.map(probe_word),
            "state": "native",
            "names": null,
            "desired": 0,
            "skipped": 0,
            "last_error": null,
            "tld_error": null,
            "helper": null,
        })
    }
}

/// The daemon's view of the hosts fallback: policy, probe result and the sync task.
pub struct HostsFile {
    policy: HostsFilePolicy,
    resolution: Option<LocalhostResolution>,
    suffixes: Suffixes,
    tld_error: Option<HostsError>,
    sync: HostsSync,
    /// Set once names were requested (or the helper was found at startup). Until
    /// then an empty set is not sent, so machines that never need the fallback
    /// never run the helper and never report `helper_missing`.
    armed: AtomicBool,
    /// Last computed set, for `doctor` and `why`.
    desired: Mutex<DesiredHosts>,
    client: HostsClient,
    expected_version: String,
    /// [`helper_check`] of the installed helper, read once per daemon run.
    helper: Arc<tokio::sync::OnceCell<Value>>,
    checking: AtomicBool,
}

impl HostsFile {
    /// Starts the sync task on the current runtime. With the helper installed,
    /// its version is checked right away; otherwise when names are first needed.
    pub fn spawn(client: HostsClient, options: HostsOptions) -> Self {
        let suffixes = client.suffixes().clone();
        let hosts = Self {
            policy: options.policy,
            resolution: options.resolution,
            suffixes,
            tld_error: options.tld_error,
            sync: HostsSync::spawn(client.clone(), options.sync),
            armed: AtomicBool::new(options.installed),
            desired: Mutex::new(DesiredHosts::default()),
            client,
            expected_version: options.expected_version,
            helper: Arc::default(),
            checking: AtomicBool::new(false),
        };
        if options.installed {
            hosts.check_helper();
        }
        hosts
    }

    /// Reads the installed helper's version once, in the background: `version`
    /// runs without sudo, so it never prompts, and with the client's timeout.
    fn check_helper(&self) {
        if self.checking.swap(true, Ordering::SeqCst) {
            return;
        }
        let (client, expected, cell) = (
            self.client.clone(),
            self.expected_version.clone(),
            self.helper.clone(),
        );
        tokio::spawn(async move {
            let check = helper_check(&client.version().await, &expected);
            if check["level"] == "warning" {
                tracing::warn!(
                    state = %check["state"],
                    message = %check["message"].as_str().unwrap_or_default(),
                    "hosts helper"
                );
            }
            let _ = cell.set(check);
        });
    }

    /// The cached check, `null` until it ran.
    fn helper_status(&self) -> Value {
        self.helper.get().cloned().unwrap_or(Value::Null)
    }

    /// A failure's `{code, message}`; when the helper version is off, the version
    /// is what the user needs to hear, not the generic failure.
    fn failure_value(&self, error: &HostsError) -> Value {
        let helper = self.helper_status();
        if helper["level"] == "warning" && helper["state"] != "missing" {
            return json!({
                "code": "helper_version_mismatch",
                "message": helper["message"],
                "fix": REINSTALL,
                "cause": error_value(error),
            });
        }
        error_value(error)
    }

    /// Recomputes the names for `routes` and hands them to the sync task, which
    /// ignores a set identical to the last one.
    pub fn update(&self, routes: &[RouteEntry], hints: &BTreeMap<String, Option<HostsFileMode>>) {
        // A skipped probe only happens under the user's `never`, where `auto` never applies.
        let resolution = self.resolution.unwrap_or(LocalhostResolution::Loopback);
        let desired = desired_names(self.policy, resolution, &self.suffixes, routes, hints);
        let names = desired.names.clone();
        *lock(&self.desired) = desired;
        if names.is_empty() && !self.armed.load(Ordering::SeqCst) {
            return;
        }
        self.armed.store(true, Ordering::SeqCst);
        // First use of the fallback: learn whether the helper matches this devlanes.
        self.check_helper();
        self.sync.update(names);
    }

    /// Drops the sudo backoff and reapplies the current set now, then waits briefly
    /// for that attempt so the reply reports its outcome.
    pub async fn retry(&self) -> Value {
        let names: BTreeSet<String> = lock(&self.desired).names.clone();
        self.armed.store(true, Ordering::SeqCst);
        let mut state = self.sync.state();
        state.borrow_and_update();
        self.sync.update(names);
        self.sync.retry();
        // A timeout leaves the earlier state in the reply; the attempt still runs.
        let _ = tokio::time::timeout(RETRY_WAIT, state.changed()).await;
        self.status()
    }

    /// `doctor`'s `hosts` object: `{mode, probe, state, names, desired, skipped,
    /// last_error, tld_error, helper}`. `last_error` is set while the latest
    /// attempt failed; `helper` is [`helper_check`] once it ran (else `null`).
    pub fn status(&self) -> Value {
        let state = self.sync.state().borrow().clone();
        let (word, names, last_error) = match &state {
            SyncState::Idle => ("idle", Value::Null, Value::Null),
            SyncState::Applied { names, .. } => ("applied", json!(names), Value::Null),
            SyncState::Failed(error) => ("failed", Value::Null, self.failure_value(error)),
        };
        let desired = lock(&self.desired);
        json!({
            "mode": self.policy.user_mode().to_string(),
            "probe": self.resolution.map(probe_word),
            "state": word,
            "names": names,
            "desired": desired.names.len(),
            "skipped": desired.skipped.len(),
            "last_error": last_error,
            "tld_error": self.tld_error.as_ref().map(error_value),
            "helper": self.helper_status(),
        })
    }

    /// A `why` reason when `hosts` should be in the block but the helper cannot
    /// write it: `helper_missing`, `sudo_denied`, `helper_outdated` or (any failure
    /// with an installed helper of another version) `helper_version_mismatch`,
    /// each fixed by `devlanes certs trust --hosts`.
    pub fn reason(&self, hosts: &[String]) -> Option<(&'static str, Value)> {
        let state = self.sync.state().borrow().clone();
        let SyncState::Failed(error) = state else {
            return None;
        };
        let failure = self.failure_value(&error);
        let mismatch = failure["code"] == "helper_version_mismatch";
        if !mismatch
            && !matches!(
                error,
                HostsError::HelperMissing { .. }
                    | HostsError::SudoDenied { .. }
                    | HostsError::Outdated { .. }
            )
        {
            return None;
        }
        let desired = lock(&self.desired);
        let affected: Vec<&String> = hosts
            .iter()
            .filter(|host| desired.names.contains(&host.to_ascii_lowercase()))
            .collect();
        if affected.is_empty() {
            return None;
        }
        if mismatch {
            return Some((
                "helper_version_mismatch",
                json!({"hosts": affected, "message": failure["message"], "fix": REINSTALL, "helper": self.helper_status()}),
            ));
        }
        Some((
            error.code(),
            json!({"hosts": affected, "message": error.to_string()}),
        ))
    }
}

impl Inner {
    /// Feeds a route snapshot to the hosts fallback, when one is registered.
    pub(crate) fn sync_hosts(&self, routes: &[RouteEntry]) {
        if let Some(hosts) = &self.integ.hosts {
            hosts.update(routes, &self.hosts_hints());
        }
    }

    /// `lanes.hosts_file` of every local lane, by canonical lane reference.
    fn hosts_hints(&self) -> BTreeMap<String, Option<HostsFileMode>> {
        self.all_lanes()
            .into_iter()
            .map(|(project, lane)| {
                let st = lane.st();
                let hint = st
                    .config
                    .loaded()
                    .and_then(|config| config.lanes.hosts_file)
                    .map(mode);
                (st.reference(&project.name()), hint)
            })
            .collect()
    }

    /// `hosts.retry`: recompute from the current routes and apply now.
    pub(crate) async fn hosts_retry(&self) -> Result<Value, RpcError> {
        let Some(hosts) = &self.integ.hosts else {
            return Ok(json!({"retried": false, "hosts": null}));
        };
        self.sync_hosts(&self.route_entries());
        Ok(json!({"retried": true, "hosts": hosts.retry().await}))
    }

    /// `doctor`'s `hosts` entry; `null` without the fallback.
    pub(crate) fn hosts_status(&self) -> Value {
        #[cfg(windows)]
        if let Some(native) = &self.integ.native_hosts {
            return native.status();
        }
        self.integ
            .hosts
            .as_ref()
            .map_or(Value::Null, |hosts| hosts.status())
    }

    /// The hosts reason for `why` about `unit` of `lane`.
    pub(crate) fn hosts_reason(&self, lane: &str, unit: &str) -> Option<(&'static str, Value)> {
        let hosts = self.integ.hosts.as_ref()?;
        let routed: Vec<String> = self
            .route_entries()
            .into_iter()
            .filter(|route| route.lane == lane && route.unit == unit)
            .map(|route| route.host)
            .collect();
        hosts.reason(&routed)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use HostsFileMode::{Always, Auto, Never};
    use LocalhostResolution::{Fails, Loopback};
    use devlanes_resolver_hosts::{
        CommandOutput, FileKind, INSTALL_PATH, PROTOCOL,
        fake::{FakeExecutor, FakeFs},
    };

    use super::*;
    use crate::integrations::RouteStatus;

    /// A route of `lane` for `host`.
    fn route(lane: &str, host: &str) -> RouteEntry {
        RouteEntry {
            host: host.into(),
            port: 20_000,
            fallback: false,
            host_header_upstream: false,
            status: RouteStatus::Idle,
            start_hint: String::new(),
            project: lane.split('@').next().unwrap_or_default().into(),
            lane: lane.into(),
            unit: "web".into(),
            certificate_names: Vec::new(),
            remote: false,
        }
    }

    /// Routes of a plain project, a workerd project and a wildcard.
    fn routes() -> Vec<RouteEntry> {
        vec![
            route("acme@2", "web.acme-2.localhost"),
            route("acme@2", "*.pages-2.acme.localhost"),
            route("worker@0", "api.worker.localhost"),
            route("box:remote@1", "web.remote-1.localhost"),
        ]
    }

    /// `acme` has no hint, `worker` says `always`.
    fn hints() -> BTreeMap<String, Option<HostsFileMode>> {
        BTreeMap::from([
            ("acme@2".to_owned(), None),
            ("worker@0".to_owned(), Some(Always)),
        ])
    }

    /// Names chosen for the fixture under `user` and `resolution`.
    fn chosen(user: Option<HostsFileMode>, resolution: LocalhostResolution) -> Vec<String> {
        let suffixes = Suffixes::new(None).expect("suffixes");
        desired_names(
            HostsFilePolicy::new(user),
            resolution,
            &suffixes,
            &routes(),
            &hints(),
        )
        .names
        .into_iter()
        .collect()
    }

    #[test]
    fn desired_names_follow_mode_probe_and_hint() {
        assert_eq!(chosen(None, Loopback), ["api.worker.localhost"]);
        assert_eq!(
            chosen(Some(Auto), Fails),
            [
                "api.worker.localhost",
                "web.acme-2.localhost",
                "web.remote-1.localhost"
            ]
        );
        assert_eq!(chosen(Some(Always), Loopback).len(), 3);
        assert!(
            chosen(Some(Never), Fails).is_empty(),
            "a repo cannot override never"
        );
    }

    #[test]
    fn wildcards_are_skipped_not_sent() {
        let suffixes = Suffixes::new(None).expect("suffixes");
        let desired = desired_names(
            HostsFilePolicy::new(Some(Always)),
            Loopback,
            &suffixes,
            &routes(),
            &hints(),
        );
        assert!(desired.names.iter().all(|name| !name.contains('*')));
        assert_eq!(desired.skipped.len(), 1);
        assert_eq!(desired.skipped[0].name, "*.pages-2.acme.localhost");
    }

    /// Windows reports the probe and writes nothing, whatever the mode.
    #[cfg(windows)]
    #[test]
    fn windows_reports_the_probe_instead_of_a_helper() {
        let status = NativeHosts::new(HostsFilePolicy::new(Some(Always)), Some(Loopback)).status();
        assert_eq!(status["state"], "native");
        assert_eq!(status["mode"], "always");
        assert_eq!(status["probe"], "loopback");
        assert!(status["helper"].is_null() && status["last_error"].is_null());
        let never = NativeHosts::new(HostsFilePolicy::new(Some(Never)), None).status();
        assert!(never["probe"].is_null(), "{never}");
    }

    #[test]
    fn user_config_maps_to_policy_and_tld() {
        let mut user = UserConfig::default();
        assert_eq!(user_policy(&user), (HostsFilePolicy::new(None), None));
        user.hostnames.hosts_file = Some(devlanes_config::HostsFile::Never);
        assert_eq!(user_policy(&user).0.user_mode(), Never);
        user.hostnames.resolver = Resolver::HostsFile;
        assert_eq!(user_policy(&user).0.user_mode(), Always);
        user.hostnames.tld = Some("test".into());
        assert_eq!(
            user_policy(&user).1,
            None,
            "tld only with the resolver strategy"
        );
        user.hostnames.resolver = Resolver::Resolver;
        user.hostnames.hosts_file = None;
        assert_eq!(
            user_policy(&user),
            (HostsFilePolicy::new(None), Some("test".into()))
        );
    }

    /// Answers `version` and `apply`; `deny` makes sudo refuse `apply`.
    fn executor(deny: Arc<AtomicBool>) -> Arc<FakeExecutor> {
        Arc::new(FakeExecutor::new(move |command| {
            if command.args.iter().any(|arg| arg == "version") {
                return Ok(CommandOutput::ok(format!(
                    "devlanes-hosts 0.1.0 protocol={PROTOCOL} build=release\n"
                )));
            }
            if deny.load(Ordering::SeqCst) {
                return Ok(CommandOutput::failed(1, "sudo: a password is required\n"));
            }
            let count = command
                .stdin
                .as_deref()
                .map_or(0, |stdin| stdin.iter().filter(|&&b| b == b'\n').count());
            Ok(CommandOutput::ok(format!("changed names={count}\n")))
        }))
    }

    /// A fallback over `executor`, with the helper present when `installed`.
    fn hosts_file(executor: Arc<FakeExecutor>, user: HostsFileMode, installed: bool) -> HostsFile {
        let fs = FakeFs::standard();
        if installed {
            fs.set(INSTALL_PATH, FileKind::File, 0, 0o755);
        }
        let client = HostsClient::new(executor, Arc::new(fs), None).expect("client");
        HostsFile::spawn(
            client,
            HostsOptions {
                policy: HostsFilePolicy::new(Some(user)),
                resolution: Some(Loopback),
                installed,
                tld_error: None,
                sync: SyncConfig::default(),
                expected_version: "0.1.0".into(),
            },
        )
    }

    /// stdin of every `apply`.
    fn applies(executor: &FakeExecutor) -> Vec<String> {
        executor
            .calls()
            .into_iter()
            .filter(|command| command.args.iter().any(|arg| arg == "apply"))
            .map(|command| String::from_utf8(command.stdin.unwrap_or_default()).expect("utf-8"))
            .collect()
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_runs_until_names_are_needed() {
        let executor = executor(Arc::default());
        let hosts = hosts_file(executor.clone(), Auto, false);
        hosts.update(&[route("acme@2", "web.acme-2.localhost")], &BTreeMap::new());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(
            executor.calls().is_empty(),
            "the resolver works and nothing is armed"
        );
        assert_eq!(hosts.status()["state"], "idle");
        assert_eq!(hosts.status()["probe"], "loopback");
    }

    #[tokio::test(start_paused = true)]
    async fn route_changes_apply_and_an_emptied_set_clears() {
        let executor = executor(Arc::default());
        let hosts = hosts_file(executor.clone(), Always, true);
        hosts.update(&routes(), &hints());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(
            applies(&executor),
            ["api.worker.localhost\nweb.acme-2.localhost\nweb.remote-1.localhost\n"]
        );
        assert_eq!(hosts.status()["state"], "applied");
        assert_eq!(hosts.status()["names"], 3);
        hosts.update(&[], &hints());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(applies(&executor).last().map(String::as_str), Some(""));
    }

    #[tokio::test(start_paused = true)]
    async fn sudo_denied_is_reported_until_retry() {
        let deny = Arc::new(AtomicBool::new(true));
        let executor = executor(deny.clone());
        let hosts = hosts_file(executor.clone(), Always, true);
        hosts.update(&routes(), &hints());
        tokio::time::sleep(Duration::from_secs(1)).await;
        let status = hosts.status();
        assert_eq!(status["state"], "failed");
        assert_eq!(status["last_error"]["code"], "sudo_denied");
        let (code, data) = hosts
            .reason(&["web.acme-2.localhost".into()])
            .expect("why reason");
        assert_eq!(code, "sudo_denied");
        assert_eq!(data["hosts"], json!(["web.acme-2.localhost"]));
        assert!(hosts.reason(&["other.localhost".into()]).is_none());

        deny.store(false, Ordering::SeqCst);
        let status = hosts.retry().await;
        assert_eq!(status["state"], "applied", "{status}");
        assert_eq!(
            applies(&executor).len(),
            2,
            "retry skips the five-minute backoff"
        );
        assert!(hosts.reason(&["web.acme-2.localhost".into()]).is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_helper_names_the_fix() {
        let executor = executor(Arc::default());
        let hosts = hosts_file(executor.clone(), Always, false);
        hosts.update(&routes(), &hints());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(hosts.status()["last_error"]["code"], "helper_missing");
        assert!(executor.calls().is_empty(), "no sudo without the helper");
        assert_eq!(hosts.status()["helper"]["state"], "missing");
    }

    /// An installed helper whose `version` prints `line` (or fails to start).
    fn versioned(line: Option<&'static str>, deny: bool) -> Arc<FakeExecutor> {
        Arc::new(FakeExecutor::new(move |command| {
            if command.args.iter().any(|arg| arg == "version") {
                assert_ne!(command.program, "sudo", "version never goes through sudo");
                return match line {
                    Some(line) => Ok(CommandOutput::ok(format!("{line}\n"))),
                    None => Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
                };
            }
            if deny {
                return Ok(CommandOutput::failed(1, "sudo: a password is required\n"));
            }
            Ok(CommandOutput::ok("changed names=0\n"))
        }))
    }

    #[test]
    /// Matching, older, other-protocol, missing and unrunnable helpers.
    fn helper_versions_are_judged() {
        let found = |version: &str| HelperVersion {
            version: version.into(),
            protocol: PROTOCOL,
            build: "release".into(),
        };
        let ok = helper_check(&Ok(found("0.4.0")), "0.4.0");
        assert_eq!(
            (ok["state"].as_str(), ok["level"].as_str()),
            (Some("ok"), Some("ok"))
        );
        let older = helper_check(&Ok(found("0.3.0")), "0.4.0");
        assert_eq!(older["state"], "mismatch");
        assert_eq!(older["level"], "warning");
        assert_eq!(older["fix"], "devlanes certs trust --hosts");
        assert_eq!(
            older["message"],
            "The installed hosts helper is version 0.3.0 and devlanes is 0.4.0. Run `devlanes certs trust --hosts` to reinstall it."
        );
        let other = helper_check(&Err(HostsError::Outdated { found: "2".into() }), "0.4.0");
        assert_eq!(other["state"], "incompatible");
        let missing = helper_check(
            &Err(HostsError::HelperMissing {
                path: INSTALL_PATH.into(),
            }),
            "0.4.0",
        );
        assert_eq!(missing["state"], "missing");
        let broken = helper_check(
            &Err(HostsError::HelperFailed {
                code: None,
                message: "permission denied".into(),
            }),
            "0.4.0",
        );
        assert_eq!(broken["state"], "unrunnable");
        assert!(
            broken["message"]
                .as_str()
                .is_some_and(|m| m.contains("permission denied"))
        );
        for check in [&older, &other, &missing, &broken] {
            assert!(
                !check["message"]
                    .as_str()
                    .unwrap_or_default()
                    .contains('\u{2014}')
            );
        }
    }

    #[tokio::test(start_paused = true)]
    /// An installed older helper is reported at startup without sudo, and a
    /// later update failure is named as the version mismatch.
    async fn an_older_installed_helper_is_named() {
        let executor = versioned(Some("devlanes-hosts 0.0.9 protocol=1 build=release"), true);
        let hosts = hosts_file(executor.clone(), Always, true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        let helper = hosts.status()["helper"].clone();
        assert_eq!(helper["state"], "mismatch", "{helper}");
        assert_eq!(helper["installed"], "0.0.9");
        assert_eq!(helper["expected"], "0.1.0");

        hosts.update(&routes(), &hints());
        tokio::time::sleep(Duration::from_secs(1)).await;
        let status = hosts.status();
        assert_eq!(
            status["last_error"]["code"], "helper_version_mismatch",
            "{status}"
        );
        assert_eq!(status["last_error"]["cause"]["code"], "sudo_denied");
        let (code, data) = hosts
            .reason(&["web.acme-2.localhost".into()])
            .expect("why reason");
        assert_eq!(code, "helper_version_mismatch");
        assert_eq!(data["fix"], "devlanes certs trust --hosts");
    }

    #[tokio::test(start_paused = true)]
    /// A matching helper is fine; one that cannot be run is reported as such.
    async fn matching_and_unrunnable_helpers() {
        let matching = hosts_file(
            versioned(Some("devlanes-hosts 0.1.0 protocol=1 build=release"), false),
            Always,
            true,
        );
        let broken = hosts_file(versioned(None, false), Always, true);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(matching.status()["helper"]["state"], "ok");
        assert_eq!(broken.status()["helper"]["state"], "unrunnable");
    }
}

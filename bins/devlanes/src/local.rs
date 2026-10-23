//! Actions that must run in the user's terminal, never in the daemon: CA trust, the
//! hosts helper and the OS resolver file (they prompt for a password) and
//! `secrets unlock` (fnox asks for the vault there).

#[cfg(unix)]
use std::net::SocketAddr;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    str::FromStr,
    sync::Arc,
};

use devlanes_cli::{Action, Rendered, Runtime};
use devlanes_client::{Client, Paths};
use devlanes_proto::{Context, Method, RpcError};
#[cfg(unix)]
use devlanes_resolver_dns::ResolverFile;
#[cfg(unix)]
use devlanes_resolver_hosts::CommandSpec;
use devlanes_trust::{CaCertificate, TrustStatus, TrustStore, TrustTarget};
use serde_json::{Value, json};

use crate::{driver::rpc_error, output::emit};

/// `certs trust --resolver` through an NRPT rule, Windows' counterpart of the
/// resolver file.
#[cfg(windows)]
pub(crate) mod nrpt;
#[cfg(windows)]
use nrpt::resolver_install;

/// Name shown in keychains and NSS databases.
const CA_NAME: &str = "devlanes development CA";

/// Routes the terminal-bound actions.
pub async fn run(runtime: &Runtime, action: Action) -> Result<u8, RpcError> {
    match action {
        Action::Trust { hosts, resolver } => {
            let paths = paths()?;
            let mut code = trust(runtime, &paths, true).await?;
            if hosts {
                code = code.max(crate::hosts::install(runtime).await?);
            }
            if resolver {
                code = code.max(resolver_install(runtime, &paths, true).await?);
            }
            Ok(code)
        }
        // Mirrors `certs trust --hosts`: the helper and resolver file go first, so a CA
        // removal failure (keychain prompt cancelled) still leaves them uninstalled.
        // A failed step is reported and the next one still runs: a helper that cannot
        // be removed must not keep the resolver file or the CA in place.
        Action::Untrust {
            hosts,
            resolver,
            keep_ca,
        } => {
            let paths = paths()?;
            let mut code = 0;
            let mut step = |result| {
                if let Some(error) = add_step(&mut code, result) {
                    let hint = crate::user_config::hint(&error);
                    emit(&runtime.reporter().error(&error, hint.as_ref()));
                }
            };
            if hosts {
                step(crate::hosts::uninstall(runtime).await);
            }
            if resolver {
                step(resolver_install(runtime, &paths, false).await);
            }
            if !keep_ca {
                step(trust(runtime, &paths, false).await);
            }
            Ok(code)
        }
        Action::Unlock(context) => unlock(runtime, context).await,
        action @ (Action::Tunnel | Action::Askpass(_)) => crate::remote::run(action).await,
        _ => Err(RpcError::new(
            "usage",
            json!({"message": "unexpected action"}),
            2,
        )),
    }
}

/// The devlanes home's paths for the trust and resolver steps.
fn paths() -> Result<Paths, RpcError> {
    Paths::discover().map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))
}

/// Folds one untrust step into the exit status (the worst one wins) and hands back
/// its error for reporting, so the caller can go on with the next step.
fn add_step(code: &mut u8, result: Result<u8, RpcError>) -> Option<RpcError> {
    match result {
        Ok(step) => {
            *code = (*code).max(step);
            None
        }
        Err(error) => {
            *code = (*code).max(error.exit_code);
            Some(error)
        }
    }
}

/// The CAROOT from `mkcert -CAROOT`: it must exit 0 and print a non-empty
/// absolute path. Anything else would make the CA load from a path relative to
/// the current directory.
pub(crate) fn mkcert_caroot(output: &std::process::Output) -> Result<PathBuf, String> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("`mkcert -CAROOT` failed: {}", stderr.trim()));
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if root.is_empty() {
        return Err("`mkcert -CAROOT` printed no directory".into());
    }
    let root = PathBuf::from(root);
    if !root.is_absolute() {
        return Err(format!(
            "`mkcert -CAROOT` printed a relative path: {}",
            root.display()
        ));
    }
    Ok(root)
}

/// Loads the CA `config` selects (devlanes' own, or mkcert's).
///
/// For removal (`create` false) devlanes' own CA is only read: `ca.pem` alone is
/// enough, and `None` means there is none to remove. Untrusting must never create
/// a CA.
pub(crate) async fn certificate(
    paths: &Paths,
    config: &devlanes_config::UserConfig,
    create: bool,
) -> Result<Option<CaCertificate>, RpcError> {
    let mkcert = mkcert_selected(config);
    let tld = ca_tld(config);
    let home = paths.home.clone();
    let pem = tokio::task::spawn_blocking(move || -> Result<Option<String>, String> {
        if mkcert {
            let output = std::process::Command::new("mkcert")
                .arg("-CAROOT")
                .stdin(std::process::Stdio::null())
                .output()
                .map_err(|error| format!("mkcert: {error}"))?;
            let root = mkcert_caroot(&output)?;
            let ca = devlanes_pki::CertificateAuthority::from_mkcert(&root)
                .map_err(|error| error.to_string())?;
            return Ok(Some(ca.certificate_pem().to_owned()));
        }
        let directory = home.join("ca");
        if create {
            let ca = devlanes_pki::CertificateAuthority::load_or_create_with_tld(
                &directory,
                tld.as_deref(),
            )
            .map_err(|error| error.to_string())?;
            return Ok(Some(ca.certificate_pem().to_owned()));
        }
        match std::fs::read_to_string(directory.join("ca.pem")) {
            Ok(pem) => Ok(Some(pem)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("reading {}: {error}", directory.display())),
        }
    })
    .await
    .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?
    .map_err(|message| RpcError::new("ca_unavailable", json!({"message": message}), 1))?;
    pem.map(|pem| {
        CaCertificate::from_pem(&pem, CA_NAME)
            .map_err(|error| RpcError::new("ca_invalid", json!({"message": error.to_string()}), 1))
    })
    .transpose()
}

/// The TLD a new devlanes CA should also permit: `hostnames.tld` when it passes the
/// resolver's special-use check. Anything else is dropped rather than widening the
/// CA's name constraints to a name that could belong to a public site.
pub(crate) fn ca_tld(config: &devlanes_config::UserConfig) -> Option<String> {
    let tld = config.hostnames.tld.as_deref()?;
    devlanes_resolver_dns::Zone::new(tld)
        .ok()
        .map(|zone| zone.name().to_owned())
}

/// How to replace a CA created before devlanes added name constraints; `None`
/// when `ca` is constrained. `certs untrust` keeps `ca.pem`, so this never fixes
/// itself.
pub(crate) fn unconstrained_ca_hint(ca: &CaCertificate, home: &Path) -> Option<String> {
    (!ca.is_name_constrained()).then(|| {
        format!(
            "{UNCONSTRAINED_CA}; recreate it: {}",
            recreate_ca_command(home)
        )
    })
}

/// Why an unconstrained devlanes CA is worth replacing.
pub(crate) const UNCONSTRAINED_CA: &str = "the devlanes CA has no name constraints (it predates them), so a leaked key could sign certificates for any site";

/// The commands that replace the CA under `home` with a constrained one
/// (`crates/pki/README.md`, "Name constraints").
#[cfg(unix)]
pub(crate) fn recreate_ca_command(home: &Path) -> String {
    format!(
        "devlanes certs untrust && rm -r {} && devlanes certs trust && devlanes daemon restart",
        home.join("ca").display()
    )
}

/// The steps that replace the CA under `home` with a constrained one
/// (`crates/pki/README.md`, "Name constraints"), in words: Windows shells
/// disagree on both `&&` and recursive delete.
#[cfg(windows)]
pub(crate) fn recreate_ca_command(home: &Path) -> String {
    format!(
        "devlanes certs untrust, delete the folder {}, then devlanes certs trust and devlanes daemon restart",
        home.join("ca").display()
    )
}

/// Whether the user config selects `tls.ca: mkcert`.
pub(crate) fn mkcert_selected(config: &devlanes_config::UserConfig) -> bool {
    matches!(config.tls.ca, devlanes_config::CaKind::Mkcert)
}

/// Configured trust targets, defaulting to the system store; mobile targets are refused.
fn targets(config: &devlanes_config::UserConfig) -> Vec<TrustTarget> {
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

/// The OS decides the store implementation at compile time. `own_ca` lets the
/// macOS keychain and the Windows root store also remove older devlanes CAs by
/// label or subject; mkcert's CA is not ours.
/// On Linux, sudo may ask for a password only when this invocation may prompt.
fn store(
    target: TrustTarget,
    own_ca: bool,
    can_prompt: bool,
) -> Result<Box<dyn TrustStore>, devlanes_trust::TrustError> {
    #[cfg(target_os = "macos")]
    {
        let _ = can_prompt;
        let trust = devlanes_trust_macos::MacosTrust::from_env()?;
        let trust = if own_ca {
            trust.with_ca_label(devlanes_trust_macos::DEVLANES_CA_LABEL)
        } else {
            trust
        };
        trust.store(target)
    }
    // Windows confirms each change in its own dialog, as macOS does, so
    // `can_prompt` changes nothing; without a desktop the store refuses first.
    #[cfg(windows)]
    {
        let _ = can_prompt;
        let trust = devlanes_trust_windows::WindowsTrust::default();
        let trust = if own_ca {
            trust.with_ca_subject(devlanes_trust_windows::DEVLANES_CA_SUBJECT)
        } else {
            trust
        };
        trust.store(target)
    }
    #[cfg(target_os = "linux")]
    {
        use devlanes_trust_linux::{LinuxTrust, Privilege};
        let _ = own_ca;
        // A terminal on stdin is not enough: agents and CI have one but must not prompt.
        let privilege = match Privilege::detect() {
            Privilege::Sudo { .. } => Privilege::Sudo {
                interactive: can_prompt,
            },
            root => root,
        };
        let home = std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            devlanes_trust::TrustError::io(
                "finding the home directory",
                std::io::Error::new(std::io::ErrorKind::NotFound, "HOME is not set"),
            )
        })?;
        LinuxTrust::new(Arc::new(devlanes_trust::SystemExecutor), home, privilege).store(target)
    }
}

/// Whether each configured trust store trusts the CA, for `doctor`:
/// `[{store, state: trusted|untrusted|unknown, reason?}]`, `null` without a CA.
/// Read-only and never prompts: a store that would need a password is `unknown`.
/// The config is read as the daemon reads it; doctor lists its problems itself.
pub(crate) async fn trust_report() -> Value {
    let Ok(paths) = Paths::discover() else {
        return Value::Null;
    };
    let config = crate::user_config::lenient(&paths).await;
    let mkcert = mkcert_selected(&config);
    let Ok(Some(certificate)) = certificate(&paths, &config, false).await else {
        return Value::Null;
    };
    let mut done = BTreeSet::new();
    let mut report = Vec::new();
    for target in targets(&config) {
        let status = async {
            target.ensure_supported()?;
            let store: Arc<dyn TrustStore> = Arc::from(store(target, !mkcert, false)?);
            if !done.insert(store.id()) {
                return Ok(None);
            }
            store.status(&certificate).await.map(Some)
        }
        .await;
        report.push(match status {
            Ok(None) => continue,
            Ok(Some(status)) => trust_entry(target, &status),
            Err(error) => {
                json!({"store": target.as_str(), "state": "unknown", "reason": error.to_string()})
            }
        });
    }
    Value::Array(report)
}

/// One store's line of [`trust_report`].
fn trust_entry(target: TrustTarget, status: &TrustStatus) -> Value {
    match status {
        TrustStatus::Installed => json!({"store": target.as_str(), "state": "trusted"}),
        TrustStatus::Missing => json!({"store": target.as_str(), "state": "untrusted"}),
        TrustStatus::Unknown { reason } => {
            json!({"store": target.as_str(), "state": "unknown", "reason": reason})
        }
    }
}

/// Installs or removes the CA in every configured store; one failure doesn't skip the rest.
/// An invalid user config fails before any store is touched: a partial read could
/// pick another CA or other stores than the file names.
async fn trust(runtime: &Runtime, paths: &Paths, install: bool) -> Result<u8, RpcError> {
    let config = crate::user_config::load(paths).await?;
    let mkcert = mkcert_selected(&config);
    let say = |text: String| {
        if !runtime.quiet {
            emit(&Rendered {
                stdout: text,
                ..Rendered::default()
            });
        }
    };
    if !install && mkcert {
        // `mkcert -install` may have added this trust for other tools; removing it
        // is mkcert's call, not a side effect of `devlanes certs untrust`.
        say(
            "kept mkcert's CA trusted (tls.ca: mkcert); run `mkcert -uninstall` to remove it\n"
                .into(),
        );
        return Ok(0);
    }
    let Some(certificate) = certificate(paths, &config, install).await? else {
        say(format!(
            "no devlanes CA under {}; nothing to untrust\n",
            paths.home.join("ca").display()
        ));
        return Ok(0);
    };
    let mut done = BTreeSet::new();
    let mut failed = false;
    let mut lines = String::new();
    for target in targets(&config) {
        let result = async {
            target.ensure_supported()?;
            let store: Arc<dyn TrustStore> =
                Arc::from(store(target, !mkcert, runtime.can_prompt())?);
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
    // mkcert roots are never constrained and not ours to replace.
    if install
        && !mkcert
        && let Some(hint) = unconstrained_ca_hint(&certificate, &paths.home)
    {
        emit(&Rendered {
            stderr: format!("warning: {hint}\n"),
            ..Rendered::default()
        });
    }
    // --quiet keeps only the failures.
    if runtime.quiet {
        lines = lines
            .lines()
            .filter(|line| line.starts_with("failed "))
            .map(|line| format!("{line}\n"))
            .collect();
    }
    emit(&Rendered {
        stdout: lines,
        ..Rendered::default()
    });
    Ok(u8::from(failed))
}

/// The OS resolver file that sends `tld` lookups to the daemon's DNS server at
/// `server`. Blocking: on Linux it asks `systemctl` for the systemd version.
#[cfg(unix)]
pub fn resolver_file(tld: &str, server: SocketAddr) -> Result<ResolverFile, String> {
    let zone = devlanes_resolver_dns::Zone::new(tld).map_err(|error| error.to_string())?;
    #[cfg(target_os = "macos")]
    {
        Ok(devlanes_resolver_dns::macos_resolver(&zone, server))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let systemctl = system_program("systemctl")
            .map_err(|_| "the resolver strategy needs systemd-resolved; systemctl was not found")?;
        let output = std::process::Command::new(systemctl)
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
#[cfg(all(unix, any(not(target_os = "macos"), test)))]
fn systemd_version(output: &str) -> Option<u32> {
    let mut words = output.lines().next()?.split_whitespace();
    (words.next()? == "systemd").then_some(())?;
    words.next()?.parse().ok()
}

/// Absolute path of a root-run program from the system directories, never `PATH`.
#[cfg(unix)]
fn system_program(name: &str) -> Result<String, RpcError> {
    devlanes_trust::find_system_program(&devlanes_trust::SystemExecutor, name)
        .map(|path| path.display().to_string())
        .ok_or_else(|| {
            RpcError::new(
                "tool_missing",
                json!({"message": format!("`{name}` was not found in {:?}", devlanes_trust::SYSTEM_PROGRAM_DIRS)}),
                1,
            )
        })
}

/// Where `certs trust --resolver` records the file it wrote, so `untrust` removes
/// that file even after `hostnames.tld` changes.
#[cfg(unix)]
fn resolver_record(paths: &Paths) -> PathBuf {
    paths.home.join("resolver-file")
}

/// What occupies a resolver file path now.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Occupant {
    Absent,
    /// Starts with the devlanes marker.
    Devlanes,
    /// Someone else's file (Valet and puma-dev both use `/etc/resolver/test`).
    Foreign,
}

/// Reads `path` to see whose file it is. Blocking.
#[cfg(unix)]
fn occupant(path: &Path) -> Result<Occupant, String> {
    match std::fs::read_to_string(path) {
        Ok(contents) if devlanes_resolver_dns::is_managed(&contents) => Ok(Occupant::Devlanes),
        Ok(_) => Ok(Occupant::Foreign),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Occupant::Absent),
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => Ok(Occupant::Foreign),
        Err(error) => Err(format!("reading {}: {error}", path.display())),
    }
}

/// Files to remove and the file to write for one `--resolver` run.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct ResolverPlan {
    remove: Vec<ResolverFile>,
    write: Option<ResolverFile>,
}

/// Decides what `--resolver` changes. `configured` is the file for the current
/// `hostnames.tld`, `recorded` the file the last install wrote. Only files that
/// start with the devlanes marker are overwritten or removed.
#[cfg(unix)]
fn resolver_plan(
    install: bool,
    configured: Option<ResolverFile>,
    recorded: Option<ResolverFile>,
    occupant: impl Fn(&Path) -> Result<Occupant, String>,
) -> Result<ResolverPlan, RpcError> {
    let foreign = |path: &Path| {
        RpcError::new(
            "resolver_file_foreign",
            json!({
                "path": path,
                "message": format!(
                    "{} was not written by devlanes (another tool such as Valet or puma-dev may own it); remove it yourself or choose another hostnames.tld",
                    path.display()
                ),
            }),
            1,
        )
    };
    let unreadable = |message: String| RpcError::new("io", json!({"message": message}), 1);
    let mut plan = ResolverPlan {
        remove: Vec::new(),
        write: None,
    };
    if install {
        let file = configured.ok_or_else(not_configured)?;
        if occupant(&file.path).map_err(unreadable)? == Occupant::Foreign {
            return Err(foreign(&file.path));
        }
        // After a TLD change the old file still routes the old TLD; drop it if it is ours.
        if let Some(old) = recorded
            && old.path != file.path
            && occupant(&old.path).map_err(unreadable)? == Occupant::Devlanes
        {
            plan.remove.push(old);
        }
        plan.write = Some(file);
    } else {
        let file = recorded.or(configured).ok_or_else(not_configured)?;
        match occupant(&file.path).map_err(unreadable)? {
            Occupant::Absent => {}
            Occupant::Foreign => return Err(foreign(&file.path)),
            Occupant::Devlanes => plan.remove.push(file),
        }
    }
    Ok(plan)
}

/// The error for `--resolver` without the strategy configured or a recorded install.
fn not_configured() -> RpcError {
    RpcError::new(
        "resolver_not_configured",
        json!({"message": "set hostnames.resolver: resolver and hostnames.tld in the user config first"}),
        2,
    )
}

/// `certs trust --resolver` writes the OS resolver file for `hostnames.tld` with
/// sudo; `certs untrust --resolver` removes the file the last install recorded.
/// Either way the OS is told to reload. Files without the devlanes marker are
/// never touched.
#[cfg(unix)]
async fn resolver_install(runtime: &Runtime, paths: &Paths, install: bool) -> Result<u8, RpcError> {
    let (tld, config_error) = match crate::user_config::load(paths).await {
        Ok(config) => {
            let hostnames = config.hostnames;
            let tld = (hostnames.resolver == devlanes_config::Resolver::Resolver)
                .then_some(hostnames.tld)
                .flatten();
            (tld, None)
        }
        Err(error) if install => return Err(error),
        // Removal can still go by the record of the last install.
        Err(error) => (None, Some(error)),
    };
    // The daemon listens on the default address; there is no setting for it.
    let server = devlanes_resolver_dns::default_listen();
    let record = resolver_record(paths);
    let lookup = tld.clone();
    let record_path = record.clone();
    let (configured, recorded) = tokio::task::spawn_blocking(move || {
        let configured = lookup.map(|tld| resolver_file(&tld, server)).transpose();
        // The record is user-writable; generated_file only accepts devlanes' own paths.
        let recorded = std::fs::read_to_string(&record_path)
            .ok()
            .and_then(|text| devlanes_resolver_dns::generated_file(Path::new(text.trim())));
        (configured, recorded)
    })
    .await
    .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))?;
    if let Some(error) = config_error
        && recorded.is_none()
    {
        return Err(error);
    }
    // Removal can proceed from the record even when the TLD is now unusable.
    let configured = match configured {
        Ok(file) => file,
        Err(message) if install || recorded.is_none() => {
            return Err(RpcError::new(
                "resolver_unsupported",
                json!({"message": message}),
                2,
            ));
        }
        Err(_) => None,
    };
    let plan = {
        let configured = configured.clone();
        let recorded = recorded.clone();
        tokio::task::spawn_blocking(move || resolver_plan(install, configured, recorded, occupant))
            .await
            .map_err(|error| RpcError::new("io", json!({"message": error.to_string()}), 1))??
    };
    if plan.remove.is_empty() && plan.write.is_none() {
        tokio::task::spawn_blocking(move || std::fs::remove_file(record))
            .await
            .ok();
        emit(&Rendered {
            stdout: "no devlanes resolver file to remove\n".into(),
            ..Rendered::default()
        });
        return Ok(0);
    }
    let command = if install {
        "devlanes certs trust --resolver"
    } else {
        "devlanes certs untrust --resolver"
    };
    let executor = crate::hosts::privileged(runtime, command).await?;
    let touched: Vec<String> = plan
        .remove
        .iter()
        .chain(&plan.write)
        .map(|file| file.path.display().to_string())
        .collect();
    if runtime.can_prompt() {
        emit(&Rendered {
            stderr: format!(
                "updating {}; sudo may ask for your password\n",
                touched.join(", ")
            ),
            ..Rendered::default()
        });
    }
    let sudo = system_program("sudo")?;
    let step = |program: &str, args: &[&str]| -> Result<CommandSpec, RpcError> {
        Ok(CommandSpec::new(sudo.as_str())
            .arg(system_program(program)?)
            .args(args.iter().map(|arg| (*arg).to_owned())))
    };
    let mut steps = Vec::new();
    let mut activations: Vec<Vec<String>> = Vec::new();
    for old in &plan.remove {
        steps.push(step("rm", &["-f", &old.path.display().to_string()])?);
        activations.extend(old.activate.clone());
    }
    if let Some(file) = &plan.write {
        let path = file.path.display().to_string();
        if let Some(parent) = file.path.parent() {
            steps.push(step("mkdir", &["-p", &parent.display().to_string()])?);
        }
        steps.push(step("tee", &[&path])?.stdin(file.contents.clone().into_bytes()));
        steps.push(step("chmod", &["644", &path])?);
        activations.extend(file.activate.clone());
    }
    activations.dedup();
    for activate in activations {
        let Some((program, args)) = activate.split_first() else {
            continue;
        };
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        steps.push(step(program, &args)?);
    }
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
    let mut stdout: String = plan
        .remove
        .iter()
        .map(|file| format!("removed {}\n", file.path.display()))
        .collect();
    let mut stderr = String::new();
    let written = plan.write.as_ref().map(|file| file.path.clone());
    if let (Some(path), Some(tld)) = (&written, &tld) {
        stdout.push_str(&format!(
            "wrote {}: *.{tld} resolves through the devlanes DNS server at {server}\n",
            path.display()
        ));
    }
    let saved = tokio::task::spawn_blocking(move || match written {
        Some(path) => std::fs::write(&record, format!("{}\n", path.display())),
        None => match std::fs::remove_file(&record) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
    })
    .await;
    if let Ok(Err(error)) = saved {
        stderr.push_str(&format!(
            "warning: could not record the installed resolver file: {error}\n"
        ));
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

/// Runs fnox in this terminal so it can prompt, then hands the values to the daemon,
/// which keeps them in memory only.
async fn unlock(runtime: &Runtime, context: Context) -> Result<u8, RpcError> {
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
    if !runtime.quiet {
        emit(&Rendered {
            stdout: format!("unlocked {count} secrets\n"),
            ..Rendered::default()
        });
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Doctor's trust entries name the store and its state.
    fn trust_entries_name_store_and_state() {
        assert_eq!(
            trust_entry(TrustTarget::System, &TrustStatus::Missing),
            json!({"store": "system", "state": "untrusted"})
        );
        assert_eq!(
            trust_entry(
                TrustTarget::Firefox,
                &TrustStatus::Unknown {
                    reason: "certutil is missing".into()
                }
            ),
            json!({"store": "firefox", "state": "unknown", "reason": "certutil is missing"})
        );
    }

    #[tokio::test]
    /// `certs trust`, `certs untrust` and both `--resolver` steps refuse a user
    /// config that the lenient load would take with one key dropped, before any
    /// trust store, sudo or resolver file is touched.
    async fn trust_commands_load_the_user_config_strictly() {
        use devlanes_cli::clap::Parser;
        let temp = tempfile::tempdir().expect("tempdir");
        let paths = Paths::new(temp.path()).expect("paths");
        std::fs::write(paths.home.join("config.yaml"), "tui:\n  theme: neon\n").expect("config");
        // Guards the real trust store: every step below must stop at the config.
        assert!(crate::user_config::load(&paths).await.is_err());
        let cli = devlanes_cli::Cli::parse_from(["devlanes", "--output", "plain"]);
        let runtime = Runtime::resolve(&cli, devlanes_cli::Signals::default()).expect("runtime");
        for install in [true, false] {
            let error = trust(&runtime, &paths, install)
                .await
                .expect_err("trust refuses");
            assert_eq!(error.code, "config_error", "{error:?}");
            let error = resolver_install(&runtime, &paths, install)
                .await
                .expect_err("resolver refuses");
            assert_eq!(error.code, "config_error", "{error:?}");
        }
    }

    #[test]
    /// A failed untrust step is handed back for reporting and only raises the status.
    fn untrust_steps_keep_going() {
        let mut code = 0;
        let failed = add_step(
            &mut code,
            Err(RpcError::new("helper_missing", json!({}), 1)),
        );
        assert_eq!(
            failed.map(|error| error.code).as_deref(),
            Some("helper_missing")
        );
        assert!(add_step(&mut code, Ok(0)).is_none());
        assert_eq!(code, 1, "a later success does not hide the failure");
    }

    // Resolver files are the Unix form; Windows writes an NRPT rule.
    #[cfg(unix)]
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

    /// An `Output` with exit `code` and `stdout`.
    #[cfg(unix)]
    fn output(code: i32, stdout: &str) -> std::process::Output {
        use std::os::unix::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code << 8),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        }
    }

    /// An `Output` with exit `code` and `stdout`.
    #[cfg(windows)]
    fn output(code: u32, stdout: &str) -> std::process::Output {
        use std::os::windows::process::ExitStatusExt;
        std::process::Output {
            status: std::process::ExitStatus::from_raw(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: b"boom".to_vec(),
        }
    }

    /// A CA without name constraints, as devlanes created before it added them.
    const UNCONSTRAINED_CA: &str = "-----BEGIN CERTIFICATE-----
MIICPTCCAeSgAwIBAgIJAJaVCy5tlb5jMAoGCCqGSM49BAMCMB8xHTAbBgNVBAMM
FG9sZCBkZXZsYW5lcyB0ZXN0IENBMCAXDTI2MTAyMTE0NTI1MloYDzIxMjYwOTI3
MTQ1MjUyWjAfMR0wGwYDVQQDDBRvbGQgZGV2bGFuZXMgdGVzdCBDQTCCAUswggED
BgcqhkjOPQIBMIH3AgEBMCwGByqGSM49AQECIQD/////AAAAAQAAAAAAAAAAAAAA
AP///////////////zBbBCD/////AAAAAQAAAAAAAAAAAAAAAP//////////////
/AQgWsY12Ko6k+ez671VdpiGvGUdBrDMU7D2O848PifSYEsDFQDEnTYIhucEk2pm
eOETnSa3gZ9+kARBBGsX0fLhLEJH+Lzm5WOkQPJ3A32BLeszoPShOUXYmMKWT+NC
4v4af5uO5+tKfA+eFivOM1drMV7Oy7ZAaDe/UfUCIQD/////AAAAAP//////////
vOb6racXnoTzucrC/GMlUQIBAQNCAASIxUCGuJknyIxMqf5MclGYTAa+e/giJixq
OQbGke4EXIGPJkdlXfvri5n+hgHH9Cwbuvnf4ez2zjp6+zyKCR8boxMwETAPBgNV
HRMBAf8EBTADAQH/MAoGCCqGSM49BAMCA0cAMEQCIBylpbhd3gjizW3ZngIiRU5v
O+HxDzOmVEDsdX+MjUbmAiBkD9cQf6fFQI4As7GUERYvQS7pD+2s2XYRMpLgi1MZ
oA==
-----END CERTIFICATE-----
";

    #[test]
    /// An old unconstrained CA gets recreate instructions; a new one does not.
    fn unconstrained_ca_is_reported() {
        #[cfg(unix)]
        let (home, ca) = (Path::new("/home/me/.devlanes"), "/home/me/.devlanes/ca");
        #[cfg(windows)]
        let (home, ca) = (
            Path::new(r"C:\Users\me\.devlanes"),
            r"C:\Users\me\.devlanes\ca",
        );
        let old = CaCertificate::from_pem(UNCONSTRAINED_CA, CA_NAME).expect("parse");
        let hint = unconstrained_ca_hint(&old, home).expect("hint");
        #[cfg(unix)]
        let remove = format!("rm -r {ca}");
        #[cfg(windows)]
        let remove = format!("delete the folder {ca}");
        for step in [
            "devlanes certs untrust",
            &remove,
            "devlanes certs trust",
            "devlanes daemon restart",
        ] {
            assert!(hint.contains(step), "{hint}");
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let new = devlanes_pki::CertificateAuthority::load_or_create_with_tld(
            &dir.path().join("ca"),
            Some("dev.internal"),
        )
        .expect("ca");
        let new = CaCertificate::from_pem(new.certificate_pem(), CA_NAME).expect("parse");
        assert_eq!(unconstrained_ca_hint(&new, home), None);
    }

    #[test]
    /// Only special-use TLDs reach the CA's name constraints.
    fn ca_tld_never_widens_constraints() {
        let mut config = devlanes_config::UserConfig::default();
        assert_eq!(ca_tld(&config), None);
        for (tld, expected) in [
            ("Dev.Internal", Some("dev.internal")),
            ("test", Some("test")),
            ("com", None),
            ("example", None),
            ("localhost", None),
        ] {
            config.hostnames.tld = Some(tld.into());
            assert_eq!(ca_tld(&config).as_deref(), expected, "{tld}");
        }
    }

    #[test]
    /// CAROOT needs a successful exit and a non-empty absolute path.
    fn mkcert_caroot_is_validated() {
        #[cfg(unix)]
        let root = "/Users/me/Library/Application Support/mkcert";
        #[cfg(windows)]
        let root = r"C:\Users\me\AppData\Local\mkcert";
        assert_eq!(
            mkcert_caroot(&output(0, &format!("{root}\n"))),
            Ok(PathBuf::from(root))
        );
        assert!(mkcert_caroot(&output(1, "/abs\n")).is_err_and(|e| e.contains("boom")));
        assert!(mkcert_caroot(&output(0, " \n")).is_err());
        assert!(mkcert_caroot(&output(0, "mkcert-root\n")).is_err_and(|e| e.contains("relative")));
    }

    /// A resolver file at `path` (contents are irrelevant to planning).
    #[cfg(unix)]
    fn file(path: &str) -> ResolverFile {
        ResolverFile {
            path: PathBuf::from(path),
            contents: String::new(),
            activate: None,
        }
    }

    /// Occupants by path for planning tests.
    #[cfg(unix)]
    fn occupants(
        entries: &'static [(&'static str, Occupant)],
    ) -> impl Fn(&Path) -> Result<Occupant, String> {
        move |path| {
            Ok(entries
                .iter()
                .find(|(p, _)| Path::new(p) == path)
                .map_or(Occupant::Absent, |(_, o)| *o))
        }
    }

    #[cfg(unix)]
    #[test]
    /// Valet's or puma-dev's `/etc/resolver/test` is neither overwritten nor removed.
    fn foreign_resolver_files_are_refused() {
        let foreign = occupants(&[("/etc/resolver/test", Occupant::Foreign)]);
        let error = resolver_plan(true, Some(file("/etc/resolver/test")), None, &foreign)
            .expect_err("install refused");
        assert_eq!(error.code, "resolver_file_foreign");
        let error = resolver_plan(false, Some(file("/etc/resolver/test")), None, &foreign)
            .expect_err("removal refused");
        assert_eq!(error.code, "resolver_file_foreign");
    }

    #[cfg(unix)]
    #[test]
    /// Install writes the configured file and drops the recorded one after a TLD
    /// change; untrust removes the recorded file, not the configured one.
    fn recorded_file_follows_tld_changes() {
        let both = occupants(&[
            ("/etc/resolver/test", Occupant::Devlanes),
            ("/etc/resolver/dev.internal", Occupant::Devlanes),
        ]);
        let plan = resolver_plan(
            true,
            Some(file("/etc/resolver/dev.internal")),
            Some(file("/etc/resolver/test")),
            &both,
        )
        .expect("plan");
        assert_eq!(plan.remove, [file("/etc/resolver/test")]);
        assert_eq!(plan.write, Some(file("/etc/resolver/dev.internal")));

        let plan = resolver_plan(
            false,
            Some(file("/etc/resolver/dev.internal")),
            Some(file("/etc/resolver/test")),
            &both,
        )
        .expect("plan");
        assert_eq!(plan.remove, [file("/etc/resolver/test")]);
        assert_eq!(plan.write, None);

        // A recorded file someone replaced since is left alone on install.
        let replaced = occupants(&[("/etc/resolver/test", Occupant::Foreign)]);
        let plan = resolver_plan(
            true,
            Some(file("/etc/resolver/dev.internal")),
            Some(file("/etc/resolver/test")),
            &replaced,
        )
        .expect("plan");
        assert!(plan.remove.is_empty());

        let nothing = resolver_plan(
            false,
            None,
            Some(file("/etc/resolver/test")),
            occupants(&[]),
        )
        .expect("plan");
        assert_eq!(
            nothing,
            ResolverPlan {
                remove: Vec::new(),
                write: None
            }
        );
        assert_eq!(
            resolver_plan(false, None, None, occupants(&[]))
                .expect_err("nothing configured")
                .code,
            "resolver_not_configured"
        );
    }

    #[cfg(unix)]
    #[test]
    /// Only marker-first files count as devlanes'.
    fn occupant_reads_the_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ours = dir.path().join("ours");
        std::fs::write(&ours, "# Managed by devlanes: x\nnameserver 127.0.0.1\n").expect("write");
        let valet = dir.path().join("valet");
        std::fs::write(&valet, "nameserver 127.0.0.1\n").expect("write");
        assert_eq!(occupant(&ours), Ok(Occupant::Devlanes));
        assert_eq!(occupant(&valet), Ok(Occupant::Foreign));
        assert_eq!(occupant(&dir.path().join("none")), Ok(Occupant::Absent));
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

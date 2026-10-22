//! `devlanes certs trust --hosts` and `certs untrust --hosts`: install or remove the `/etc/hosts`
//! helper from the user's terminal, then make the daemon retry at once.
//!
//! The commands and their checks live in `devlanes_resolver_hosts::install`; this
//! module picks the helper binary, decides whether sudo may prompt, and reports.
//! Windows installs the `devlanes-hosts` service instead (`crate::hosts_service`).

#[cfg(unix)]
use std::{
    ffi::OsString,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use async_trait::async_trait;
#[cfg(unix)]
use devlanes_cli::{Rendered, Runtime};
use devlanes_client::{Client, Paths};
#[cfg(unix)]
use devlanes_proto::RpcError;
use devlanes_proto::{Context, Method};
#[cfg(unix)]
use devlanes_resolver_hosts::{
    CommandOutput, CommandSpec, Executor, FileKind, FsProbe, INSTALL_PATH, InstallError,
    SystemExecutor, SystemFs,
    install::{self, HELPER_SOURCE_ENV},
};
use serde_json::{Value, json};

#[cfg(unix)]
use crate::output::emit;

#[cfg(windows)]
pub use crate::hosts_service::{install, uninstall};

/// Marker the helper writes first; its presence means a block exists.
#[cfg(unix)]
const BLOCK_BEGIN: &str = "# devlanes begin";

/// Installs the helper and its sudo rule, then asks a running daemon to reapply.
#[cfg(unix)]
pub async fn install(runtime: &Runtime) -> Result<u8, RpcError> {
    let source = helper_source(
        &SystemFs,
        std::env::var_os(HELPER_SOURCE_ENV),
        std::env::current_exe(),
    )
    .map_err(install_error)?;
    let executor = privileged(runtime, "devlanes certs trust --hosts").await?;
    if runtime.can_prompt() {
        emit(&Rendered {
            stderr: "installing the /etc/hosts helper; sudo may ask for your password\n".into(),
            ..Rendered::default()
        });
    }
    let version = install::install(executor.as_ref(), &SystemFs, &source)
        .await
        .map_err(install_error)?;
    let mut stdout = format!(
        "installed devlanes-hosts {} at {INSTALL_PATH}\n",
        version.version
    );
    let mut code = 0;
    if let Some(hosts) = retry_daemon().await {
        match hosts.get("state").and_then(Value::as_str) {
            Some("applied") => stdout.push_str(&format!(
                "the daemon wrote {} names to /etc/hosts\n",
                hosts.get("names").and_then(Value::as_u64).unwrap_or(0)
            )),
            Some("failed") => {
                code = 1;
                let message = hosts
                    .pointer("/last_error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                stdout.push_str(&format!(
                    "the daemon could not update /etc/hosts: {message}\n"
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

/// Clears the block, removes the rule and the helper, and prints what could not be done.
#[cfg(unix)]
pub async fn uninstall(runtime: &Runtime) -> Result<u8, RpcError> {
    let had_helper = matches!(
        SystemFs.lstat(Path::new(INSTALL_PATH)),
        Ok(Some(info)) if info.kind == FileKind::File
    );
    let executor = privileged(runtime, "devlanes certs untrust --hosts").await?;
    let warnings = install::uninstall(executor.as_ref(), &SystemFs)
        .await
        .map_err(install_error)?;
    let mut stderr: String = warnings
        .iter()
        .map(|warning| format!("warning: {warning}\n"))
        .collect();
    // Without the helper nothing can clear the block; the README says to edit it by hand.
    if !had_helper && block_present().await {
        stderr.push_str(
            "warning: the helper was already gone, so the devlanes block is still in /etc/hosts; delete the lines from `# devlanes begin` to `# devlanes end` by hand\n",
        );
    }
    // The daemon's next attempt reports `helper_missing` instead of a stale success.
    let _ = retry_daemon().await;
    emit(&Rendered {
        stdout: format!("removed {INSTALL_PATH} and its sudo rule\n"),
        stderr,
        ..Rendered::default()
    });
    Ok(0)
}

/// The helper to install: `DEVLANES_HOSTS_HELPER` when set, else `devlanes-hosts`
/// next to this binary (also next to the target of a symlinked `devlanes`).
#[cfg(unix)]
pub fn helper_source(
    fs: &dyn FsProbe,
    override_path: Option<OsString>,
    current_exe: std::io::Result<PathBuf>,
) -> Result<PathBuf, InstallError> {
    let override_path = override_path
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    match current_exe {
        Ok(exe) => install::locate_helper_source(fs, override_path, &exe),
        // Without our own path only the override can name the binary.
        Err(_) => match override_path {
            Some(path) => install::locate_helper_source(fs, Some(path), Path::new("/")),
            None => Err(InstallError::SourceMissing(Vec::new())),
        },
    }
}

/// The executor for privileged steps. sudo may prompt in an interactive terminal;
/// agents and CI get `sudo -n` on every step, and only when it already works.
/// `command` is what the user should run from a terminal instead. Windows
/// elevates through the UAC prompt instead (`crate::hosts_service`,
/// `crate::resolver_nrpt`).
#[cfg(unix)]
pub(crate) async fn privileged(
    runtime: &Runtime,
    command: &str,
) -> Result<Box<dyn Executor>, RpcError> {
    if runtime.can_prompt() {
        return Ok(Box::new(SystemExecutor));
    }
    let probe = CommandSpec::new("sudo").args(["-n", "true"]);
    let works = SystemExecutor
        .run(&probe)
        .await
        .is_ok_and(|output| output.success());
    if !works {
        return Err(RpcError::new(
            "terminal_required",
            json!({
                "command": command,
                "reason": "sudo needs a password and this session cannot prompt",
            }),
            2,
        ));
    }
    Ok(Box::new(NonInteractive(SystemExecutor)))
}

/// Runs every `sudo` as `sudo -n`, so a session that cannot answer a prompt fails
/// instead of waiting on one when the sudo timestamp lapses mid-install.
#[cfg(unix)]
struct NonInteractive<E>(E);

#[cfg(unix)]
#[async_trait]
impl<E: Executor> Executor for NonInteractive<E> {
    /// Adds `-n` to sudo commands and runs them with the inner executor.
    async fn run(&self, command: &CommandSpec) -> std::io::Result<CommandOutput> {
        self.0.run(&non_interactive(command)).await
    }
}

/// `sudo X` (or `/usr/bin/sudo X`) becomes `sudo -n X`; other commands are unchanged.
#[cfg(unix)]
fn non_interactive(command: &CommandSpec) -> CommandSpec {
    let mut command = command.clone();
    let sudo = Path::new(&command.program)
        .file_name()
        .is_some_and(|name| name == "sudo");
    if sudo && command.args.first().map(String::as_str) != Some("-n") {
        command.args.insert(0, "-n".into());
    }
    command
}

/// Maps installer failures to the CLI's error shape, keeping their stable codes.
#[cfg(unix)]
fn install_error(error: InstallError) -> RpcError {
    let exit = match error {
        InstallError::Unsupported => 2,
        _ => 1,
    };
    RpcError::new(error.code(), json!({"message": error.to_string()}), exit)
}

/// Whether `/etc/hosts` (world-readable) still holds a devlanes block.
#[cfg(unix)]
async fn block_present() -> bool {
    tokio::fs::read_to_string("/etc/hosts")
        .await
        .is_ok_and(|hosts| hosts.lines().any(|line| line.trim() == BLOCK_BEGIN))
}

/// Asks a running daemon to reapply now; a stopped daemon applies at its next start.
/// Returns the daemon's `hosts` status after the attempt.
pub(crate) async fn retry_daemon() -> Option<Value> {
    let paths = Paths::discover().ok()?;
    let mut client = Client::connect(&paths).await.ok()?;
    let reply = client
        .request(Method::HostsRetry, json!({}), Context::default())
        .await
        .ok()?;
    reply.get("hosts").filter(|hosts| !hosts.is_null()).cloned()
}

// The sudo helper is Unix only; `crate::hosts_service` has the Windows tests.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A directory with a `devlanes` binary and, when `sibling`, `devlanes-hosts`.
    fn layout(sibling: bool) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = dir.path().join("devlanes");
        std::fs::write(&exe, b"").expect("exe");
        if sibling {
            std::fs::write(dir.path().join("devlanes-hosts"), b"").expect("helper");
        }
        (dir, exe)
    }

    // `SystemFs` can't inspect files on Windows yet.
    #[cfg(unix)]
    #[test]
    fn the_sibling_of_the_running_binary_is_installed() {
        let (dir, exe) = layout(true);
        let found = helper_source(&SystemFs, None, Ok(exe)).expect("sibling");
        assert_eq!(found, dir.path().join("devlanes-hosts"));
        // An empty variable counts as unset.
        let (dir, exe) = layout(true);
        let found = helper_source(&SystemFs, Some(OsString::new()), Ok(exe)).expect("sibling");
        assert_eq!(found, dir.path().join("devlanes-hosts"));
    }

    // Creating symlinks on Windows needs developer mode or admin rights.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_binary_finds_the_helper_next_to_its_target() {
        let (dir, exe) = layout(true);
        let bin = tempfile::tempdir().expect("bin");
        let link = bin.path().join("devlanes");
        std::os::unix::fs::symlink(&exe, &link).expect("symlink");
        let found = helper_source(&SystemFs, None, Ok(link)).expect("through the link");
        let expected = dir.path().join("devlanes-hosts");
        assert_eq!(
            found.canonicalize().expect("found"),
            expected.canonicalize().expect("expected")
        );
    }

    // `SystemFs` can't inspect files on Windows yet.
    #[cfg(unix)]
    #[test]
    fn the_override_wins_and_is_not_second_guessed() {
        let (dir, exe) = layout(true);
        let custom = dir.path().join("custom-helper");
        std::fs::write(&custom, b"").expect("custom");
        let found = helper_source(
            &SystemFs,
            Some(custom.clone().into_os_string()),
            Ok(exe.clone()),
        )
        .expect("override");
        assert_eq!(found, custom);
        // A wrong override fails rather than silently using the sibling.
        let missing = dir.path().join("missing");
        let error = helper_source(&SystemFs, Some(missing.clone().into_os_string()), Ok(exe))
            .expect_err("missing override");
        assert!(matches!(error, InstallError::SourceMissing(paths) if paths == [missing]));
    }

    #[test]
    fn a_missing_helper_names_where_it_looked() {
        let (dir, exe) = layout(false);
        let error = helper_source(&SystemFs, None, Ok(exe)).expect_err("missing");
        assert_eq!(error.code(), "helper_source_missing");
        assert!(
            error
                .to_string()
                .contains(&dir.path().join("devlanes-hosts").display().to_string()),
            "{error}"
        );
        let error = helper_source(&SystemFs, None, Err(std::io::Error::other("no exe")))
            .expect_err("no exe");
        assert_eq!(error.code(), "helper_source_missing");
    }

    #[test]
    fn non_interactive_sessions_never_let_sudo_prompt() {
        let step = CommandSpec::new("sudo").args(["/bin/mv", "-f", "a", "b"]);
        assert_eq!(non_interactive(&step).to_string(), "sudo -n /bin/mv -f a b");
        let already = CommandSpec::new("sudo").args(["-n", "-k", INSTALL_PATH, "version"]);
        assert_eq!(non_interactive(&already), already);
        let plain = CommandSpec::new("/usr/bin/id").arg("-un");
        assert_eq!(non_interactive(&plain), plain);
        let absolute = CommandSpec::new("/usr/bin/sudo").args(["/bin/rm", "-f", "x"]);
        assert_eq!(
            non_interactive(&absolute).to_string(),
            "/usr/bin/sudo -n /bin/rm -f x"
        );
    }
}

//! Release and version notices. After a command in human mode, one stderr line
//! per notice, each at most once a day; `doctor` always shows the facts. The
//! daemon finds releases; nothing here downloads, installs or restarts anything.

use devlanes_cli::{OutputMode, Rendered, Runtime};
use devlanes_client::{
    Client, Paths,
    update::{InstallMethod, NoticeLog, now_ms, skew_key, skew_line, update_key, update_line},
};
use devlanes_proto::{
    Context, Hello, Method,
    release::{ReleaseCheck, UpdateInfo, is_newer},
};
use serde_json::{Value, json};

use crate::output::emit;

/// This binary's version.
const OWN: &str = env!("CARGO_PKG_VERSION");

/// Prints due notices after `method` completed. Human mode outside CI only;
/// `daemon status` already reports a version difference itself.
pub async fn after_command(client: &mut Client, runtime: &Runtime, paths: &Paths, method: Method) {
    if runtime.mode != OutputMode::Human || runtime.quiet || runtime.ci.is_some() {
        return;
    }
    if matches!(method, Method::DaemonShutdown | Method::DaemonResetState) {
        return;
    }
    let daemon = (method != Method::DaemonStatus)
        .then(|| client.peer.version.clone())
        .filter(|daemon| daemon != OWN);
    let update = client
        .request(Method::DaemonStatus, json!({}), Context::default())
        .await
        .ok()
        .and_then(|status| status.get("update").cloned())
        .and_then(|update| serde_json::from_value::<UpdateInfo>(update).ok())
        .filter(|update| is_newer(&update.latest, OWN));
    if daemon.is_none() && update.is_none() {
        return;
    }
    let home = paths.home.clone();
    let lines = tokio::task::spawn_blocking(move || {
        let mut log = NoticeLog::load(&home);
        let latest = update.map(|update| update.latest);
        let lines = due_lines(
            &mut log,
            now_ms(),
            daemon.as_deref(),
            latest.as_deref(),
            |v| InstallMethod::current().upgrade_command(v),
        );
        if !lines.is_empty() {
            // A lost write only repeats the notice sooner.
            let _ = log.save(&home);
        }
        lines
    })
    .await
    .unwrap_or_default();
    emit(&Rendered {
        stderr: lines,
        ..Rendered::default()
    });
}

/// The notice lines due at `now`, marked as shown in `log`: a daemon on another
/// version, then a newer release with `command` for it.
fn due_lines(
    log: &mut NoticeLog,
    now: u64,
    daemon: Option<&str>,
    latest: Option<&str>,
    command: impl Fn(&str) -> String,
) -> String {
    let mut lines = String::new();
    if let Some(daemon) = daemon {
        let key = skew_key(daemon, OWN);
        if log.due(&key, now) {
            lines.push_str(&format!("{}\n", skew_line(daemon, OWN)));
            log.mark_shown(&key, now);
        }
    }
    if let Some(latest) = latest {
        let key = update_key(latest);
        if log.due(&key, now) {
            lines.push_str(&format!("{}\n", update_line(latest, OWN, &command(latest))));
            log.mark_shown(&key, now);
        }
    }
    lines
}

/// Adds `release` to doctor data: this binary's version, build and install
/// method, the daemon's release check, and the upgrade command.
pub async fn annotate_doctor(client: &mut Client, data: &mut Value) {
    let status = client
        .request(Method::DaemonStatus, json!({}), Context::default())
        .await
        .unwrap_or(Value::Null);
    let check: ReleaseCheck = status
        .get("release_check")
        .cloned()
        .and_then(|check| serde_json::from_value(check).ok())
        .unwrap_or_default();
    let method = tokio::task::spawn_blocking(InstallMethod::current)
        .await
        .unwrap_or(InstallMethod::Script(Default::default()));
    let latest = check.latest.clone();
    let target = latest.as_deref().unwrap_or(OWN);
    if let Some(map) = data.as_object_mut() {
        map.insert(
            "release".into(),
            json!({
                "version": OWN,
                "build": Hello::current(0).build_id,
                "daemon_version": client.peer.version,
                "install_method": method.label(),
                "command": method.upgrade_command(target),
                "check": check.state,
                "check_off_reason": check.reason,
                "latest": latest,
                "checked_at": check.checked_at,
                // The daemon reports a release only once it has been out for a while.
                "update_available": status
                    .pointer("/update/latest")
                    .and_then(Value::as_str)
                    .is_some_and(|reported| is_newer(reported, OWN)),
            }),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notices_show_once_a_day_per_version() {
        let mut log = NoticeLog::default();
        let command = |v: &str| format!("brew upgrade devlanes-run/tap/devlanes # {v}");
        let lines = due_lines(&mut log, 1_000, Some("0.0.1"), Some("99.0.0"), command);
        assert_eq!(
            lines,
            format!(
                "daemon runs 0.0.1, this CLI is {OWN}; run devlanes daemon restart when convenient (it stops services)\n\
                 devlanes 99.0.0 is available (this is {OWN}): brew upgrade devlanes-run/tap/devlanes # 99.0.0\n"
            )
        );
        assert_eq!(
            due_lines(&mut log, 2_000, Some("0.0.1"), Some("99.0.0"), command),
            ""
        );
        // A newer release is news at once; the old one waits a day.
        assert!(
            due_lines(&mut log, 3_000, None, Some("99.1.0"), command)
                .starts_with("devlanes 99.1.0 is available")
        );
        let day = 24 * 3600 * 1000;
        assert_eq!(
            due_lines(&mut log, 1_000 + day, Some("0.0.1"), None, command)
                .lines()
                .count(),
            1
        );
    }
}

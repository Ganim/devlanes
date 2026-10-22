//! Turns a failed unit spawn into a `spawn_failed` error that names the program
//! and the cause, and into the `[devlanes]` line written to the unit's log.

use std::path::{Path, PathBuf};

use devlanes_proto::RpcError;
use serde_json::{Value, json};

/// Most PATH entries listed in `path_searched`; `path_more` counts the rest.
const PATH_SHOWN: usize = 12;

/// Prefix of log lines devlanes writes into a unit's log, so readers can tell
/// them from the unit's own output.
pub(crate) const LOG_PREFIX: &str = "[devlanes]";

/// What the daemon knew about the launch, kept before the runner consumes the spec.
pub(crate) struct Attempt {
    pub unit: String,
    pub program: String,
    pub cwd: PathBuf,
    /// The launch environment's `PATH`, the one the exec searched.
    pub path: Option<String>,
}

/// The `spawn_failed` error for `error`. Blocking: it inspects the filesystem to
/// tell a missing program from a missing working directory, so call it from
/// `spawn_blocking`.
pub(crate) fn classify(attempt: &Attempt, error: &devlanes_core::Error) -> RpcError {
    let mut data = json!({
        "unit": attempt.unit,
        "program": attempt.program,
        "message": error.to_string(),
    });
    let io = match error {
        devlanes_core::Error::Io(io) => io,
        _ => return RpcError::new("spawn_failed", data, 1),
    };
    if let Some(errno) = io.raw_os_error() {
        data["errno"] = json!(errno_name(errno));
    }
    // std reports a failed chdir in the child with the chdir's errno, which is
    // ENOENT for a missing directory, so the directory is checked first.
    if !attempt.cwd.is_dir() {
        data["reason"] = json!("cwd_missing");
        data["cwd"] = json!(attempt.cwd);
        return RpcError::new("spawn_failed", data, 1);
    }
    let explicit = os::is_explicit(&attempt.program);
    let dirs: Vec<String> = if explicit {
        Vec::new()
    } else {
        attempt
            .path
            .as_deref()
            .unwrap_or_default()
            .split(os::PATH_SEPARATOR)
            .filter(|dir| !dir.is_empty())
            .map(str::to_owned)
            .collect()
    };
    match io.kind() {
        std::io::ErrorKind::NotFound => {
            data["reason"] = json!("not_found");
            if explicit {
                data["resolved"] = json!(attempt.cwd.join(&attempt.program));
            } else {
                data["path_searched"] = json!(dirs.iter().take(PATH_SHOWN).collect::<Vec<_>>());
                if dirs.len() > PATH_SHOWN {
                    data["path_more"] = json!(dirs.len() - PATH_SHOWN);
                }
            }
        }
        std::io::ErrorKind::PermissionDenied => {
            data["reason"] = json!("not_executable");
            let found = if explicit {
                Some(attempt.cwd.join(&attempt.program))
            } else {
                first_match(&dirs, &attempt.program, &attempt.cwd)
            };
            if let Some(found) = found {
                data["resolved"] = json!(found);
            }
        }
        _ => {}
    }
    RpcError::new("spawn_failed", data, 1)
}

impl crate::engine::Inner {
    /// Classifies a failed spawn of `attempt`, writes the `[devlanes]` line to the
    /// unit's log in `log_dir` and returns the error with `lane` set.
    pub(crate) async fn spawn_failed(
        &self,
        attempt: Attempt,
        error: devlanes_core::Error,
        log_dir: PathBuf,
        lane: &str,
    ) -> RpcError {
        let unit = attempt.unit.clone();
        let mut failure = tokio::task::spawn_blocking(move || classify(&attempt, &error))
            .await
            .unwrap_or_else(|error| crate::units::internal("spawn_failed", error));
        failure.data["lane"] = json!(lane);
        let line = log_line(&failure);
        tracing::info!(unit = %unit, lane, reason = ?failure.data.get("reason"), "{line}");
        let retention = crate::units::LogRetention::from_config(&self.opts.user.logs.retention);
        if let Err(error) = crate::units::append_note(log_dir, &unit, &line, retention).await {
            tracing::warn!(
                unit = %unit,
                code = %error.code,
                "spawn failure not written to the unit log"
            );
        }
        failure
    }
}

/// The first PATH entry that holds `program`, relative entries from `cwd`.
fn first_match(dirs: &[String], program: &str, cwd: &Path) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| cwd.join(dir).join(program))
        .find(|candidate| candidate.exists())
}

/// The `[devlanes] could not start: ...` line for a `spawn_failed` error.
pub(crate) fn log_line(error: &RpcError) -> String {
    format!("{LOG_PREFIX} could not start: {}", describe(&error.data))
}

/// One sentence for the unit's log; clients word the same data themselves.
fn describe(data: &Value) -> String {
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
    let program = text("program");
    match text("reason") {
        "not_found" if os::is_explicit(program) => format!("{program}: no such file"),
        "not_found" => {
            let dirs: Vec<&str> = data
                .get("path_searched")
                .and_then(Value::as_array)
                .map(|dirs| dirs.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            let more = match data.get("path_more").and_then(Value::as_u64) {
                Some(more) => format!(" and {more} more"),
                None => String::new(),
            };
            format!(
                "{program}: command not found on PATH ({}{more})",
                if dirs.is_empty() {
                    "PATH is empty".to_owned()
                } else {
                    dirs.join(os::PATH_SEPARATOR)
                }
            )
        }
        "not_executable" => format!("{program}: not executable (permission denied)"),
        "cwd_missing" => format!("working directory {} does not exist", text("cwd")),
        _ => match data.get("errno").and_then(Value::as_str) {
            Some(errno) => format!("{program}: {} ({errno})", text("message")),
            None => format!("{program}: {}", text("message")),
        },
    }
}

/// How the host spells `PATH` lists and program paths.
#[cfg(unix)]
mod os {
    /// Separator between `PATH` entries.
    pub(super) const PATH_SEPARATOR: &str = ":";

    /// Whether `program` names a file instead of a command looked up on `PATH`.
    pub(super) fn is_explicit(program: &str) -> bool {
        program.contains('/')
    }
}

/// How the host spells `PATH` lists and program paths.
#[cfg(windows)]
mod os {
    /// Separator between `PATH` entries.
    pub(super) const PATH_SEPARATOR: &str = ";";

    /// Whether `program` names a file instead of a command looked up on `PATH`:
    /// either separator, or a drive prefix (`C:tool` is relative to drive C).
    pub(super) fn is_explicit(program: &str) -> bool {
        program.contains(['/', '\\'])
            || matches!(program.as_bytes(), [letter, b':', ..] if letter.is_ascii_alphabetic())
    }
}

/// Symbolic name of the exec errors a spawn can report; the number otherwise.
#[cfg(windows)]
fn errno_name(code: i32) -> String {
    let name = match code {
        2 => "ERROR_FILE_NOT_FOUND",
        3 => "ERROR_PATH_NOT_FOUND",
        5 => "ERROR_ACCESS_DENIED",
        8 => "ERROR_NOT_ENOUGH_MEMORY",
        87 => "ERROR_INVALID_PARAMETER",
        123 => "ERROR_INVALID_NAME",
        193 => "ERROR_BAD_EXE_FORMAT",
        206 => "ERROR_FILENAME_EXCED_RANGE",
        267 => "ERROR_DIRECTORY",
        740 => "ERROR_ELEVATION_REQUIRED",
        _ => return format!("error {code}"),
    };
    name.to_owned()
}

/// Symbolic name of the exec errors a spawn can report; the number otherwise.
#[cfg(unix)]
fn errno_name(errno: i32) -> String {
    let errno = rustix::io::Errno::from_raw_os_error(errno);
    let name = [
        (rustix::io::Errno::NOENT, "ENOENT"),
        (rustix::io::Errno::ACCESS, "EACCES"),
        (rustix::io::Errno::PERM, "EPERM"),
        (rustix::io::Errno::NOEXEC, "ENOEXEC"),
        (rustix::io::Errno::NOTDIR, "ENOTDIR"),
        (rustix::io::Errno::ISDIR, "EISDIR"),
        (rustix::io::Errno::TOOBIG, "E2BIG"),
        (rustix::io::Errno::TXTBSY, "ETXTBSY"),
        (rustix::io::Errno::LOOP, "ELOOP"),
        (rustix::io::Errno::NAMETOOLONG, "ENAMETOOLONG"),
        (rustix::io::Errno::NOMEM, "ENOMEM"),
        (rustix::io::Errno::AGAIN, "EAGAIN"),
        (rustix::io::Errno::MFILE, "EMFILE"),
        (rustix::io::Errno::NFILE, "ENFILE"),
    ]
    .into_iter()
    .find(|(known, _)| *known == errno)
    .map(|(_, name)| name.to_owned());
    name.unwrap_or_else(|| format!("errno {}", errno.raw_os_error()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An attempt in `cwd` with `PATH` set to `path`.
    fn attempt(program: &str, cwd: &Path, path: &str) -> Attempt {
        Attempt {
            unit: "api".into(),
            program: program.into(),
            cwd: cwd.into(),
            path: Some(path.into()),
        }
    }

    /// The error a real spawn of `attempt` returns.
    fn spawned(attempt: &Attempt) -> devlanes_core::Error {
        std::process::Command::new(&attempt.program)
            .current_dir(&attempt.cwd)
            .env_clear()
            .env("PATH", attempt.path.clone().unwrap_or_default())
            .spawn()
            .map(|_| ())
            .expect_err("the spawn fails")
            .into()
    }

    #[test]
    /// A program missing from PATH names the program and the directories searched.
    fn missing_program_lists_the_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let searched: Vec<String> = ["a", "b"]
            .map(|name| dir.path().join(name).display().to_string())
            .into();
        let joined = searched.join(os::PATH_SEPARATOR);
        // Not `fixture-server`: Windows also searches the test binary's directory.
        let attempt = attempt("devlanes-missing-tool", dir.path(), &joined);
        let error = classify(&attempt, &spawned(&attempt));
        assert_eq!(error.code, "spawn_failed");
        assert_eq!(error.data["reason"], "not_found");
        assert_eq!(error.data["program"], "devlanes-missing-tool");
        // On Windows std searches PATH itself and reports a miss as a plain
        // `NotFound` without an OS code, so there is no errno to name.
        #[cfg(unix)]
        assert_eq!(error.data["errno"], "ENOENT");
        assert_eq!(error.data["path_searched"], json!(searched));
        assert_eq!(
            log_line(&error),
            format!(
                "[devlanes] could not start: devlanes-missing-tool: command not found on PATH ({joined})"
            )
        );
    }

    #[cfg(windows)]
    #[test]
    /// Backslashes and drive prefixes make a program explicit on Windows.
    fn windows_explicit_programs() {
        for program in [
            r".\tool",
            r"bin\tool",
            "./tool",
            r"C:\tools\tool.exe",
            "c:tool",
        ] {
            assert!(os::is_explicit(program), "{program}");
        }
        for program in ["tool", "tool.exe", "npm.cmd"] {
            assert!(!os::is_explicit(program), "{program}");
        }
    }

    #[test]
    /// A long PATH is cut to the first entries with a count of the rest.
    fn long_path_is_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path: Vec<String> = (0..20).map(|n| format!("/nonexistent/{n}")).collect();
        let attempt = attempt("missing-tool", dir.path(), &path.join(os::PATH_SEPARATOR));
        let error = classify(&attempt, &spawned(&attempt));
        assert_eq!(
            error.data["path_searched"].as_array().map(Vec::len),
            Some(PATH_SHOWN)
        );
        assert_eq!(error.data["path_more"], 8);
    }

    #[cfg(unix)]
    #[test]
    /// A file without the execute bit is reported as not executable.
    fn non_executable_program() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let script = dir.path().join("tool");
        std::fs::write(&script, "#!/bin/sh\n").expect("script");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o644)).expect("mode");
        let attempt = attempt("./tool", dir.path(), "");
        let error = classify(&attempt, &spawned(&attempt));
        assert_eq!(error.data["reason"], "not_executable", "{}", error.data);
        assert_eq!(error.data["errno"], "EACCES");
        assert!(log_line(&error).contains("./tool: not executable"));
    }

    #[test]
    /// A missing working directory is named instead of blaming the program.
    fn missing_cwd_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("gone");
        let real = attempt("sh", dir.path(), "/bin:/usr/bin");
        let error = std::process::Command::new("sh")
            .current_dir(&gone)
            .spawn()
            .map(|_| ())
            .expect_err("no cwd")
            .into();
        let error = classify(
            &Attempt {
                cwd: gone.clone(),
                ..real
            },
            &error,
        );
        assert_eq!(error.data["reason"], "cwd_missing");
        assert_eq!(error.data["cwd"], json!(gone));
        assert!(log_line(&error).contains("does not exist"));
    }

    #[cfg(unix)]
    #[test]
    /// Other errors keep their message and gain the program and errno name.
    fn other_errors_keep_the_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let attempt = attempt("tool", dir.path(), "/bin");
        let error = classify(
            &attempt,
            &std::io::Error::from_raw_os_error(rustix::io::Errno::TOOBIG.raw_os_error()).into(),
        );
        assert!(error.data.get("reason").is_none());
        assert_eq!(error.data["errno"], "E2BIG");
        assert!(log_line(&error).starts_with("[devlanes] could not start: tool: "));
        assert!(log_line(&error).ends_with("(E2BIG)"));
    }
}

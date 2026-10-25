//! Turns a failed unit spawn into a `spawn_failed` error that names the program
//! and the cause, and into the `[devlanes]` line written to the unit's log.

use std::path::{Path, PathBuf};

use devlanes_core::paths::names_a_path;
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
    // The runner's refusals name their case; a missing program is classified
    // below like any other.
    #[cfg(windows)]
    let refusal = io
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<devlanes_core::SpawnRefusal>());
    #[cfg(windows)]
    if let Some(refusal) = refusal
        && refusal.code != "not_found"
    {
        data["reason"] = json!(refusal.code);
        data["message"] = json!(refusal.message);
        if refusal.code == "cwd_too_long" {
            data["cwd"] = json!(attempt.cwd);
        }
        return RpcError::new("spawn_failed", data, 1);
    }
    // std reports a failed chdir in the child with the chdir's errno, which is
    // ENOENT for a missing directory, so the directory is checked first.
    if !attempt.cwd.is_dir() {
        data["reason"] = json!("cwd_missing");
        data["cwd"] = json!(attempt.cwd);
        return RpcError::new("spawn_failed", data, 1);
    }
    let explicit = names_a_path(&attempt.program);
    let dirs: Vec<String> = if explicit {
        Vec::new()
    } else {
        path_dirs(attempt.path.as_deref().unwrap_or_default())
    };
    // The directories the runner searched, relative entries already resolved.
    #[cfg(windows)]
    let dirs = match refusal {
        Some(refusal) if !explicit => refusal
            .searched
            .iter()
            .map(|dir| dir.display().to_string())
            .collect(),
        _ => dirs,
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
        // The unit's log gets the searched PATH; the daemon log names the cause only.
        let reason = failure.data.get("reason").and_then(Value::as_str);
        tracing::warn!(
            unit = %unit,
            lane,
            reason = reason.unwrap_or("other"),
            "could not start: {}",
            describe(&failure.data, false)
        );
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
    format!(
        "{LOG_PREFIX} could not start: {}",
        describe(&error.data, true)
    )
}

/// One sentence for the unit's log, with the PATH searched when `path` is
/// set; clients word the same data themselves.
fn describe(data: &Value, path: bool) -> String {
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or_default();
    let program = text("program");
    match text("reason") {
        "not_found" if names_a_path(program) => format!("{program}: no such file"),
        "not_found" if !path => format!("{program}: command not found on PATH"),
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
                    dirs.join(PATH_SEPARATOR)
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

/// What separates `PATH` entries.
#[cfg(unix)]
const PATH_SEPARATOR: &str = ":";

/// What separates `PATH` entries.
#[cfg(windows)]
const PATH_SEPARATOR: &str = ";";

/// The non-empty entries of `path`.
#[cfg(unix)]
fn path_dirs(path: &str) -> Vec<String> {
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(str::to_owned)
        .collect()
}

/// The non-empty entries of `path`, split as Windows does (`;`, with double
/// quotes protecting a `;` inside an entry).
#[cfg(windows)]
fn path_dirs(path: &str) -> Vec<String> {
    std::env::split_paths(path)
        .map(|dir| dir.display().to_string())
        .filter(|dir| !dir.is_empty())
        .collect()
}

/// Symbolic name of the Windows errors a spawn can report; the number
/// otherwise.
#[cfg(windows)]
fn errno_name(errno: i32) -> String {
    let name = match errno {
        2 => "ERROR_FILE_NOT_FOUND",
        3 => "ERROR_PATH_NOT_FOUND",
        5 => "ERROR_ACCESS_DENIED",
        193 => "ERROR_BAD_EXE_FORMAT",
        267 => "ERROR_DIRECTORY",
        _ => return format!("os error {errno}"),
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

// Windows PATH syntax, error names and the runner's refusals.
#[cfg(all(test, windows))]
mod windows_tests {
    use std::io;

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

    /// The error the Windows runner returns for a refusal with `code`.
    fn refused(kind: io::ErrorKind, code: &'static str, searched: &[&str]) -> devlanes_core::Error {
        devlanes_core::Error::Io(io::Error::new(
            kind,
            devlanes_core::SpawnRefusal {
                code,
                message: format!("refused: {code}"),
                searched: searched.iter().map(PathBuf::from).collect(),
            },
        ))
    }

    #[test]
    /// A program missing from PATH lists the directories the runner searched,
    /// joined with `;`.
    fn missing_program_lists_the_runner_search() {
        let dir = tempfile::tempdir().expect("tempdir");
        let attempt = attempt("fixture-server", dir.path(), r"C:\ignored");
        let error = classify(
            &attempt,
            &refused(io::ErrorKind::NotFound, "not_found", &[r"C:\a", r"C:\b"]),
        );
        assert_eq!(error.code, "spawn_failed");
        assert_eq!(error.data["reason"], "not_found");
        assert_eq!(error.data["path_searched"], json!([r"C:\a", r"C:\b"]));
        assert_eq!(
            log_line(&error),
            r"[devlanes] could not start: fixture-server: command not found on PATH (C:\a;C:\b)"
        );
        // The daemon log leaves the directories to the unit's log.
        assert_eq!(
            describe(&error.data, false),
            "fixture-server: command not found on PATH"
        );
    }

    #[test]
    /// Without the runner's list, `PATH` is split on `;` with quotes, and a
    /// long one is cut with a count of the rest.
    fn path_splits_on_semicolons() {
        assert_eq!(
            path_dirs(r#"C:\a;;"C:\semi;colon";C:\b"#),
            [r"C:\a", r"C:\semi;colon", r"C:\b"]
        );
        let dir = tempfile::tempdir().expect("tempdir");
        let path: Vec<String> = (0..20).map(|n| format!(r"C:\nonexistent\{n}")).collect();
        let attempt = attempt("missing-tool", dir.path(), &path.join(";"));
        let error = classify(&attempt, &io::Error::from(io::ErrorKind::NotFound).into());
        assert_eq!(
            error.data["path_searched"].as_array().map(Vec::len),
            Some(PATH_SHOWN)
        );
        assert_eq!(error.data["path_more"], 8);
    }

    #[test]
    /// Backslashes and drive letters make a path explicit: the error names
    /// the file instead of PATH.
    fn explicit_paths_name_the_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        for program in [r".\tool", "bin/tool", r"C:\tools\tool.exe"] {
            let attempt = attempt(program, dir.path(), r"C:\bin");
            let error = classify(
                &attempt,
                &refused(io::ErrorKind::NotFound, "not_found", &[r"C:\elsewhere"]),
            );
            assert_eq!(error.data["reason"], "not_found");
            assert!(error.data.get("path_searched").is_none(), "{program}");
            assert_eq!(error.data["resolved"], json!(dir.path().join(program)));
            assert!(log_line(&error).ends_with(": no such file"), "{program}");
        }
    }

    #[test]
    /// The runner's refusals keep their code as the reason and their message.
    fn refusals_keep_their_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        for code in [
            "shell_missing",
            "cwd_too_long",
            "unsafe_batch_argument",
            "invalid_command",
            "invalid_environment",
        ] {
            let attempt = attempt("/bin/sh", dir.path(), r"C:\bin");
            let error = classify(&attempt, &refused(io::ErrorKind::InvalidInput, code, &[]));
            assert_eq!(error.code, "spawn_failed");
            assert_eq!(error.data["reason"], code);
            assert_eq!(error.data["message"], format!("refused: {code}"));
            assert_eq!(
                log_line(&error),
                format!("[devlanes] could not start: /bin/sh: refused: {code}")
            );
        }
        // Refused before any directory was entered, so it is not blamed.
        let gone = attempt("/bin/sh", &dir.path().join("gone"), "");
        let error = classify(
            &gone,
            &refused(io::ErrorKind::InvalidInput, "shell_missing", &[]),
        );
        assert_eq!(error.data["reason"], "shell_missing");
    }

    #[test]
    /// A missing working directory is named instead of blaming the program.
    fn missing_cwd_is_named() {
        let dir = tempfile::tempdir().expect("tempdir");
        let gone = dir.path().join("gone");
        let attempt = attempt("tool", &gone, r"C:\bin");
        let error = classify(&attempt, &io::Error::from_raw_os_error(267).into());
        assert_eq!(error.data["reason"], "cwd_missing");
        assert_eq!(error.data["cwd"], json!(gone));
        assert_eq!(error.data["errno"], "ERROR_DIRECTORY");
    }

    #[test]
    /// Common Windows error codes get their names, others their number.
    fn error_names() {
        assert_eq!(errno_name(2), "ERROR_FILE_NOT_FOUND");
        assert_eq!(errno_name(3), "ERROR_PATH_NOT_FOUND");
        assert_eq!(errno_name(5), "ERROR_ACCESS_DENIED");
        assert_eq!(errno_name(193), "ERROR_BAD_EXE_FORMAT");
        assert_eq!(errno_name(267), "ERROR_DIRECTORY");
        assert_eq!(errno_name(1455), "os error 1455");
        let dir = tempfile::tempdir().expect("tempdir");
        let attempt = attempt("tool", dir.path(), r"C:\bin");
        let error = classify(&attempt, &io::Error::from_raw_os_error(193).into());
        assert!(error.data.get("reason").is_none());
        assert!(log_line(&error).ends_with("(ERROR_BAD_EXE_FORMAT)"));
    }
}

// Unix PATH syntax and errno names.
#[cfg(all(test, unix))]
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
        let attempt = attempt(
            "fixture-server",
            dir.path(),
            "/nonexistent/a:/nonexistent/b",
        );
        let error = classify(&attempt, &spawned(&attempt));
        assert_eq!(error.code, "spawn_failed");
        assert_eq!(error.data["reason"], "not_found");
        assert_eq!(error.data["program"], "fixture-server");
        assert_eq!(error.data["errno"], "ENOENT");
        assert_eq!(
            error.data["path_searched"],
            json!(["/nonexistent/a", "/nonexistent/b"])
        );
        assert_eq!(
            log_line(&error),
            "[devlanes] could not start: fixture-server: command not found on PATH (/nonexistent/a:/nonexistent/b)"
        );
        // The daemon log leaves the directories to the unit's log.
        assert_eq!(
            describe(&error.data, false),
            "fixture-server: command not found on PATH"
        );
    }

    #[test]
    /// A long PATH is cut to the first entries with a count of the rest.
    fn long_path_is_truncated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path: Vec<String> = (0..20).map(|n| format!("/nonexistent/{n}")).collect();
        let attempt = attempt("missing-tool", dir.path(), &path.join(":"));
        let error = classify(&attempt, &spawned(&attempt));
        assert_eq!(
            error.data["path_searched"].as_array().map(Vec::len),
            Some(PATH_SHOWN)
        );
        assert_eq!(error.data["path_more"], 8);
    }

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

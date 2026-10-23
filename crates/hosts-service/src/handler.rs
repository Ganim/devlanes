//! Answers one request against a hosts file, with the helper's validation and
//! exit codes. No pipe, no service: tests call [`handle`] on a temp file.

use std::collections::BTreeSet;
use std::path::PathBuf;

use devlanes_hosts_helper::block::{self, BlockError};
use devlanes_hosts_helper::exit;
use devlanes_hosts_helper::names::{self, Suffixes};
use devlanes_hosts_helper::protocol::{Request, Response, version_line};

use crate::write::{self, Outcome, UpdateError, WriteError};

/// The file requests edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The hosts file. The service uses the one in System32; only tests pass
    /// another one.
    pub hosts: PathBuf,
    /// Flush the DNS Client cache after a change; only for the real file.
    pub flush: bool,
}

/// A [`Response`] for `error`, with the helper's exit code.
fn write_failure(error: WriteError) -> Response {
    let code = match error {
        WriteError::Io { .. } => exit::IO,
        WriteError::Symlink(_)
        | WriteError::NotRegular(_)
        | WriteError::ReadOnly(_)
        | WriteError::Changed(_) => exit::REFUSED_FILE,
    };
    Response::failure(code, error.to_string())
}

/// A [`Response`] for a failed update.
fn update_failure(error: UpdateError<BlockError>) -> Response {
    match error {
        UpdateError::Content(error) => Response::failure(exit::REFUSED_FILE, error.to_string()),
        UpdateError::Write(error) => write_failure(error),
    }
}

/// Runs `request` against `target`. Blocking: it reads, writes and may run
/// `ipconfig`.
pub fn handle(request: &Request, target: &Target) -> Response {
    match request {
        Request::Version => Response::ok(format!("{}\n", version_line(env!("CARGO_PKG_VERSION")))),
        Request::Status => match write::read(&target.hosts) {
            Err(error) => write_failure(error),
            Ok(bytes) => match block::current(&bytes) {
                Err(error) => Response::failure(exit::REFUSED_FILE, error.to_string()),
                Ok(current) => Response::ok(String::from_utf8_lossy(current.unwrap_or_default())),
            },
        },
        Request::Clear => match write::update(&target.hosts, |bytes| {
            block::rewrite(bytes, &BTreeSet::new())
        }) {
            Ok(outcome) => finish(target, outcome, 0),
            Err(error) => update_failure(error),
        },
        Request::Apply { tld, names } => {
            let suffixes = match Suffixes::new(tld.as_deref()) {
                Ok(suffixes) => suffixes,
                Err(error) => return Response::failure(exit::INVALID_NAMES, error.to_string()),
            };
            let names = match names::parse_names(names, &suffixes) {
                Ok(names) => names,
                Err(error) => return Response::failure(exit::INVALID_NAMES, error.to_string()),
            };
            match write::update(&target.hosts, |bytes| block::rewrite(bytes, &names)) {
                Ok(outcome) => finish(target, outcome, names.len()),
                Err(error) => update_failure(error),
            }
        }
    }
}

/// Flushes after a change and builds the helper's `changed names=<n>` line.
fn finish(target: &Target, outcome: Outcome, count: usize) -> Response {
    // The warning has no reader: the daemon only acts on the result line, and the
    // DNS Client service reloads the file by itself.
    if outcome == Outcome::Changed && target.flush {
        let _ = crate::flush::flush();
    }
    let word = match outcome {
        Outcome::Changed => "changed",
        Outcome::Unchanged => "unchanged",
    };
    Response::ok(format!("{word} names={count}\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    use devlanes_hosts_helper::protocol::PIPE_PROTOCOL;

    const STOCK: &str = "# Copyright (c) 1993-2009 Microsoft Corp.\r\n#\r\n# localhost name resolution is handled within DNS itself.\r\n#\t127.0.0.1       localhost\r\n";

    fn target() -> (tempfile::TempDir, Target) {
        let dir = tempfile::tempdir().expect("tempdir");
        let hosts = dir.path().join("hosts");
        fs::write(&hosts, STOCK).expect("seed");
        (
            dir,
            Target {
                hosts,
                flush: false,
            },
        )
    }

    fn apply(tld: Option<&str>, names: &str) -> Request {
        Request::Apply {
            tld: tld.map(str::to_owned),
            names: names.as_bytes().to_vec(),
        }
    }

    #[test]
    fn apply_status_and_clear_keep_crlf() {
        let (_dir, target) = target();
        let applied = handle(
            &apply(Some("test"), "web.acme-2.localhost\napi.acme.test\n"),
            &target,
        );
        assert_eq!(applied, Response::ok("changed names=2\n"));
        let content = fs::read_to_string(&target.hosts).expect("read");
        assert!(content.starts_with(STOCK), "{content}");
        assert!(
            content.ends_with(
                "# devlanes begin\r\n# Managed by devlanes-hosts. Lines between these markers are replaced on every update.\r\n127.0.0.1 api.acme.test\r\n::1 api.acme.test\r\n127.0.0.1 web.acme-2.localhost\r\n::1 web.acme-2.localhost\r\n# devlanes end\r\n"
            ),
            "{content}"
        );
        let again = handle(
            &apply(Some("test"), "api.acme.test\r\nweb.acme-2.localhost\r\n"),
            &target,
        );
        assert_eq!(again, Response::ok("unchanged names=2\n"));
        let status = handle(&Request::Status, &target);
        assert_eq!(status.code, exit::OK);
        assert!(
            status.message.starts_with("# devlanes begin\r\n"),
            "{status:?}"
        );
        assert_eq!(
            handle(&Request::Clear, &target),
            Response::ok("changed names=0\n")
        );
        assert_eq!(fs::read_to_string(&target.hosts).expect("read"), STOCK);
        assert_eq!(handle(&Request::Status, &target), Response::ok(""));
        assert_eq!(
            handle(&Request::Clear, &target),
            Response::ok("unchanged names=0\n")
        );
    }

    #[test]
    fn invalid_names_and_tlds_never_touch_the_file() {
        let (_dir, target) = target();
        for (request, needle) in [
            (apply(None, "example.com\n"), "not under an allowed suffix"),
            (apply(None, "*.acme.localhost\n"), "wildcards"),
            (apply(Some("com"), "a.com\n"), "the TLD must be"),
            (apply(None, "a.localhost\n\u{ff}\n"), "line 2"),
        ] {
            let response = handle(&request, &target);
            assert_eq!(response.code, exit::INVALID_NAMES, "{response:?}");
            assert!(response.message.contains(needle), "{response:?}");
        }
        let too_many: String = (0..=names::MAX_NAMES)
            .map(|index| format!("n{index}.localhost\n"))
            .collect();
        assert_eq!(
            handle(&apply(None, &too_many), &target).code,
            exit::INVALID_NAMES
        );
        assert_eq!(fs::read_to_string(&target.hosts).expect("read"), STOCK);
    }

    #[test]
    fn broken_markers_and_missing_files_are_refused() {
        let (_dir, target) = target();
        fs::write(&target.hosts, "# devlanes begin\r\n").expect("broken");
        let response = handle(&apply(None, "a.localhost\n"), &target);
        assert_eq!(response.code, exit::REFUSED_FILE, "{response:?}");
        assert_eq!(handle(&Request::Status, &target).code, exit::REFUSED_FILE);
        fs::remove_file(&target.hosts).expect("remove");
        assert_eq!(handle(&Request::Clear, &target).code, exit::IO);
        let directory = Target {
            hosts: target.hosts.parent().expect("parent").to_owned(),
            flush: false,
        };
        assert_eq!(
            handle(&Request::Status, &directory).code,
            exit::REFUSED_FILE
        );
    }

    #[test]
    fn version_reports_the_pipe_protocol() {
        let (_dir, target) = target();
        let response = handle(&Request::Version, &target);
        assert_eq!(response.code, exit::OK);
        assert!(
            response
                .message
                .contains(&format!("protocol={PIPE_PROTOCOL} ")),
            "{response:?}"
        );
    }
}

//! Windows: the daemon's side of the `devlanes-hosts` service's pipe.
//!
//! Before anything is sent, the pipe must be owned by the expected account
//! (SYSTEM for the real service), and when the server process can be inspected,
//! run as it: a pipe name taken by another program is refused, never fed names.
//! Responses carry the helper's exit codes and map to the same [`HostsError`]s
//! as the sudo client, plus `service_missing`, `service_denied` and
//! `service_untrusted`.

use std::ffi::OsString;
use std::io;
use std::time::Duration;

use devlanes_hosts_helper::exit;
use devlanes_hosts_helper::protocol::{
    self, DENIED, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, PIPE_NAME, PIPE_PROTOCOL, Request,
    Response,
};
use devlanes_sys::windows::pipe::{self as sys_pipe, ERROR_PIPE_BUSY};
use devlanes_sys::windows::token::{self, Sid};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::windows::named_pipe::NamedPipeClient;

use crate::client::{Applied, HelperVersion, HostsError, parse_applied};

/// Pause between attempts while every pipe instance is busy.
const BUSY_RETRY: Duration = Duration::from_millis(25);

/// A client of the hosts service. Cheap to clone; one connection per request.
#[derive(Debug, Clone)]
pub struct PipeHostsClient {
    pipe_name: OsString,
    server_owner: Sid,
}

impl PipeHostsClient {
    /// The real service: `\\.\pipe\devlanes-hosts`, served by SYSTEM.
    pub fn system() -> Self {
        Self::new(PIPE_NAME, Sid::local_system())
    }

    /// A client of `pipe_name` that only talks to a pipe owned by (and, when that
    /// can be read, served by a process of) `server_owner`. Tests pass their own
    /// pipe and account.
    pub fn new(pipe_name: impl Into<OsString>, server_owner: Sid) -> Self {
        Self {
            pipe_name: pipe_name.into(),
            server_owner,
        }
    }

    /// The pipe name, for messages.
    fn pipe(&self) -> String {
        self.pipe_name.to_string_lossy().into_owned()
    }

    /// Sends `request` and reads the response, all within `timeout`.
    pub async fn request(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<Response, HostsError> {
        tokio::time::timeout(timeout, self.exchange(request))
            .await
            .unwrap_or(Err(HostsError::Timeout(timeout)))
    }

    /// Connects, checks the server, sends one frame, reads one frame.
    async fn exchange(&self, request: &Request) -> Result<Response, HostsError> {
        let mut client = self.connect().await?;
        self.check_server(&client)?;
        let framed = protocol::frame(&request.encode(), MAX_REQUEST_BYTES)
            .map_err(|error| failed(error.to_string()))?;
        client
            .write_all(&framed)
            .await
            .map_err(|error| failed(format!("sending the request: {error}")))?;
        let mut prefix = [0; 4];
        client
            .read_exact(&mut prefix)
            .await
            .map_err(|error| failed(format!("reading the response: {error}")))?;
        let length = protocol::frame_length(prefix, MAX_RESPONSE_BYTES)
            .map_err(|error| failed(error.to_string()))?;
        let mut payload = vec![0; length];
        client
            .read_exact(&mut payload)
            .await
            .map_err(|error| failed(format!("reading the response: {error}")))?;
        Response::decode(&payload).map_err(|error| failed(error.to_string()))
    }

    /// Opens the pipe, waiting while every instance is busy.
    async fn connect(&self) -> Result<NamedPipeClient, HostsError> {
        loop {
            match sys_pipe::open_client(&self.pipe_name) {
                Ok(client) => return Ok(client),
                Err(error) if error.raw_os_error() == Some(ERROR_PIPE_BUSY) => {
                    tokio::time::sleep(BUSY_RETRY).await;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    return Err(HostsError::ServiceMissing { pipe: self.pipe() });
                }
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(HostsError::ServiceDenied {
                        message: format!("{} refuses this account: {error}", self.pipe()),
                    });
                }
                Err(error) => return Err(failed(format!("opening {}: {error}", self.pipe()))),
            }
        }
    }

    /// Refuses a pipe that is not owned by the expected account, or whose server
    /// process runs as another one. Only the account itself (or an
    /// administrator) can create an object it owns, so the owner check alone
    /// stops a squatting user process. The process check is a second look; an
    /// unelevated daemon usually may not read a SYSTEM process's token, and that
    /// is fine.
    fn check_server(&self, client: &NamedPipeClient) -> Result<(), HostsError> {
        let expected = self
            .server_owner
            .to_sddl()
            .map_err(|error| failed(error.to_string()))?;
        let owner = sys_pipe::server_owner(client)
            .map_err(|error| failed(format!("reading the pipe's owner: {error}")))?;
        if owner.as_deref() != Some(expected.as_str()) {
            return Err(HostsError::ServiceUntrusted {
                owner: owner.unwrap_or_else(|| "nobody".into()),
            });
        }
        if let Ok(user) = sys_pipe::server_process_id(client).and_then(token::process_user)
            && user != self.server_owner
        {
            return Err(HostsError::ServiceUntrusted {
                owner: user.to_sddl().unwrap_or_else(|_| "another account".into()),
            });
        }
        Ok(())
    }

    /// `apply` or `clear`: the helper's `changed names=<n>` line.
    pub(crate) async fn applied(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<Applied, HostsError> {
        let response = success(self.request(request, timeout).await?)?;
        parse_applied(&response).ok_or_else(|| HostsError::HelperFailed {
            code: Some(0),
            message: format!("unexpected output {:?}", response.trim()),
        })
    }

    /// `status`: the block.
    pub(crate) async fn status(&self, timeout: Duration) -> Result<String, HostsError> {
        success(self.request(&Request::Status, timeout).await?)
    }

    /// `version`, with the pipe protocol checked.
    pub(crate) async fn version(&self, timeout: Duration) -> Result<HelperVersion, HostsError> {
        let message = success(self.request(&Request::Version, timeout).await?)?;
        let found = message.trim();
        let line = protocol::parse_version_line(found).ok_or_else(|| HostsError::Outdated {
            found: if found.is_empty() {
                "unknown".into()
            } else {
                found.into()
            },
        })?;
        if line.protocol != PIPE_PROTOCOL {
            return Err(HostsError::Outdated {
                found: line.protocol.to_string(),
            });
        }
        Ok(HelperVersion {
            version: line.version,
            protocol: line.protocol,
            build: line.build,
        })
    }
}

/// A transport failure.
fn failed(message: String) -> HostsError {
    HostsError::HelperFailed {
        code: None,
        message,
    }
}

/// The message of a successful response, or the error its code stands for.
fn success(response: Response) -> Result<String, HostsError> {
    let message = response.message;
    match response.code {
        exit::OK => Ok(message),
        DENIED => Err(HostsError::ServiceDenied { message }),
        exit::INVALID_NAMES => Err(HostsError::InvalidNames { message }),
        exit::REFUSED_FILE => Err(HostsError::HostsFileRefused { message }),
        code => Err(HostsError::HelperFailed {
            code: Some(i32::from(code)),
            message,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_codes_map_like_the_helper_exits() {
        assert_eq!(success(Response::ok("x")), Ok("x".into()));
        for (code, expected) in [
            (DENIED, "service_denied"),
            (exit::INVALID_NAMES, "hosts_invalid_names"),
            (exit::REFUSED_FILE, "hosts_file_refused"),
            (exit::IO, "helper_failed"),
            (exit::USAGE, "helper_failed"),
        ] {
            assert_eq!(
                success(Response::failure(code, "m"))
                    .expect_err("failure")
                    .code(),
                expected
            );
        }
    }

    /// No pipe of that name: the service is missing, nothing else is tried.
    #[tokio::test]
    async fn a_missing_pipe_is_service_missing() {
        let client = PipeHostsClient::new(
            format!(r"\\.\pipe\devlanes-hosts-missing-{}", std::process::id()),
            Sid::local_system(),
        );
        let error = client
            .version(Duration::from_secs(5))
            .await
            .expect_err("missing");
        assert_eq!(error.code(), "service_missing");
    }
}

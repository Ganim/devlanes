//! The request and response format of the Windows hosts service's named pipe.
//!
//! Windows has no NOPASSWD sudo, so there the helper's job is done by
//! `devlanes-hosts-service`, a service that answers the daemon over
//! `\\.\pipe\devlanes-hosts`. It speaks the helper's command line as messages:
//! the same commands, limits and exit-code meanings, so both sides share this
//! module and [`crate::names`] instead of a second contract.
//!
//! Every message is a frame: a little-endian `u32` byte count, then that many
//! bytes. A request is one header line (`version`, `status`, `clear`, `apply` or
//! `apply --tld <tld>`), a `\n`, and for `apply` the names exactly as the helper
//! reads them on stdin. A response is the exit code in decimal, a `\n`, and the
//! message: what the helper prints on stdout on success (`changed names=2`, the
//! block, the version line), its error text otherwise. One request per connection.

use std::fmt;

use crate::exit;
use crate::names::MAX_INPUT_BYTES;

/// The pipe the service listens on.
pub const PIPE_NAME: &str = r"\\.\pipe\devlanes-hosts";

/// The service's name in the service manager.
pub const SERVICE_NAME: &str = "devlanes-hosts";

/// The service binary, installed under `%ProgramFiles%\devlanes`.
pub const SERVICE_BINARY: &str = "devlanes-hosts-service.exe";

/// Version of the pipe contract. Bump it when requests or responses change in a
/// way an older daemon or service would misread; the daemon compares it with the
/// `version` reply and asks for a reinstall on mismatch.
pub const PIPE_PROTOCOL: u32 = 1;

/// Longest request header line, without its `\n`.
pub const MAX_HEADER_BYTES: usize = 512;

/// Largest request: a full header plus the helper's input limit.
pub const MAX_REQUEST_BYTES: usize = MAX_HEADER_BYTES + 1 + MAX_INPUT_BYTES;

/// Largest response. `status` returns the block, which at [`crate::names::MAX_NAMES`]
/// names is about 2.2 MB; the rest leaves room for hand-edited blocks.
pub const MAX_RESPONSE_BYTES: usize = 8 << 20;

/// A malformed or oversized message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    /// A frame announces more bytes than the receiver accepts.
    TooLarge {
        /// Announced length.
        length: usize,
        /// The receiver's limit.
        limit: usize,
    },
    /// The bytes do not follow the format; the text says what is wrong.
    Malformed(&'static str),
}

impl fmt::Display for ProtocolError {
    /// One-line description for the caller.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { length, limit } => {
                write!(f, "message of {length} bytes is larger than {limit}")
            }
            Self::Malformed(reason) => write!(f, "malformed message: {reason}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

/// One command for the service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Version, protocol and build profile.
    Version,
    /// The current block.
    Status,
    /// Removes the block.
    Clear,
    /// Sets the block to `names`, allowing `.localhost` plus `tld`.
    Apply {
        /// Extra allowed suffix, validated by the service like `--tld`.
        tld: Option<String>,
        /// One name per line, the helper's stdin format; validated by the service.
        names: Vec<u8>,
    },
}

impl Request {
    /// The frame payload: header line, `\n`, body.
    pub fn encode(&self) -> Vec<u8> {
        let (header, body): (String, &[u8]) = match self {
            Self::Version => ("version".into(), &[]),
            Self::Status => ("status".into(), &[]),
            Self::Clear => ("clear".into(), &[]),
            Self::Apply { tld: None, names } => ("apply".into(), names),
            Self::Apply {
                tld: Some(tld),
                names,
            } => (format!("apply --tld {tld}"), names),
        };
        let mut out = Vec::with_capacity(header.len() + 1 + body.len());
        out.extend_from_slice(header.as_bytes());
        out.push(b'\n');
        out.extend_from_slice(body);
        out
    }

    /// Parses a frame payload. Only `apply` may carry a body; the names in it
    /// are left for the caller to validate with [`crate::names::parse_names`].
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() > MAX_REQUEST_BYTES {
            return Err(ProtocolError::TooLarge {
                length: payload.len(),
                limit: MAX_REQUEST_BYTES,
            });
        }
        let (header, body) = match payload.iter().position(|&byte| byte == b'\n') {
            Some(newline) => (&payload[..newline], &payload[newline + 1..]),
            None => (payload, &[][..]),
        };
        if header.len() > MAX_HEADER_BYTES {
            return Err(ProtocolError::Malformed("the header line is too long"));
        }
        let header = std::str::from_utf8(header)
            .map_err(|_| ProtocolError::Malformed("the header is not UTF-8"))?;
        let words: Vec<&str> = header.split(' ').collect();
        let request = match words.as_slice() {
            ["version"] => Self::Version,
            ["status"] => Self::Status,
            ["clear"] => Self::Clear,
            ["apply"] => Self::Apply {
                tld: None,
                names: body.to_vec(),
            },
            ["apply", "--tld", tld] if !tld.is_empty() => Self::Apply {
                tld: Some((*tld).to_owned()),
                names: body.to_vec(),
            },
            _ => return Err(ProtocolError::Malformed("unknown command")),
        };
        if !body.is_empty() && !matches!(request, Self::Apply { .. }) {
            return Err(ProtocolError::Malformed("only apply takes names"));
        }
        Ok(request)
    }
}

/// The service's answer: the helper's exit code and its output or error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    /// [`exit`] codes.
    pub code: u8,
    /// stdout of the helper on success, its error message otherwise.
    pub message: String,
}

impl Response {
    /// Success with the helper's output.
    pub fn ok(message: impl Into<String>) -> Self {
        Self {
            code: exit::OK,
            message: message.into(),
        }
    }

    /// A failure with `code`.
    pub fn failure(code: u8, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// The frame payload: code, `\n`, message.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.code.to_string().into_bytes();
        out.push(b'\n');
        out.extend_from_slice(self.message.as_bytes());
        out
    }

    /// Parses a frame payload.
    pub fn decode(payload: &[u8]) -> Result<Self, ProtocolError> {
        if payload.len() > MAX_RESPONSE_BYTES {
            return Err(ProtocolError::TooLarge {
                length: payload.len(),
                limit: MAX_RESPONSE_BYTES,
            });
        }
        let newline = payload
            .iter()
            .position(|&byte| byte == b'\n')
            .ok_or(ProtocolError::Malformed("no code line"))?;
        let code = std::str::from_utf8(&payload[..newline])
            .ok()
            .filter(|code| !code.is_empty() && code.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|code| code.parse().ok())
            .ok_or(ProtocolError::Malformed(
                "the code is not a number below 256",
            ))?;
        let message = String::from_utf8(payload[newline + 1..].to_vec())
            .map_err(|_| ProtocolError::Malformed("the message is not UTF-8"))?;
        Ok(Self { code, message })
    }
}

/// `payload` with its length prefix; refused above `limit`, so a sender never
/// writes what the receiver would drop.
pub fn frame(payload: &[u8], limit: usize) -> Result<Vec<u8>, ProtocolError> {
    let too_large = || ProtocolError::TooLarge {
        length: payload.len(),
        limit,
    };
    if payload.len() > limit {
        return Err(too_large());
    }
    let length = u32::try_from(payload.len()).map_err(|_| too_large())?;
    let mut out = Vec::with_capacity(4 + payload.len());
    out.extend_from_slice(&length.to_le_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// The payload length a frame's prefix announces, refused above `limit` before
/// anything is allocated.
pub fn frame_length(prefix: [u8; 4], limit: usize) -> Result<usize, ProtocolError> {
    let length = u32::from_le_bytes(prefix) as usize;
    if length > limit {
        return Err(ProtocolError::TooLarge { length, limit });
    }
    Ok(length)
}

/// The `version` reply: `devlanes-hosts-service <version> protocol=<n> build=<profile>`.
pub fn version_line(version: &str) -> String {
    format!(
        "devlanes-hosts-service {version} protocol={PIPE_PROTOCOL} build={}",
        crate::BUILD
    )
}

/// A parsed [`version_line`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionLine {
    /// Crate version.
    pub version: String,
    /// [`PIPE_PROTOCOL`] of the service that printed it.
    pub protocol: u32,
    /// `release` or `debug`.
    pub build: String,
}

/// Parses a [`version_line`]; `None` for anything else.
pub fn parse_version_line(line: &str) -> Option<VersionLine> {
    let mut parts = line.split_whitespace();
    if parts.next()? != "devlanes-hosts-service" {
        return None;
    }
    let version = parts.next()?.to_owned();
    let protocol = parts.next()?.strip_prefix("protocol=")?.parse().ok()?;
    let build = parts.next()?.strip_prefix("build=")?.to_owned();
    if parts.next().is_some() {
        return None;
    }
    Some(VersionLine {
        version,
        protocol,
        build,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for request in [
            Request::Version,
            Request::Status,
            Request::Clear,
            Request::Apply {
                tld: None,
                names: b"a.localhost\nb.localhost\n".to_vec(),
            },
            Request::Apply {
                tld: Some("acme.test".into()),
                names: b"api.acme.test\r\n".to_vec(),
            },
            Request::Apply {
                tld: None,
                names: Vec::new(),
            },
        ] {
            assert_eq!(Request::decode(&request.encode()), Ok(request.clone()));
        }
        assert_eq!(Request::Version.encode(), b"version\n");
        assert_eq!(
            Request::Apply {
                tld: Some("test".into()),
                names: b"a.test\n".to_vec()
            }
            .encode(),
            b"apply --tld test\na.test\n"
        );
        // A header without its line break is accepted.
        assert_eq!(Request::decode(b"clear"), Ok(Request::Clear));
    }

    #[test]
    fn bad_requests_are_refused() {
        for payload in [
            &b""[..],
            b"\n",
            b"remove\n",
            b"apply  --tld test\n",
            b"apply --tld\n",
            b"apply --tld \n",
            b"apply --tld a b\n",
            b"apply --hosts-file C:\\x\n",
            b"Version\n",
            b"clear\nextra.localhost\n",
            b"status\n\n",
            b"\xff\n",
        ] {
            assert!(
                matches!(Request::decode(payload), Err(ProtocolError::Malformed(_))),
                "{payload:?}"
            );
        }
        let long = format!("apply --tld {}\n", "a".repeat(MAX_HEADER_BYTES));
        assert_eq!(
            Request::decode(long.as_bytes()),
            Err(ProtocolError::Malformed("the header line is too long"))
        );
    }

    #[test]
    fn the_request_limit_covers_a_full_apply() {
        let mut payload = b"apply --tld acme.test\n".to_vec();
        payload.resize(payload.len() + MAX_INPUT_BYTES, b'\n');
        assert!(payload.len() <= MAX_REQUEST_BYTES);
        assert!(matches!(
            Request::decode(&payload),
            Ok(Request::Apply { .. })
        ));
        let mut over = vec![b'\n'; MAX_REQUEST_BYTES + 1];
        over[..6].copy_from_slice(b"apply\n");
        assert_eq!(
            Request::decode(&over),
            Err(ProtocolError::TooLarge {
                length: MAX_REQUEST_BYTES + 1,
                limit: MAX_REQUEST_BYTES
            })
        );
    }

    #[test]
    fn responses_round_trip() {
        for response in [
            Response::ok("changed names=2\n"),
            Response::ok(""),
            Response::failure(exit::INVALID_NAMES, "line 1: invalid name \"x\""),
            Response::failure(exit::IO, "refused"),
            Response::failure(255, "multi\nline"),
        ] {
            assert_eq!(Response::decode(&response.encode()), Ok(response.clone()));
        }
        assert_eq!(Response::failure(3, "bad").encode(), b"3\nbad");
        for payload in [
            &b""[..],
            b"0",
            b"x\nmessage",
            b"256\n",
            b"-1\n",
            b"\n",
            b"0\n\xff",
        ] {
            assert!(Response::decode(payload).is_err(), "{payload:?}");
        }
    }

    #[test]
    fn frames_announce_their_length() {
        let framed = frame(b"version\n", MAX_REQUEST_BYTES).expect("frame");
        assert_eq!(&framed[..4], &[8, 0, 0, 0]);
        assert_eq!(&framed[4..], b"version\n");
        assert_eq!(
            frame(b"version\n", 4),
            Err(ProtocolError::TooLarge {
                length: 8,
                limit: 4
            })
        );
        assert_eq!(frame_length([8, 0, 0, 0], MAX_REQUEST_BYTES), Ok(8));
        assert_eq!(
            frame_length([0xff, 0xff, 0xff, 0xff], MAX_REQUEST_BYTES),
            Err(ProtocolError::TooLarge {
                length: u32::MAX as usize,
                limit: MAX_REQUEST_BYTES
            })
        );
    }

    #[test]
    fn the_version_line_names_the_pipe_protocol() {
        let line = version_line("1.2.3");
        assert!(
            line.starts_with(&format!(
                "devlanes-hosts-service 1.2.3 protocol={PIPE_PROTOCOL} build="
            )),
            "{line}"
        );
        assert_eq!(
            parse_version_line(&format!("{line}\n")),
            Some(VersionLine {
                version: "1.2.3".into(),
                protocol: PIPE_PROTOCOL,
                build: crate::BUILD.into(),
            })
        );
        for other in [
            "",
            "devlanes-hosts 1.2.3 protocol=1 build=release",
            "devlanes-hosts-service 1.2.3 protocol=x build=release",
            "devlanes-hosts-service 1.2.3 build=release protocol=1",
            "devlanes-hosts-service 1.2.3 protocol=1 build=release extra",
        ] {
            assert_eq!(parse_version_line(other), None, "{other:?}");
        }
    }
}

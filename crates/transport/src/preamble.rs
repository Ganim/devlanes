//! The line `devlanes tunnel --stdio` writes before any mux byte.
//!
//! A login shell or profile file on the box may print to stdout before the
//! tunnel starts (Tailscale SSH runs `su -l`, for example). The client drops
//! that output up to the preamble line, so the mux only sees its own bytes.

use std::io;
use std::time::Duration;

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::{Instant, timeout_at};

use crate::wire::{PROTOCOL_VERSION, Prefixed};

/// The line the box's tunnel writes first, naming its protocol revision.
pub const TUNNEL_PREAMBLE: &str = "devlanes-tunnel 4\n";

/// What a preamble line starts with; the protocol revision follows.
const MARKER: &[u8] = b"devlanes-tunnel ";

/// Most output dropped before the preamble.
pub const PREAMBLE_LIMIT: usize = 64 * 1024;

/// Room for the preamble line itself on top of [`PREAMBLE_LIMIT`].
const LINE_ROOM: usize = 32;

/// Bytes of dropped output an error message shows, from the end.
const SHOWN: usize = 4096;

/// A pipe positioned right after a preamble line naming [`PROTOCOL_VERSION`].
#[derive(Debug)]
pub struct Preamble<S> {
    /// The pipe; bytes read past the preamble come first.
    pub stream: Prefixed<S>,
    /// Output dropped before the preamble, such as a login banner.
    pub header: Vec<u8>,
}

/// The pipe did not deliver a preamble line.
#[derive(Debug, thiserror::Error)]
pub enum PreambleError {
    /// The pipe ended first.
    #[error("the box closed the connection before `devlanes tunnel --stdio` started{}", shown(.output))]
    Ended {
        /// Everything the box printed.
        output: Vec<u8>,
    },
    /// The box printed more than [`PREAMBLE_LIMIT`] bytes without one.
    #[error(
        "the box printed more than {} KiB before `devlanes tunnel --stdio` started; check what \
         the login shell or its profile files print{}",
        PREAMBLE_LIMIT / 1024,
        shown(.output)
    )]
    TooLong {
        /// Everything the box printed.
        output: Vec<u8>,
    },
    /// Nothing arrived in time. The box may run a release from before the
    /// preamble (protocol 4), whose tunnel waits for the client to speak first.
    #[error(
        "`devlanes tunnel --stdio` did not answer within {}s; the box may run an older \
         devlanes release (protocol below 4), so install the same release on both machines{}",
        .waited.as_secs(),
        shown(.output)
    )]
    Silent {
        /// How long the client waited.
        waited: Duration,
        /// Everything the box printed.
        output: Vec<u8>,
    },
    /// The preamble names another protocol revision than [`PROTOCOL_VERSION`].
    #[error(
        "the box runs another devlanes release (protocol {found}) but this client speaks \
         protocol {PROTOCOL_VERSION}; install the same release on both machines"
    )]
    Protocol {
        /// The box's revision.
        found: u32,
        /// What the box printed before the preamble.
        output: Vec<u8>,
    },
    /// Reading the pipe failed, for example because the tunnel exited with an error.
    #[error("{source}{}", shown(.output))]
    Io {
        /// The read error.
        #[source]
        source: io::Error,
        /// What the box printed before it.
        output: Vec<u8>,
    },
}

impl PreambleError {
    /// What the box printed before the failure.
    pub fn output(&self) -> &[u8] {
        match self {
            Self::Ended { output }
            | Self::TooLong { output }
            | Self::Silent { output, .. }
            | Self::Protocol { output, .. }
            | Self::Io { output, .. } => output,
        }
    }

    /// Whether reconnecting without user action is pointless. `first` is set
    /// until the transport's first successful open: before it, silence or a
    /// flood of output comes from how the box is set up (an older release, a
    /// chatty profile file); after it, they may pass, like a box under load.
    pub fn is_permanent(&self, first: bool) -> bool {
        match self {
            Self::Protocol { .. } => true,
            Self::Silent { .. } | Self::TooLong { .. } => first,
            Self::Ended { .. } | Self::Io { .. } => false,
        }
    }
}

/// The dropped output for an error message: its last [`SHOWN`] bytes, or nothing.
fn shown(output: &[u8]) -> String {
    let start = output.len().saturating_sub(SHOWN);
    let text = String::from_utf8_lossy(&output[start..]);
    let text = text.trim();
    if text.is_empty() {
        String::new()
    } else {
        format!("; the box printed: {text}")
    }
}

/// Reads `io` up to and including the preamble line, giving up after `wait`.
///
/// Output before the line is dropped and logged at debug level; a line that
/// ends in the marker (output without a final newline glued to it) counts.
/// Bytes already read past the line stay in [`Preamble::stream`]. A pipe that
/// ends, fails, stays silent or exceeds [`PREAMBLE_LIMIT`] first is an error
/// carrying what was dropped, and so is a preamble naming another protocol
/// revision than [`PROTOCOL_VERSION`] ([`PreambleError::Protocol`]).
pub async fn read_preamble<S: AsyncRead + Unpin>(
    mut io: S,
    wait: Duration,
) -> Result<Preamble<S>, PreambleError> {
    let deadline = Instant::now() + wait;
    let mut buffer: Vec<u8> = Vec::new();
    let mut line_start = 0;
    let mut chunk = [0u8; 8192];
    loop {
        while let Some(end) = buffer[line_start..].iter().position(|byte| *byte == b'\n') {
            let line_end = line_start + end;
            if let Some((start, protocol)) = marker(&buffer[line_start..line_end]) {
                let header = buffer[..line_start + start].to_vec();
                if protocol != PROTOCOL_VERSION {
                    return Err(PreambleError::Protocol {
                        found: protocol,
                        output: header,
                    });
                }
                if !header.is_empty() {
                    tracing::debug!(
                        dropped = %String::from_utf8_lossy(&header),
                        "dropped box output before the tunnel preamble"
                    );
                }
                let rest = Bytes::copy_from_slice(&buffer[line_end + 1..]);
                return Ok(Preamble {
                    stream: Prefixed::new(rest, io),
                    header,
                });
            }
            line_start = line_end + 1;
        }
        if buffer.len() > PREAMBLE_LIMIT + LINE_ROOM {
            return Err(PreambleError::TooLong { output: buffer });
        }
        let read = match timeout_at(deadline, io.read(&mut chunk)).await {
            Err(_) => {
                return Err(PreambleError::Silent {
                    waited: wait,
                    output: buffer,
                });
            }
            Ok(Err(source)) => {
                return Err(PreambleError::Io {
                    source,
                    output: buffer,
                });
            }
            Ok(Ok(0)) => return Err(PreambleError::Ended { output: buffer }),
            Ok(Ok(read)) => read,
        };
        buffer.extend_from_slice(&chunk[..read]);
    }
}

/// Where the preamble starts in `line` (without its newline) and the revision
/// it names, when the line ends in one.
fn marker(line: &[u8]) -> Option<(usize, u32)> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let start = line
        .windows(MARKER.len())
        .rposition(|window| window == MARKER)?;
    let digits = &line[start + MARKER.len()..];
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let protocol = std::str::from_utf8(digits).ok()?.parse().ok()?;
    Some((start, protocol))
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    const WAIT: Duration = Duration::from_secs(5);

    #[test]
    fn the_preamble_names_this_protocol() {
        assert_eq!(
            TUNNEL_PREAMBLE,
            format!("devlanes-tunnel {PROTOCOL_VERSION}\n")
        );
    }

    /// Login output is dropped, and mux bytes that arrived with the preamble
    /// stay readable.
    #[tokio::test]
    async fn output_before_the_preamble_is_dropped() -> TestResult {
        let input = b"Last login: today\r\nwelcome\nmotd without newline devlanes-tunnel 4\r\nMUX";
        let mut found = read_preamble(&input[..], WAIT).await?;
        assert_eq!(
            found.header,
            b"Last login: today\r\nwelcome\nmotd without newline ".to_vec()
        );
        let mut rest = Vec::new();
        found.stream.read_to_end(&mut rest).await?;
        assert_eq!(rest, b"MUX");
        Ok(())
    }

    /// A preamble split over several reads, and bytes that follow it later.
    #[tokio::test]
    async fn a_split_preamble_is_found() -> TestResult {
        let (mut box_side, client_side) = tokio::io::duplex(64);
        let writer = tokio::spawn(async move {
            for part in [&b"devlanes-tun"[..], b"nel 4", b"\n", b"after"] {
                box_side.write_all(part).await?;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, io::Error>(box_side)
        });
        let mut found = read_preamble(client_side, WAIT).await?;
        assert!(found.header.is_empty());
        let box_side = writer.await??;
        drop(box_side);
        let mut rest = Vec::new();
        found.stream.read_to_end(&mut rest).await?;
        assert_eq!(rest, b"after");
        Ok(())
    }

    /// Output without a preamble fails with that output in the message.
    #[tokio::test]
    async fn output_without_a_preamble_fails_with_it() {
        let input = b"This account is currently not available.\n";
        let error = read_preamble(&input[..], WAIT)
            .await
            .expect_err("no preamble");
        assert!(matches!(error, PreambleError::Ended { .. }), "{error:?}");
        let message = error.to_string();
        assert!(
            message.contains("This account is currently not available."),
            "{message}"
        );
        assert_eq!(error.output(), input);
    }

    /// Lines that only look like the preamble are output too.
    #[tokio::test]
    async fn near_misses_are_not_the_preamble() {
        let input = b"devlanes-tunnel \ndevlanes-tunnel 4x\ndevlanes-tunnel 4";
        let error = read_preamble(&input[..], WAIT)
            .await
            .expect_err("no complete preamble line");
        assert_eq!(error.output(), input);
    }

    #[tokio::test]
    async fn too_much_output_fails() {
        let input = vec![b'x'; PREAMBLE_LIMIT + 1024];
        let error = read_preamble(&input[..], WAIT)
            .await
            .expect_err("over the limit");
        assert!(matches!(error, PreambleError::TooLong { .. }), "{error:?}");
        assert!(error.to_string().contains("xxxx"), "the output is shown");
    }

    /// A pipe that stays open and quiet fails after the wait, naming an older
    /// release as the likely cause.
    #[tokio::test]
    async fn a_silent_box_fails_after_the_wait() {
        let (_box_side, client_side) = tokio::io::duplex(64);
        let error = read_preamble(client_side, Duration::from_millis(50))
            .await
            .expect_err("silence");
        assert!(matches!(error, PreambleError::Silent { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.contains("older devlanes release"), "{message}");
        assert!(message.contains("protocol below 4"), "{message}");
        assert!(
            message.contains("install the same release on both machines"),
            "{message}"
        );
    }

    /// A box whose tunnel speaks another revision fails before the mux starts,
    /// naming both revisions, and retrying cannot help.
    #[tokio::test]
    async fn another_protocol_fails_for_good() {
        let found = PROTOCOL_VERSION + 1;
        let input = format!("motd\ndevlanes-tunnel {found}\nMUX");
        let error = read_preamble(input.as_bytes(), WAIT)
            .await
            .expect_err("another protocol");
        assert!(
            matches!(error, PreambleError::Protocol { found: f, .. } if f == found),
            "{error:?}"
        );
        assert_eq!(error.output(), b"motd\n");
        let message = error.to_string();
        assert!(
            message.contains(&format!(
                "(protocol {found}) but this client speaks protocol {PROTOCOL_VERSION}"
            )),
            "{message}"
        );
        assert!(
            message.contains("install the same release on both machines"),
            "{message}"
        );
        assert!(error.is_permanent(true));
        assert!(error.is_permanent(false));
    }

    /// Silence and a flood of output stop the link only before the first
    /// open; a pipe that ends or fails always retries.
    #[test]
    fn permanence_depends_on_the_first_open() {
        let silent = PreambleError::Silent {
            waited: WAIT,
            output: Vec::new(),
        };
        assert!(silent.is_permanent(true));
        assert!(!silent.is_permanent(false));
        let flood = PreambleError::TooLong { output: Vec::new() };
        assert!(flood.is_permanent(true));
        assert!(!flood.is_permanent(false));
        let ended = PreambleError::Ended { output: Vec::new() };
        assert!(!ended.is_permanent(true));
        let failed = PreambleError::Io {
            source: io::Error::other("gone"),
            output: Vec::new(),
        };
        assert!(!failed.is_permanent(true));
    }

    /// A failing pipe keeps its own error and adds what was printed.
    #[tokio::test]
    async fn read_errors_keep_their_source() {
        let failing = tokio_test_reader(b"banner\n", "tunnel exited with 75");
        let Err(error) = read_preamble(failing, WAIT).await else {
            panic!("a failing pipe had a preamble");
        };
        assert!(matches!(error, PreambleError::Io { .. }), "{error:?}");
        let message = error.to_string();
        assert!(message.starts_with("tunnel exited with 75"), "{message}");
        assert!(message.contains("banner"), "{message}");
    }

    /// A reader that yields `first`, then fails with `message`.
    fn tokio_test_reader(first: &'static [u8], message: &'static str) -> impl AsyncRead + Unpin {
        first.chain(Failing(message))
    }

    /// Every read fails with the given message.
    struct Failing(&'static str);

    impl AsyncRead for Failing {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                self.0,
            )))
        }
    }
}

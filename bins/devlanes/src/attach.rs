//! Raw terminal attach (`devlanes attach`). Detaching never stops the unit.

use std::io::{Read, Write};

use bytes::Bytes;
use devlanes_client::{Client, Paths};
use devlanes_ipc::Frame;
use devlanes_proto::{AttachParams, AttachResult, Control, Method, Outcome, Request, ResizeParams};
use tokio::sync::mpsc;

#[cfg(windows)]
use devlanes_sys::windows::console::ModeGuard;

/// Detach chord, as in tmux/screen: Ctrl-A then `d`.
const PREFIX: u8 = 0x01;

/// Restores the terminal mode even when attach ends through an error path.
#[cfg(unix)]
struct RawGuard(bool);

#[cfg(unix)]
impl RawGuard {
    /// Only an interactive terminal is switched to raw mode.
    fn enable(interactive: bool) -> std::io::Result<Self> {
        if interactive {
            crossterm::terminal::enable_raw_mode()?;
        }
        Ok(Self(interactive))
    }
}

#[cfg(unix)]
impl Drop for RawGuard {
    /// Leaving raw mode enabled would break the user's shell.
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Console modes for attach; dropping the guards restores the exact previous
/// modes, also when attach ends through an error path.
#[cfg(windows)]
struct RawGuard {
    _input: Option<ModeGuard<std::io::Stdin>>,
    output: Option<ModeGuard<std::io::Stdout>>,
}

#[cfg(windows)]
impl Drop for RawGuard {
    /// Shows the cursor: ConPTY hides it around every repaint, so attach can end
    /// between a hide and its show. Runs before the guards drop, while the
    /// console still interprets VT sequences.
    fn drop(&mut self) {
        if self.output.is_some() {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(b"\x1b[?25h");
            let _ = stdout.flush();
        }
    }
}

#[cfg(windows)]
impl RawGuard {
    /// Only an interactive console is switched to raw mode. crossterm's raw mode
    /// is not used: it leaves VT input off, so arrows and function keys would not
    /// reach the unit as escape sequences.
    fn enable(interactive: bool) -> anyhow::Result<Self> {
        use anyhow::Context;
        use devlanes_sys::windows::console;
        if !interactive {
            return Ok(Self {
                _input: None,
                output: None,
            });
        }
        // No line editing, echo or Ctrl-C handling: Ctrl-C reaches the unit as a
        // byte, as in a Unix raw terminal.
        let input = ModeGuard::change(
            std::io::stdin(),
            console::VIRTUAL_TERMINAL_INPUT,
            console::LINE_INPUT | console::ECHO_INPUT | console::PROCESSED_INPUT,
        )
        .context(
            "attach needs a Windows console for input (Windows Terminal or a console window); \
             mintty pipes are not consoles",
        )?;
        // The unit's pseudoconsole emits VT sequences and explicit carriage returns.
        // Redirected output is not a console and needs no mode.
        let output = ModeGuard::change(
            std::io::stdout(),
            console::VIRTUAL_TERMINAL_PROCESSING | console::DISABLE_NEWLINE_AUTO_RETURN,
            0,
        )
        .ok();
        Ok(Self {
            _input: Some(input),
            output,
        })
    }
}

/// Attaches stdin/stdout to a unit's terminal until detach or unit exit.
pub async fn run(paths: &Paths, request: Request) -> anyhow::Result<u8> {
    let mut client = Client::connect_or_spawn(paths).await?;
    let unit = request
        .params
        .get("unit")
        .and_then(|unit| unit.as_str())
        .unwrap_or_default()
        .to_owned();
    let interactive = std::io::IsTerminal::is_terminal(&std::io::stdin());
    let (columns, rows) = crossterm::terminal::size().unwrap_or((120, 40));
    let params = serde_json::to_value(AttachParams {
        unit,
        interactive,
        columns: Some(columns),
        rows: Some(rows),
    })?;
    let attach: AttachResult = serde_json::from_value(
        client
            .request(Method::Attach, params, request.context.clone())
            .await?,
    )?;
    let _raw = RawGuard::enable(interactive)?;
    let mut stdout = std::io::stdout();
    if interactive {
        let _ = write!(
            stdout,
            "\r\n[attached to {}:{}; C-a d detaches]\r\n",
            attach.lane, attach.unit
        );
    }

    // stdin blocks; a thread keeps it off the runtime and is abandoned at exit.
    let (input_tx, mut input) = mpsc::channel::<Vec<u8>>(64);
    std::thread::spawn(move || read_input(interactive, &input_tx));
    let mut resizes = Resizes::new()?;
    let mut output = OutputFilter::default();
    let mut prefix = false;
    loop {
        tokio::select! {
            frame = client.next_frame() => match frame? {
                Frame::Bytes { stream_id, data } if stream_id == attach.stream_id => {
                    if data.is_empty() {
                        let _ = stdout.write_all(&output.finish());
                        if interactive { let _ = write!(stdout, "\r\n[unit exited]\r\n"); }
                        return Ok(0);
                    }
                    stdout.write_all(&output.filter(&data))?;
                    stdout.flush()?;
                }
                Frame::Control(Control::Response(response)) => {
                    if let Outcome::Error { error } = response.result {
                        tracing::debug!(code = %error.code, "attach side request failed");
                    }
                }
                _ => {}
            },
            Some(bytes) = input.recv() => {
                let mut forward = Vec::with_capacity(bytes.len());
                for byte in bytes {
                    if prefix {
                        prefix = false;
                        if byte == b'd' {
                            if interactive { let _ = write!(stdout, "\r\n[detached]\r\n"); }
                            return Ok(0);
                        }
                        // C-a C-a sends a literal C-a.
                        forward.push(PREFIX);
                        if byte != PREFIX { forward.push(byte); }
                    } else if byte == PREFIX && interactive {
                        prefix = true;
                    } else {
                        forward.push(byte);
                    }
                }
                if !forward.is_empty() {
                    client.send_bytes(attach.stream_id, Bytes::from(forward)).await?;
                }
            },
            size = resizes.next() => {
                if let Some((columns, rows)) = size {
                    let params = serde_json::to_value(ResizeParams { stream_id: attach.stream_id, columns, rows })?;
                    client.begin_request(Method::Resize, params, request.context.clone()).await?;
                }
            }
        }
    }
}

/// Sends what the user types to `input_tx` until stdin ends or attach is gone.
#[cfg(unix)]
fn read_input(_interactive: bool, input_tx: &mpsc::Sender<Vec<u8>>) {
    read_bytes(input_tx);
}

/// Sends what the user types to `input_tx` until stdin ends or attach is gone.
/// A raw console is read as text, because std's console reader ends input at
/// Ctrl-Z, which in raw mode is a key for the unit.
#[cfg(windows)]
fn read_input(interactive: bool, input_tx: &mpsc::Sender<Vec<u8>>) {
    if !interactive {
        read_bytes(input_tx);
        return;
    }
    let stdin = std::io::stdin();
    let mut units = [0_u16; 2048];
    let mut decoder = Utf16Decoder::default();
    loop {
        let read = match devlanes_sys::windows::console::read_input(&stdin, &mut units) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        let bytes = decoder.decode(&units[..read]);
        if !bytes.is_empty() && input_tx.blocking_send(bytes).is_err() {
            break;
        }
    }
}

/// Copies stdin bytes to `input_tx` until end of input or attach is gone.
fn read_bytes(input_tx: &mpsc::Sender<Vec<u8>>) {
    let mut stdin = std::io::stdin();
    let mut buffer = [0_u8; 4096];
    while let Ok(read) = stdin.read(&mut buffer) {
        if read == 0 || input_tx.blocking_send(buffer[..read].to_vec()).is_err() {
            break;
        }
    }
}

/// Console input as UTF-8. A surrogate pair split across two reads is kept
/// until its second half arrives; unpaired halves become U+FFFD.
#[cfg(any(windows, test))]
#[derive(Default)]
struct Utf16Decoder {
    high: Option<u16>,
}

#[cfg(any(windows, test))]
impl Utf16Decoder {
    /// The UTF-8 bytes of `units`, after any half pair left from the last call.
    fn decode(&mut self, units: &[u16]) -> Vec<u8> {
        let mut all: Vec<u16> = self
            .high
            .take()
            .into_iter()
            .chain(units.iter().copied())
            .collect();
        if all
            .last()
            .is_some_and(|unit| (0xD800..0xDC00).contains(unit))
        {
            self.high = all.pop();
        }
        char::decode_utf16(all)
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect::<String>()
            .into_bytes()
    }
}

/// Unit output for the outer terminal, unchanged on Unix.
#[cfg(unix)]
type OutputFilter = Passthrough;

/// Unit output for the outer terminal, without the pseudoconsole's own mode
/// switches.
#[cfg(windows)]
type OutputFilter = ConptyModes;

/// Forwards unit output as it is.
#[cfg(unix)]
#[derive(Default)]
struct Passthrough;

#[cfg(unix)]
impl Passthrough {
    /// `data` itself.
    fn filter<'a>(&mut self, data: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        std::borrow::Cow::Borrowed(data)
    }

    /// Nothing is ever held back.
    fn finish(&mut self) -> Vec<u8> {
        Vec::new()
    }
}

/// Mode switches ConPTY emits for itself when it starts (win32-input-mode and
/// focus events) and turns off when it closes. Forwarded, they would change how
/// the user's terminal sends keys and focus changes after attach ends. The cost:
/// a unit that requests focus events itself does not get them while attached.
#[cfg(any(windows, test))]
const CONPTY_MODES: [&[u8]; 4] = [
    b"\x1b[?9001h",
    b"\x1b[?9001l",
    b"\x1b[?1004h",
    b"\x1b[?1004l",
];

/// Removes [`CONPTY_MODES`] from a byte stream. A sequence may be split across
/// frames, so a chunk ending in a prefix of one is held back for the next.
#[cfg(any(windows, test))]
#[derive(Default)]
struct ConptyModes {
    pending: Vec<u8>,
}

#[cfg(any(windows, test))]
impl ConptyModes {
    /// `data` without the mode switches, after anything held back from the last call.
    fn filter<'a>(&mut self, data: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        if self.pending.is_empty() && !data.contains(&0x1b) {
            return std::borrow::Cow::Borrowed(data);
        }
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(data);
        let mut output = Vec::with_capacity(input.len());
        let mut at = 0;
        while let Some(&byte) = input.get(at) {
            if byte == 0x1b {
                let rest = &input[at..];
                if let Some(mode) = CONPTY_MODES.iter().find(|mode| rest.starts_with(mode)) {
                    at += mode.len();
                    continue;
                }
                if CONPTY_MODES.iter().any(|mode| mode.starts_with(rest)) {
                    self.pending = rest.to_vec();
                    break;
                }
            }
            output.push(byte);
            at += 1;
        }
        std::borrow::Cow::Owned(output)
    }

    /// What is held back, when the stream ends.
    fn finish(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pending)
    }
}

/// Terminal size changes, from SIGWINCH.
#[cfg(unix)]
struct Resizes(tokio::signal::unix::Signal);

#[cfg(unix)]
impl Resizes {
    /// Listens for SIGWINCH.
    fn new() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self(signal(SignalKind::window_change())?))
    }

    /// Waits for the next SIGWINCH; the new size, or `None` when it can't be read.
    async fn next(&mut self) -> Option<(u16, u16)> {
        self.0.recv().await;
        crossterm::terminal::size().ok()
    }
}

/// Terminal size changes, by polling: consoles have no SIGWINCH, and reading
/// console input events would compete with the raw stdin reader.
#[cfg(windows)]
struct Resizes {
    interval: tokio::time::Interval,
    last: Option<(u16, u16)>,
}

#[cfg(windows)]
impl Resizes {
    /// Starts from the current size, so only later changes are reported.
    fn new() -> std::io::Result<Self> {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Ok(Self {
            interval,
            last: crossterm::terminal::size().ok(),
        })
    }

    /// Waits until the size differs from the last one reported. Cancel-safe:
    /// nothing is awaited after a size is read.
    async fn next(&mut self) -> Option<(u16, u16)> {
        loop {
            self.interval.tick().await;
            let size = crossterm::terminal::size().ok();
            if size.is_some() && size != self.last {
                self.last = size;
                return size;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds `chunks` through one filter and joins what reaches the terminal.
    fn filtered(chunks: &[&[u8]]) -> Vec<u8> {
        let mut filter = ConptyModes::default();
        let mut output = Vec::new();
        for chunk in chunks {
            output.extend_from_slice(&filter.filter(chunk));
        }
        output.extend(filter.finish());
        output
    }

    #[test]
    fn conpty_mode_switches_are_removed_even_when_split() {
        let start = b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b[2J\x1b[mhi\r\n";
        assert_eq!(filtered(&[start]), b"\x1b[?25l\x1b[2J\x1b[mhi\r\n");
        // Split inside each sequence, one byte at a time.
        let bytes: Vec<&[u8]> = start.chunks(1).collect();
        assert_eq!(filtered(&bytes), b"\x1b[?25l\x1b[2J\x1b[mhi\r\n");
        assert_eq!(filtered(&[b"bye\x1b[?90", b"01l\x1b[?1004l"]), b"bye");
    }

    #[test]
    fn other_sequences_and_unfinished_prefixes_pass_through() {
        // Similar modes the unit itself sets are the user's business.
        assert_eq!(
            filtered(&[b"\x1b[?1049h\x1b[?100h"]),
            b"\x1b[?1049h\x1b[?100h"
        );
        assert_eq!(filtered(&[b"a\x1b[?9001x"]), b"a\x1b[?9001x");
        // A prefix at the end of the stream is flushed, not lost.
        assert_eq!(filtered(&[b"a\x1b[?10"]), b"a\x1b[?10");
        let mut filter = ConptyModes::default();
        assert!(matches!(
            filter.filter(b"plain"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    #[test]
    fn console_input_decodes_pairs_split_across_reads() {
        let mut decoder = Utf16Decoder::default();
        let units: Vec<u16> = "a😀\x1a\x03".encode_utf16().collect();
        let mut bytes = decoder.decode(&units[..2]);
        bytes.extend(decoder.decode(&units[2..]));
        assert_eq!(bytes, "a😀\x1a\x03".as_bytes());
        // A lone low surrogate is replaced, the rest kept.
        assert_eq!(
            decoder.decode(&[0xDC00, u16::from(b'x')]),
            "\u{FFFD}x".as_bytes()
        );
    }
}

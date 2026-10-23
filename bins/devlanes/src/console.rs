//! The Windows console for prompts and `attach`: where Unix opens `/dev/tty`,
//! switches termios modes and listens for SIGWINCH, Windows reads `CONIN$`
//! with console modes and polls the window size.
//!
//! Git Bash's mintty passes no typed keys to Windows programs. There a prompt
//! is shown on stderr and anything but a notice is refused, and `attach`
//! refuses to start; Windows Terminal, a console window or `winpty` work.

use std::io;

use devlanes_sys_windows::console::{self, Keyboard, ModeGuard};
use tokio::sync::mpsc;

/// Said under mintty, where Windows programs get no keys.
const MINTTY: &str = "mintty does not pass typed keys to Windows programs; \
                      use Windows Terminal or a console window, or run devlanes through winpty";

/// How a prompt goes, from where the user's keys go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// Shown on the console and answered there.
    Read,
    /// Cancelled without a word, as on Unix without a terminal to open.
    Cancel,
    /// Shown on stderr, which mintty's user sees; anything but a notice is
    /// refused there.
    Show { refused: bool },
}

/// The plan for a prompt of `kind` (`confirm`, `secret`, `notify`, ...).
fn plan(keyboard: Keyboard, kind: &str) -> Plan {
    match keyboard {
        Keyboard::Console => Plan::Read,
        Keyboard::Unreachable => Plan::Cancel,
        Keyboard::Mintty => Plan::Show {
            refused: kind != "notify",
        },
    }
}

/// Shows `prompt` on the console (`CONOUT$`) and reads one line from
/// `CONIN$`, which works when stdin and stdout are redirected, without echo
/// for secrets. Returns the line with its newline, so an empty answer stays
/// distinct from cancel; `None` when nobody can type into the console, for
/// Ctrl-C and for a Ctrl-Z line (end of input, like Ctrl-D on Unix).
pub fn read_tty(prompt: &str, kind: &str) -> Option<String> {
    match plan(console::keyboard(), kind) {
        Plan::Read => {}
        Plan::Cancel => return None,
        Plan::Show { refused } => {
            eprintln!("{prompt}");
            if !refused {
                return Some("\n".into());
            }
            eprintln!("devlanes: cancelled: {MINTTY}");
            return None;
        }
    }
    let input = console::open_input().ok()?;
    let output = console::open_output().ok()?;
    if kind == "notify" {
        console::write_console(&output, &format!("{prompt}\r\n")).ok()?;
        return Some("\n".into());
    }
    console::write_console(&output, &format!("{prompt} ")).ok()?;
    let secret = kind == "secret";
    let (set, clear) = if secret {
        (0, console::ECHO_INPUT)
    } else {
        (console::ECHO_INPUT, 0)
    };
    // The console host ends a line read on Ctrl-C itself. A listener keeps
    // the event from ending the process too, so Ctrl-C cancels just the
    // prompt; with the inherited ignore-Ctrl-C flag no handler runs at all.
    let ctrl_c = CtrlC::listen();
    // A line-edited read even if a program left the console raw; the guard
    // puts back the mode it had.
    let mode = ModeGuard::change(
        &input,
        console::LINE_INPUT | console::PROCESSED_INPUT | set,
        clear,
    )
    .ok()?;
    let line = read_line(&input);
    drop(mode);
    let interrupted = line
        .as_ref()
        .is_err_and(|error| error.kind() == io::ErrorKind::Interrupted);
    // The Enter of a hidden answer was not echoed, and Ctrl-C echoes nothing.
    if secret || interrupted {
        let _ = console::write_console(&output, "\r\n");
    }
    if interrupted && let Some(ctrl_c) = ctrl_c {
        ctrl_c.settle();
    }
    line.ok()
        .flatten()
        .map(|line| format!("{}\n", line.trim_end_matches(['\r', '\n'])))
}

/// One cooked line from `input`; `None` at end of input or for a line that
/// starts with Ctrl-Z.
fn read_line(input: &std::fs::File) -> io::Result<Option<String>> {
    let mut units = Vec::new();
    let mut buffer = [0_u16; 512];
    loop {
        let read = console::read_console_utf16(input, &mut buffer)?;
        if read == 0 {
            return Ok(None);
        }
        units.extend_from_slice(&buffer[..read]);
        if units.contains(&u16::from(b'\n')) {
            let line = String::from_utf16_lossy(&units);
            return Ok((!line.starts_with('\u{1a}')).then_some(line));
        }
    }
}

/// A Ctrl-C listener on the process's runtime. While one exists, tokio's
/// console handler reports Ctrl-C as handled, so the default handling does
/// not end the process.
struct CtrlC {
    runtime: tokio::runtime::Handle,
    listener: tokio::signal::windows::CtrlC,
}

impl CtrlC {
    /// Listens from now on; `None` outside a runtime.
    fn listen() -> Option<Self> {
        let runtime = tokio::runtime::Handle::try_current().ok()?;
        let listener = {
            let _entered = runtime.enter();
            tokio::signal::windows::ctrl_c().ok()?
        };
        Some(Self { runtime, listener })
    }

    /// Waits up to a second for the Ctrl-C that ended a read. Windows runs
    /// console handlers on a thread of its own after the read has already
    /// returned; a listener dropped before then would leave the event to the
    /// default handling, which ends the process. Called from a blocking
    /// thread, never from async code.
    fn settle(mut self) {
        let wait = tokio::time::timeout(std::time::Duration::from_secs(1), self.listener.recv());
        let _ = self.runtime.block_on(wait);
    }
}

/// Raw console input for `attach`, restored on drop, also when attach ends
/// through an error path or a console event ends the process.
pub struct RawGuard(Option<ModeGuard<io::Stdin>>);

impl RawGuard {
    /// Switches an interactive console to raw input with VT sequences: no
    /// line editing, echo or Ctrl-C processing, so keys reach the unit as from
    /// a raw Unix terminal, arrows and function keys as escape sequences.
    /// crossterm's raw mode leaves VT input off.
    pub fn enable(interactive: bool) -> io::Result<Self> {
        if !interactive {
            return Ok(Self(None));
        }
        match console::raw_vt_input(io::stdin()) {
            Ok(guard) => Ok(Self(Some(guard))),
            Err(_) if console::keyboard() == Keyboard::Mintty => Err(io::Error::other(format!(
                "attach needs a Windows console: {MINTTY}"
            ))),
            Err(error) => Err(error),
        }
    }
}

impl Drop for RawGuard {
    /// Cancels the reader's raw read before the mode goes back, so it cannot
    /// take a key typed for the shell afterwards.
    fn drop(&mut self) {
        if let Some(guard) = &self.0 {
            let _ = console::cancel_io(guard.handle());
        }
    }
}

/// Sends what the user types into the raw console to `input_tx` until input
/// ends or attach is gone. The console is read as UTF-16 text: std's console
/// reader would end input at Ctrl-Z, which in raw mode is a key for the unit.
pub fn read_input(input_tx: &mpsc::Sender<Vec<u8>>) {
    let stdin = io::stdin();
    let mut units = [0_u16; 2048];
    let mut decoder = Utf16Decoder::default();
    // Raw mode has no Ctrl-C processing, so any error is the end: a cancel
    // from `RawGuard` or a console that went away.
    while let Ok(read) = console::read_console_utf16(&stdin, &mut units) {
        if read == 0 {
            break;
        }
        let bytes = decoder.decode(&units[..read]);
        if !bytes.is_empty() && input_tx.blocking_send(bytes).is_err() {
            break;
        }
    }
}

/// Console input as UTF-8. A surrogate pair split across two reads is kept
/// until its second half arrives; unpaired halves become U+FFFD.
#[derive(Default)]
struct Utf16Decoder {
    high: Option<u16>,
}

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

/// Unit output for a console. std refuses to write bytes that are not UTF-8
/// to a console, which would end `attach` at the first stray byte of a
/// program writing in the ANSI code page, so invalid bytes become U+FFFD
/// there. A character split across two frames is held until it completes.
/// Output to anything else (a file, a pipe, mintty) passes unchanged.
pub struct ConsoleText {
    console: bool,
    pending: Vec<u8>,
}

impl ConsoleText {
    /// For this process's standard output.
    pub fn stdout() -> Self {
        Self {
            console: console::mode(io::stdout()).is_ok(),
            pending: Vec::new(),
        }
    }

    /// `data` as the console can take it.
    pub fn convert<'a>(&mut self, data: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        if !self.console || self.pending.is_empty() && std::str::from_utf8(data).is_ok() {
            return std::borrow::Cow::Borrowed(data);
        }
        let mut input = std::mem::take(&mut self.pending);
        input.extend_from_slice(data);
        let mut output = Vec::with_capacity(input.len());
        let mut rest = input.as_slice();
        loop {
            match std::str::from_utf8(rest) {
                Ok(valid) => {
                    output.extend_from_slice(valid.as_bytes());
                    break;
                }
                Err(error) => {
                    let (valid, after) = rest.split_at(error.valid_up_to());
                    output.extend_from_slice(valid);
                    match error.error_len() {
                        Some(invalid) => {
                            output.extend_from_slice("\u{FFFD}".as_bytes());
                            rest = &after[invalid..];
                        }
                        // The start of a character whose end is in the next frame.
                        None => {
                            self.pending = after.to_vec();
                            break;
                        }
                    }
                }
            }
        }
        std::borrow::Cow::Owned(output)
    }
}

/// Window size changes for `attach`, by polling every 250 ms: consoles have
/// no SIGWINCH, and reading console input events would compete with the raw
/// input reader.
pub struct Resizes {
    interval: tokio::time::Interval,
    last: Option<(u16, u16)>,
}

impl Resizes {
    /// Starts from the current size, so only later changes are reported.
    pub fn new() -> Self {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(250));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        Self {
            interval,
            last: crossterm::terminal::size().ok(),
        }
    }

    /// Waits until the size differs from the last one seen, like a SIGWINCH.
    /// Cancel-safe: nothing is awaited after the size is recorded.
    pub async fn recv(&mut self) -> Option<()> {
        loop {
            self.interval.tick().await;
            let size = crossterm::terminal::size().ok();
            if size.is_some() && size != self.last {
                self.last = size;
                return Some(());
            }
        }
    }
}

impl Default for Resizes {
    /// Same as [`Resizes::new`].
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn console_input_decodes_pairs_split_across_reads() {
        let mut decoder = Utf16Decoder::default();
        let units: Vec<u16> = "a😀\x1a\x03é".encode_utf16().collect();
        // Split between the two halves of the emoji.
        let mut bytes = decoder.decode(&units[..2]);
        assert_eq!(bytes, b"a");
        bytes.extend(decoder.decode(&units[2..]));
        assert_eq!(bytes, "a😀\x1a\x03é".as_bytes());
        // A read of only the high half waits for the next one.
        assert!(decoder.decode(&units[1..2]).is_empty());
        assert_eq!(decoder.decode(&units[2..3]), "😀".as_bytes());
        // Lone halves are replaced, the rest kept.
        assert_eq!(
            decoder.decode(&[0xDC00, u16::from(b'x')]),
            "\u{FFFD}x".as_bytes()
        );
        assert!(decoder.decode(&[0xD83D]).is_empty());
        assert_eq!(decoder.decode(&[u16::from(b'y')]), "\u{FFFD}y".as_bytes());
    }

    #[test]
    fn console_output_is_utf8_with_characters_split_across_frames_kept() {
        let mut text = ConsoleText {
            console: true,
            pending: Vec::new(),
        };
        let euro = "€".as_bytes();
        assert!(matches!(
            text.convert(b"plain\r\n"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert_eq!(text.convert(&[b'a', euro[0], euro[1]]).as_ref(), b"a");
        assert_eq!(text.convert(&[euro[2], b'b']).as_ref(), "€b".as_bytes());
        // A Windows-1252 byte cannot reach the console as it is.
        assert_eq!(
            text.convert(b"caf\xe9!\xff").as_ref(),
            "caf\u{FFFD}!\u{FFFD}".as_bytes()
        );
        let mut file = ConsoleText {
            console: false,
            pending: Vec::new(),
        };
        assert_eq!(file.convert(b"caf\xe9").as_ref(), b"caf\xe9");
    }

    #[test]
    fn prompts_need_a_console_someone_types_into() {
        assert_eq!(plan(Keyboard::Console, "secret"), Plan::Read);
        assert_eq!(plan(Keyboard::Console, "notify"), Plan::Read);
        // No console window: nobody could answer, so nothing waits.
        for kind in ["confirm", "secret", "notify"] {
            assert_eq!(plan(Keyboard::Unreachable, kind), Plan::Cancel, "{kind}");
        }
        // mintty shows the prompt and refuses anything that needs an answer.
        assert_eq!(
            plan(Keyboard::Mintty, "confirm"),
            Plan::Show { refused: true }
        );
        assert_eq!(
            plan(Keyboard::Mintty, "secret"),
            Plan::Show { refused: true }
        );
        assert_eq!(
            plan(Keyboard::Mintty, "notify"),
            Plan::Show { refused: false }
        );
    }
}

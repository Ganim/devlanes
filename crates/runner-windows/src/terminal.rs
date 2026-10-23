//! The pseudo console of a `pty: true` unit: its input writer, its resizes,
//! and the filter that takes the console host's own mode switches out of its
//! output and marks where a new size begins.

use std::{
    io::{self, PipeWriter, Write},
    sync::{Arc, Mutex, PoisonError, Weak},
};

use bytes::Bytes;
use devlanes_core::TerminalSize;
use devlanes_sys_windows::conpty::PseudoConsole;
use tokio::sync::{mpsc, oneshot};

/// Input chunks queued before callers wait; enough for typing and pastes.
const INPUT_QUEUE: usize = 64;
/// The longest parameter string of a sequence [`Toggles`] removes
/// (`8;32767;32767`); anything longer is passed on at once.
const MAX_PARAMS: usize = 16;
const ESC: u8 = 0x1b;
/// Hides the cursor; the console host starts every repaint with it.
const HIDE_CURSOR: &[u8] = b"\x1b[?25l";
/// Cursor home; right after [`HIDE_CURSOR`] it starts a repaint of the window.
const HOME: &[u8] = b"\x1b[H";

/// Bytes for the console's input, and where to report the write's result.
pub(crate) type Input = (Bytes, oneshot::Sender<io::Result<()>>);

/// A unit's pseudo console. Dropping it closes the console's input and the
/// console itself, which ends its output.
pub(crate) struct Terminal {
    /// Dropped first: the field order is the drop order.
    pub(crate) input: mpsc::Sender<Input>,
    /// Shared with the output reader, which holds it weakly, so dropping this
    /// closes the console.
    pub(crate) sizing: Arc<Sizing>,
    console: Arc<PseudoConsole>,
}

impl Terminal {
    /// Takes over `console`, created at `size`, and the write end of its
    /// input pipe, which a thread of its own writes to.
    pub(crate) fn new(
        console: PseudoConsole,
        size: TerminalSize,
        input: PipeWriter,
    ) -> io::Result<Self> {
        let console = Arc::new(console);
        Ok(Self {
            input: input_writer(input)?,
            sizing: Arc::new(Sizing {
                state: Mutex::new(SizeState {
                    current: size,
                    pending: None,
                    next: None,
                }),
                console: Arc::downgrade(&console),
            }),
            console,
        })
    }

    /// The console, for starting the leader in it.
    pub(crate) fn console(&self) -> &PseudoConsole {
        &self.console
    }
}

/// Starts the thread that writes queued input to the console in order. Writes
/// block while the console does not read, so they cannot run on the executor,
/// and one thread keeps them in order even when callers give up waiting: a
/// write already queued still happens, in its place. The thread ends, closing
/// the pipe, once every sender is gone.
fn input_writer(mut pipe: PipeWriter) -> io::Result<mpsc::Sender<Input>> {
    let (sender, mut queue) = mpsc::channel::<Input>(INPUT_QUEUE);
    std::thread::Builder::new()
        .name("unit-input".into())
        .spawn(move || {
            while let Some((bytes, reply)) = queue.blocking_recv() {
                let _ = reply.send(pipe.write_all(&bytes));
            }
        })?;
    Ok(sender)
}

/// The console's size and the resizes on their way to it.
///
/// The console host takes a resize at some point after it was asked and
/// repaints its window at the new size; output before that point is drawn at
/// the old one. Whoever models the screen must switch sizes at exactly that
/// point, so the output reader marks it (see [`Toggles`]). One resize is in
/// flight at a time: a later request waits as `next` until the repaint of the
/// one before has been seen, and only the latest such request is kept. This
/// relies on the inbox console host repainting after every change of size,
/// which it does.
#[derive(Debug)]
pub(crate) struct Sizing {
    state: Mutex<SizeState>,
    console: Weak<PseudoConsole>,
}

#[derive(Debug)]
struct SizeState {
    /// The size the output is drawn at, as of the last repaint seen.
    current: TerminalSize,
    /// Asked of the console; its repaint has not been seen yet.
    pending: Option<TerminalSize>,
    /// Asked while another resize was pending.
    next: Option<TerminalSize>,
}

impl Sizing {
    /// Asks for `size`, now or once the resize in flight was repainted.
    pub(crate) fn request(&self, size: TerminalSize) -> io::Result<()> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.pending.is_some() {
            state.next = Some(size);
            return Ok(());
        }
        state.next = None;
        self.start(&mut state, size)
    }

    /// Resizes the console to `size` unless it has that size; the lock is
    /// held, so the reader cannot see the repaint before it is expected.
    fn start(&self, state: &mut SizeState, size: TerminalSize) -> io::Result<()> {
        if size == state.current {
            return Ok(());
        }
        let Some(console) = self.console.upgrade() else {
            return Err(io::Error::from(io::ErrorKind::BrokenPipe));
        };
        console.resize(size.columns, size.rows)?;
        state.pending = Some(size);
        Ok(())
    }

    /// Called by the reader at the start of a repaint: the size it is drawn
    /// at, if a resize was waiting for it, after which the next request goes
    /// out.
    fn repainted(&self) -> Option<TerminalSize> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let size = state.pending.take()?;
        state.current = size;
        if let Some(next) = state.next.take()
            && let Err(error) = self.start(&mut state, next)
        {
            tracing::debug!(%error, "resizing a pseudo console failed");
        }
        Some(size)
    }
}

/// Filters pseudo console output.
///
/// It removes the sequences the inbox console host sends for itself:
/// win32-input-mode (`ESC[?9001h`/`l`) and focus reporting (`ESC[?1004h`/`l`),
/// which it asks of the terminal it renders to and which a viewer would answer
/// with key encodings and focus events the unit never asked for, and the window
/// size report `ESC[8;<rows>;<columns>t` it sends after some resizes, which
/// would resize a viewer's window.
///
/// Where a repaint for a resize begins (`ESC[?25l ESC[H`), it puts
/// `ESC[8;<rows>;<columns>t` for the new size, so the daemon's screen model
/// switches sizes at the first byte drawn at the new one (see
/// [`devlanes_core::Unit::renders_screen`]). A sequence split across reads is
/// held until it ends; everything else passes unchanged.
#[derive(Debug, Default)]
pub(crate) struct Toggles {
    /// The start of a control sequence that may be one to remove.
    pending: Vec<u8>,
    /// An `ESC[?25l` held back until the next sequence shows whether a repaint
    /// starts.
    hidden: bool,
    /// Asked at each repaint start for the size it is drawn at.
    sizing: Option<Arc<Sizing>>,
}

impl Toggles {
    /// A filter that marks the repaints `sizing` waits for.
    pub(crate) fn with_sizing(sizing: Arc<Sizing>) -> Self {
        Self {
            sizing: Some(sizing),
            ..Self::default()
        }
    }

    /// `bytes` without the removed sequences. The start of a sequence at the
    /// end is kept for the next call.
    pub(crate) fn filter(&mut self, bytes: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len() + self.pending.len() + 16);
        for &byte in bytes {
            self.feed(byte, &mut out);
        }
        out
    }

    /// What is held at the end of output; it never became a removed sequence.
    pub(crate) fn finish(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        self.release(&mut out);
        out.append(&mut self.pending);
        out
    }

    /// Writes a held `ESC[?25l`.
    fn release(&mut self, out: &mut Vec<u8>) {
        if std::mem::take(&mut self.hidden) {
            out.extend_from_slice(HIDE_CURSOR);
        }
    }

    /// One byte through the filter.
    fn feed(&mut self, byte: u8, out: &mut Vec<u8>) {
        if self.pending.is_empty() {
            if byte == ESC {
                self.pending.push(byte);
            } else {
                self.release(out);
                out.push(byte);
            }
            return;
        }
        let at = self.pending.len();
        let accepted = match at {
            1 => byte == b'[',
            _ if (0x30..=0x3f).contains(&byte) => {
                if at - 2 >= MAX_PARAMS {
                    // Too long to be removed: passed on as it is.
                    self.release(out);
                    out.append(&mut self.pending);
                    out.push(byte);
                    return;
                }
                true
            }
            _ if (0x40..=0x7e).contains(&byte) => {
                self.pending.push(byte);
                let sequence = std::mem::take(&mut self.pending);
                self.sequence(&sequence, out);
                return;
            }
            _ => false,
        };
        if accepted {
            self.pending.push(byte);
        } else {
            // Not a sequence that is removed; the byte may start one itself.
            self.release(out);
            out.append(&mut self.pending);
            self.feed(byte, out);
        }
    }

    /// One whole control sequence, `ESC [ ... <final>`.
    fn sequence(&mut self, sequence: &[u8], out: &mut Vec<u8>) {
        let (params, last) = (
            &sequence[2..sequence.len() - 1],
            sequence[sequence.len() - 1],
        );
        if removed(params, last) {
            return;
        }
        if sequence == HIDE_CURSOR {
            self.release(out);
            self.hidden = true;
            return;
        }
        if self.hidden && sequence == HOME {
            self.hidden = false;
            if let Some(size) = self.sizing.as_ref().and_then(|sizing| sizing.repainted()) {
                out.extend_from_slice(format!("\x1b[8;{};{}t", size.rows, size.columns).as_bytes());
            }
            out.extend_from_slice(HIDE_CURSOR);
        }
        self.release(out);
        out.extend_from_slice(sequence);
    }
}

/// Whether `ESC [ <params> <last>` is one of the sequences [`Toggles`] removes.
fn removed(params: &[u8], last: u8) -> bool {
    match last {
        b'h' | b'l' => params == b"?9001" || params == b"?1004",
        b't' => {
            let mut fields = params.split(|&byte| byte == b';');
            fields.next() == Some(b"8")
                && fields
                    .by_ref()
                    .take(2)
                    .filter(|field| digits(field))
                    .count()
                    == 2
                && fields.next().is_none()
        }
        _ => false,
    }
}

/// Whether `field` is a non-empty run of ASCII digits.
fn digits(field: &[u8]) -> bool {
    !field.is_empty() && field.iter().all(u8::is_ascii_digit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The console host's toggles and size reports go, wherever reads split
    /// them; other sequences, including look-alikes, stay byte for byte.
    #[test]
    fn toggles_are_removed_across_reads() {
        let input: &[u8] = b"\x1b[?9001h\x1b[?1004h\x1b[?25l\x1b[2J\x1b[Hred\r\n\
            \x1b[8;20;80t\x1b[?1049h\x1b[?9001;1004h\x1b[8;1t\x1b[18t\x1b[38;5;1mx\x1b\x1b[?1004l\
            \x1b[?9001l\x1b]0;title\x07\x1b[?10040h\x1b[?25l\x1b[8;5;9t\x1b[Hok\x1b[?9001";
        let kept: &[u8] = b"\x1b[?25l\x1b[2J\x1b[Hred\r\n\x1b[?1049h\x1b[?9001;1004h\
            \x1b[8;1t\x1b[18t\x1b[38;5;1mx\x1b\x1b]0;title\x07\x1b[?10040h\x1b[?25l\x1b[Hok";
        for split in 0..=input.len() {
            let mut toggles = Toggles::default();
            let mut out = toggles.filter(&input[..split]);
            out.extend(toggles.filter(&input[split..]));
            assert_eq!(out, kept, "split at {split}");
            // The unfinished toggle at the end never completed.
            assert_eq!(toggles.finish(), b"\x1b[?9001", "split at {split}");
        }
        // One byte per read.
        let mut toggles = Toggles::default();
        let out: Vec<u8> = input
            .iter()
            .flat_map(|byte| toggles.filter(std::slice::from_ref(byte)))
            .collect();
        assert_eq!(out, kept);
    }

    /// A long parameter string passes as soon as it cannot be removed, so a
    /// sequence that never ends holds nothing back; so does a held `ESC[?25l`
    /// at the end of output.
    #[test]
    fn long_sequences_pass_at_once() {
        let mut toggles = Toggles::default();
        let long = format!("\x1b[{}", "1;".repeat(20));
        assert_eq!(toggles.filter(long.as_bytes()), long.as_bytes());
        assert!(toggles.finish().is_empty());
        assert!(toggles.filter(b"a\x1b[?25l").starts_with(b"a"));
        assert_eq!(toggles.finish(), b"\x1b[?25l");
    }

    /// A sizing for tests: no console to resize, one resize in flight.
    fn sizing(pending: Option<TerminalSize>) -> Arc<Sizing> {
        Arc::new(Sizing {
            state: Mutex::new(SizeState {
                current: TerminalSize::RENDERED_HEADLESS,
                pending,
                next: None,
            }),
            console: Weak::new(),
        })
    }

    /// The repaint that follows a resize gets the new size in front of it,
    /// once; later repaints and other cursor homes get nothing.
    #[test]
    fn the_repaint_of_a_resize_is_marked() {
        let size = TerminalSize {
            columns: 152,
            rows: 45,
        };
        let sizing = sizing(Some(size));
        let mut toggles = Toggles::with_sizing(Arc::clone(&sizing));
        let out = toggles.filter(b"x\r\n\x1b[Hy\x1b[?25l\x1b[8;45;152t");
        assert_eq!(out, b"x\r\n\x1b[Hy");
        let out = toggles.filter(b"\x1b[HP1\x1b[K\x1b[?25h\x1b[?25l\x1b[HP2");
        assert_eq!(
            out,
            b"\x1b[8;45;152t\x1b[?25l\x1b[HP1\x1b[K\x1b[?25h\x1b[?25l\x1b[HP2"
        );
        let state = sizing.state.lock().expect("state");
        assert_eq!((state.current, state.pending), (size, None));
    }

    /// A request while a resize is in flight waits for its repaint; only the
    /// latest one is kept, and one for the size in effect asks nothing.
    #[test]
    fn resizes_wait_for_the_repaint_before() {
        let first = TerminalSize {
            columns: 200,
            rows: 45,
        };
        let sizing = sizing(Some(first));
        let later = TerminalSize {
            columns: 152,
            rows: 45,
        };
        sizing.request(later).expect("queued");
        sizing
            .request(TerminalSize::RENDERED_HEADLESS)
            .expect("queued");
        assert_eq!(sizing.repainted(), Some(first));
        // The console is gone in this test, so the queued request fails to go out.
        let state = sizing.state.lock().expect("state");
        assert_eq!(
            (state.current, state.pending, state.next),
            (first, None, None)
        );
        drop(state);
        sizing.request(first).expect("no change");
        assert_eq!(sizing.repainted(), None);
    }
}

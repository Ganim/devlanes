/// Raw records are split at this bound, even when a child never writes a newline.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// The caller's timer should poll at least this often to expose interactive prompts.
pub const PARTIAL_FLUSH_MS: u64 = 200;

/// A frame retains terminal bytes separately from searchable, lossy UTF-8 text.
#[derive(Debug, Clone)]
pub struct Frame {
    pub raw: Vec<u8>,
    pub text: String,
    pub timestamp_ms: u64,
    pub partial: bool,
    pub continued: bool,
}

#[derive(Default)]
struct Text {
    value: String,
    carriage_return: bool,
}

impl vte::Perform for Text {
    /// A printable character resolves a bare CR, replacing the progress line.
    fn print(&mut self, c: char) {
        if self.carriage_return {
            self.value.clear();
            self.carriage_return = false;
        }
        self.value.push(c);
    }

    /// CRLF keeps the preceding text; other terminal controls stay out of disk logs.
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return = true,
            b'\t' => self.print('\t'),
            _ => {}
        }
    }
}

/// Stateful ANSI/UTF-8 parsing survives arbitrary read boundaries and partial flushes.
pub struct Framer {
    parser: vte::Parser,
    control: ControlString,
    text: Text,
    raw: Vec<u8>,
    last_byte_ms: u64,
    emitted_boundary: bool,
}

#[derive(Default)]
enum ControlString {
    #[default]
    Normal,
    Escape,
    Osc,
    OscEscape,
}

impl Default for Framer {
    /// Each stdout/stderr stream needs its own framer to avoid merging partial lines.
    fn default() -> Self {
        Self {
            parser: vte::Parser::new(),
            control: ControlString::Normal,
            text: Text::default(),
            raw: Vec::new(),
            last_byte_ms: 0,
            emitted_boundary: false,
        }
    }
}

impl Framer {
    /// Emits through a callback so arbitrarily large input cannot grow a frame queue.
    pub fn ingest<E>(
        &mut self,
        mut bytes: &[u8],
        timestamp_ms: u64,
        mut emit: impl FnMut(Frame) -> Result<(), E>,
    ) -> Result<(), E> {
        while !bytes.is_empty() {
            self.last_byte_ms = timestamp_ms;
            let available = (MAX_LINE_BYTES - self.raw.len()).min(bytes.len());
            let count = memchr::memchr(b'\n', &bytes[..available]).map_or(available, |i| i + 1);
            let chunk = &bytes[..count];
            self.raw.extend_from_slice(chunk);
            for &byte in chunk {
                self.advance(byte);
            }
            bytes = &bytes[count..];
            if chunk.last() == Some(&b'\n') {
                if self.emitted_boundary && self.raw.len() == 1 && self.text.value.is_empty() {
                    self.raw.clear();
                    self.text.carriage_return = false;
                    self.emitted_boundary = false;
                } else {
                    emit(self.take(false, false))?;
                }
            } else if self.raw.len() >= MAX_LINE_BYTES {
                emit(self.take(false, true))?;
            }
        }
        Ok(())
    }

    /// Uses caller time for deterministic timers; `force` is for EOF and shutdown.
    pub fn flush_partial(&mut self, now_ms: u64, force: bool) -> Option<Frame> {
        if force {
            // NUL terminates incomplete UTF-8 without adding a printable character.
            self.parser.advance(&mut self.text, b"\0");
        }
        if (self.raw.is_empty() && self.text.value.is_empty())
            || (!force && now_ms.saturating_sub(self.last_byte_ms) < PARTIAL_FLUSH_MS)
        {
            return None;
        }
        Some(self.take(true, false))
    }

    /// Parser state stays alive because an escape or code point may span the cap.
    fn take(&mut self, partial: bool, continued: bool) -> Frame {
        self.text.carriage_return = false;
        self.emitted_boundary = partial || continued;
        Frame {
            raw: std::mem::take(&mut self.raw),
            text: std::mem::take(&mut self.text.value),
            timestamp_ms: self.last_byte_ms,
            partial,
            continued,
        }
    }

    /// Discard OSC payloads ourselves: Cargo feature unification can enable vte's
    /// unbounded std OSC buffer even though this crate disables its default features.
    fn advance(&mut self, byte: u8) {
        match self.control {
            ControlString::Normal if byte == 0x1b => self.control = ControlString::Escape,
            ControlString::Escape if byte == b']' => {
                // Cancel an earlier incomplete CSI/UTF-8 sequence before hiding OSC.
                self.parser.advance(&mut self.text, &[0x18]);
                self.control = ControlString::Osc;
            }
            ControlString::Escape => {
                self.parser.advance(&mut self.text, &[0x1b]);
                if byte != 0x1b {
                    self.parser.advance(&mut self.text, &[byte]);
                    self.control = ControlString::Normal;
                }
            }
            ControlString::Osc | ControlString::OscEscape
                if matches!(byte, 7 | 0x18 | 0x1a | 0x9c) =>
            {
                self.control = ControlString::Normal
            }
            ControlString::OscEscape if byte == b'\\' => self.control = ControlString::Normal,
            ControlString::Osc | ControlString::OscEscape if byte == 0x1b => {
                self.control = ControlString::OscEscape
            }
            ControlString::OscEscape => self.control = ControlString::Osc,
            ControlString::Osc => {}
            ControlString::Normal => self.parser.advance(&mut self.text, &[byte]),
        }
    }
}

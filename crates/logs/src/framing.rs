/// Raw records are split at this bound, even when a child never writes a newline.
pub const MAX_LINE_BYTES: usize = 16 * 1024;
/// The caller's timer should poll at least this often to expose interactive prompts.
pub const PARTIAL_FLUSH_MS: u64 = 200;

/// A frame retains terminal bytes separately from searchable, lossy UTF-8 text.
#[derive(Debug, Clone)]
pub struct Frame {
    pub raw: Vec<u8>,
    pub text: String,
    /// `text` with its SGR color sequences and a closing reset, when it had any.
    pub styled: Option<String>,
    pub timestamp_ms: u64,
    pub partial: bool,
    pub continued: bool,
}

/// Resets every SGR attribute; ends each styled line.
pub const SGR_RESET: &str = "\x1b[0m";

#[derive(Default)]
struct Text {
    value: String,
    /// `value` with SGR sequences kept.
    styled: String,
    /// Some SGR sequence is in `styled`.
    has_style: bool,
    /// SGR sequences since the last reset: they still apply on the next line.
    active: String,
    carriage_return: bool,
}

impl Text {
    /// Starts the next line with the colors still in effect.
    fn next_line(&mut self) {
        self.styled.clone_from(&self.active);
        self.has_style = !self.active.is_empty();
    }
}

impl vte::Perform for Text {
    /// A printable character resolves a bare CR, replacing the progress line.
    fn print(&mut self, c: char) {
        if self.carriage_return {
            self.value.clear();
            self.next_line();
            self.carriage_return = false;
        }
        self.value.push(c);
        self.styled.push(c);
    }

    /// CRLF keeps the preceding text; other terminal controls stay out of disk logs.
    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return = true,
            b'\t' => self.print('\t'),
            _ => {}
        }
    }

    /// Keeps SGR (`ESC [ … m`), the only sequence that colors text; cursor moves
    /// and the rest stay out of disk logs.
    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if action != 'm' || ignore || !intermediates.is_empty() {
            return;
        }
        let mut sequence = String::from("\x1b[");
        let mut reset = true;
        for (index, param) in params.iter().enumerate() {
            if index > 0 {
                sequence.push(';');
            }
            for (sub, value) in param.iter().enumerate() {
                if sub > 0 {
                    sequence.push(':');
                }
                sequence.push_str(&value.to_string());
                reset &= *value == 0;
            }
        }
        sequence.push('m');
        if reset {
            self.active.clear();
        } else {
            self.active.push_str(&sequence);
        }
        self.styled.push_str(&sequence);
        self.has_style = true;
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
        let styled = std::mem::take(&mut self.text.styled);
        // Only a line with visible text needs its colors; the reset keeps them
        // from reaching whatever a client prints next.
        let styled =
            (self.text.has_style && !self.text.value.is_empty()).then(|| styled + SGR_RESET);
        self.text.next_line();
        Frame {
            raw: std::mem::take(&mut self.raw),
            text: std::mem::take(&mut self.text.value),
            styled,
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

//! Encodes keys, paste and mouse events into the bytes a terminal program expects.

use base64::Engine;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

/// The vt100 modes that change how input is encoded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InputModes {
    pub application_cursor: bool,
    pub application_keypad: bool,
    pub bracketed_paste: bool,
    pub mouse: MouseReporting,
}

/// Mouse reporting requested by the program, as tracked by vt100.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseReporting {
    #[default]
    Off,
    /// Presses only (X10 / 1000) with legacy or SGR encoding.
    Press { sgr: bool },
    /// Presses, releases and motion while a button is held or always.
    Motion { sgr: bool, any: bool },
}

impl InputModes {
    /// Reads the modes from a vt100 screen.
    pub fn from_screen(screen: &vt100::Screen) -> Self {
        let sgr = screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr;
        let mouse = match screen.mouse_protocol_mode() {
            vt100::MouseProtocolMode::None => MouseReporting::Off,
            vt100::MouseProtocolMode::Press | vt100::MouseProtocolMode::PressRelease => {
                MouseReporting::Press { sgr }
            }
            vt100::MouseProtocolMode::ButtonMotion => MouseReporting::Motion { sgr, any: false },
            vt100::MouseProtocolMode::AnyMotion => MouseReporting::Motion { sgr, any: true },
        };
        Self {
            application_cursor: screen.application_cursor(),
            application_keypad: screen.application_keypad(),
            bracketed_paste: screen.bracketed_paste(),
            mouse,
        }
    }
}

/// Encodes one key press; `None` for keys a terminal program cannot receive.
pub fn encode_key(key: &KeyEvent, modes: InputModes) -> Option<Vec<u8>> {
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let modifier = xterm_modifier(shift, alt, ctrl);
    let mut bytes = match key.code {
        KeyCode::Char(c) if ctrl => vec![control_byte(c)?],
        KeyCode::Char(c) => c.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace if ctrl => vec![0x08],
        KeyCode::Backspace => vec![0x7f],
        KeyCode::Tab => vec![b'\t'],
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::Esc => vec![0x1b],
        KeyCode::Up => cursor_key(b'A', modes, modifier),
        KeyCode::Down => cursor_key(b'B', modes, modifier),
        KeyCode::Right => cursor_key(b'C', modes, modifier),
        KeyCode::Left => cursor_key(b'D', modes, modifier),
        KeyCode::Home => cursor_key(b'H', modes, modifier),
        KeyCode::End => cursor_key(b'F', modes, modifier),
        KeyCode::Insert => tilde_key(2, modifier),
        KeyCode::Delete => tilde_key(3, modifier),
        KeyCode::PageUp => tilde_key(5, modifier),
        KeyCode::PageDown => tilde_key(6, modifier),
        KeyCode::F(n @ 1..=4) => {
            let last = b"PQRS"[usize::from(n - 1)];
            match modifier {
                Some(m) => format!("\x1b[1;{m}{}", char::from(last)).into_bytes(),
                None => vec![0x1b, b'O', last],
            }
        }
        KeyCode::F(n) => {
            let code = match n {
                5 => 15,
                6 => 17,
                7 => 18,
                8 => 19,
                9 => 20,
                10 => 21,
                11 => 23,
                12 => 24,
                _ => return None,
            };
            tilde_key(code, modifier)
        }
        _ => return None,
    };
    // Alt is sent as an ESC prefix for plain characters (xterm's metaSendsEscape).
    if alt
        && matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Enter | KeyCode::Backspace
        )
    {
        bytes.insert(0, 0x1b);
    }
    Some(bytes)
}

/// xterm's modifier parameter: 1 + shift(1) + alt(2) + ctrl(4), or none when unmodified.
fn xterm_modifier(shift: bool, alt: bool, ctrl: bool) -> Option<u8> {
    let value = 1 + u8::from(shift) + 2 * u8::from(alt) + 4 * u8::from(ctrl);
    (value > 1).then_some(value)
}

/// Arrows, Home and End use SS3 in application cursor mode unless modified.
fn cursor_key(last: u8, modes: InputModes, modifier: Option<u8>) -> Vec<u8> {
    match modifier {
        Some(m) => format!("\x1b[1;{m}{}", char::from(last)).into_bytes(),
        None if modes.application_cursor => vec![0x1b, b'O', last],
        None => vec![0x1b, b'[', last],
    }
}

/// `CSI n ~` keys with an optional modifier parameter.
fn tilde_key(code: u8, modifier: Option<u8>) -> Vec<u8> {
    match modifier {
        Some(m) => format!("\x1b[{code};{m}~").into_bytes(),
        None => format!("\x1b[{code}~").into_bytes(),
    }
}

/// Maps Ctrl+char to its C0 byte.
fn control_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        ' ' | '@' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' => Some(0x1f),
        '?' | '8' => Some(0x7f),
        _ => None,
    }
}

/// Encodes pasted text. Line endings become CR like a real terminal, and the
/// text is wrapped in bracketed-paste markers when the program asked for them.
pub fn encode_paste(text: &str, modes: InputModes) -> Vec<u8> {
    let normalized = text.replace("\r\n", "\r").replace('\n', "\r");
    if !modes.bracketed_paste {
        return normalized.into_bytes();
    }
    // A pasted end marker would let the paste escape bracketing.
    let safe = normalized.replace("\x1b[201~", "");
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(safe.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// Encodes a mouse event at pane-relative 0-based `(column, row)`, or `None`
/// when the program did not ask for this kind of event.
pub fn encode_mouse(
    event: &MouseEvent,
    column: u16,
    row: u16,
    modes: InputModes,
) -> Option<Vec<u8>> {
    let (sgr, motion, any) = match modes.mouse {
        MouseReporting::Off => return None,
        MouseReporting::Press { sgr } => (sgr, false, false),
        MouseReporting::Motion { sgr, any } => (sgr, true, any),
    };
    let button_code = |button: MouseButton| match button {
        MouseButton::Left => 0u16,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    };
    let (mut code, release) = match event.kind {
        MouseEventKind::Down(button) => (button_code(button), false),
        MouseEventKind::Up(button) => (button_code(button), true),
        MouseEventKind::Drag(button) if motion => (button_code(button) + 32, false),
        MouseEventKind::Moved if any => (3 + 32, false),
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        _ => return None,
    };
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        code += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        code += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        code += 16;
    }
    let (x, y) = (column + 1, row + 1);
    if sgr {
        let end = if release { 'm' } else { 'M' };
        return Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes());
    }
    // Legacy encoding has no release button identity and caps coordinates at 223.
    let code = if release { 3 } else { code };
    let clamp = |v: u16| u8::try_from(v.min(223) + 32).unwrap_or(255);
    Some(vec![
        0x1b,
        b'[',
        b'M',
        u8::try_from(code + 32).unwrap_or(255),
        clamp(x),
        clamp(y),
    ])
}

/// OSC 52 sets the clipboard of the outer terminal, including over SSH and tmux.
pub fn osc52(text: &str) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{encoded}\x07")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn arrows_follow_application_cursor_mode() {
        let normal = InputModes::default();
        let app = InputModes {
            application_cursor: true,
            ..normal
        };
        let up = key(KeyCode::Up, KeyModifiers::NONE);
        assert_eq!(encode_key(&up, normal), Some(b"\x1b[A".to_vec()));
        assert_eq!(encode_key(&up, app), Some(b"\x1bOA".to_vec()));
        let ctrl_left = key(KeyCode::Left, KeyModifiers::CONTROL);
        assert_eq!(encode_key(&ctrl_left, app), Some(b"\x1b[1;5D".to_vec()));
    }

    #[test]
    fn control_and_alt_characters() {
        let modes = InputModes::default();
        assert_eq!(
            encode_key(&key(KeyCode::Char('c'), KeyModifiers::CONTROL), modes),
            Some(vec![3])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('x'), KeyModifiers::ALT), modes),
            Some(b"\x1bx".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Char('é'), KeyModifiers::NONE), modes),
            Some("é".as_bytes().to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Enter, KeyModifiers::NONE), modes),
            Some(vec![b'\r'])
        );
        assert_eq!(
            encode_key(&key(KeyCode::Backspace, KeyModifiers::NONE), modes),
            Some(vec![0x7f])
        );
    }

    #[test]
    fn function_and_tilde_keys() {
        let modes = InputModes::default();
        assert_eq!(
            encode_key(&key(KeyCode::F(1), KeyModifiers::NONE), modes),
            Some(b"\x1bOP".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::F(5), KeyModifiers::NONE), modes),
            Some(b"\x1b[15~".to_vec())
        );
        assert_eq!(
            encode_key(&key(KeyCode::Delete, KeyModifiers::SHIFT), modes),
            Some(b"\x1b[3;2~".to_vec())
        );
    }

    #[test]
    fn paste_brackets_and_normalizes_newlines() {
        let plain = InputModes::default();
        assert_eq!(encode_paste("a\nb\r\nc", plain), b"a\rb\rc".to_vec());
        let bracketed = InputModes {
            bracketed_paste: true,
            ..plain
        };
        assert_eq!(
            encode_paste("ls\x1b[201~rm", bracketed),
            b"\x1b[200~lsrm\x1b[201~".to_vec()
        );
    }

    #[test]
    fn modes_come_from_the_screen() {
        let mut parser = vt100::Parser::new(
            std::num::NonZeroU16::MIN.saturating_add(23),
            std::num::NonZeroU16::MIN.saturating_add(79),
            0,
        );
        parser.process(b"\x1b[?1h\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        let modes = InputModes::from_screen(parser.screen());
        assert!(modes.application_cursor);
        assert!(modes.bracketed_paste);
        assert_eq!(modes.mouse, MouseReporting::Press { sgr: true });
    }

    #[test]
    fn mouse_encoding_sgr_and_legacy() {
        let event = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        };
        let sgr = InputModes {
            mouse: MouseReporting::Press { sgr: true },
            ..InputModes::default()
        };
        assert_eq!(
            encode_mouse(&event, 4, 2, sgr),
            Some(b"\x1b[<0;5;3M".to_vec())
        );
        let legacy = InputModes {
            mouse: MouseReporting::Press { sgr: false },
            ..InputModes::default()
        };
        assert_eq!(
            encode_mouse(&event, 4, 2, legacy),
            Some(vec![0x1b, b'[', b'M', 32, 37, 35])
        );
        assert_eq!(encode_mouse(&event, 4, 2, InputModes::default()), None);
    }

    #[test]
    fn osc52_is_base64() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }
}

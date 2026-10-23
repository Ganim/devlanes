//! Marks at the start of sections in long command output (one unit's logs, a
//! failure), so the terminal's jump-to-mark or jump-to-prompt keys move
//! between them.
//!
//! iTerm2 gets `OSC 1337 ; SetMark`, its own mark, which leaves its shell
//! integration alone. Terminals that jump between `OSC 133` prompts (WezTerm,
//! Ghostty, kitty, foot, Windows Terminal) get `133;A` before a section's
//! heading and `133;C` after it, so the heading is where a jump lands and
//! what follows reads as its output.

use crate::detect::{Emulator, Mode, gate};

/// Environment override for the config's `terminal.marks`: `0` turns them
/// off, `1` writes them in any terminal.
pub const MARKS_ENV: &str = "DEVLANES_TERM_MARKS";

/// The mark this terminal jumps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marks {
    /// `OSC 1337 ; SetMark` (iTerm2, cmd-shift-up and down).
    Iterm2,
    /// `OSC 133 ; A` and `C` (WezTerm `ScrollToPrompt`, Ghostty
    /// `jump_to_prompt`, kitty `scroll_to_prompt`, foot, Windows Terminal).
    Osc133,
}

/// Whether and how to mark sections, for a client writing to a terminal
/// (`tty`). [`MARKS_ENV`] overrides `mode`; `always` writes `OSC 133` where
/// the terminal is not known.
pub fn marks_support(mode: Mode, tty: bool, env: impl Fn(&str) -> Option<String>) -> Option<Marks> {
    let host = gate(mode, tty, MARKS_ENV, &env)?;
    match host.emulator {
        Emulator::Iterm2 => Some(Marks::Iterm2),
        Emulator::WezTerm
        | Emulator::Ghostty
        | Emulator::Kitty
        | Emulator::Foot
        | Emulator::WindowsTerminal => Some(Marks::Osc133),
        _ if host.mode == Mode::Always => Some(Marks::Osc133),
        _ => None,
    }
}

/// [`marks_support`] with the process environment.
pub fn marks_support_from_env(mode: Mode, tty: bool) -> Option<Marks> {
    marks_support(mode, tty, |key| std::env::var(key).ok())
}

impl Marks {
    /// Written right before a section's heading line.
    pub fn before(self) -> &'static str {
        match self {
            Self::Iterm2 => "\x1b]1337;SetMark\x1b\\",
            Self::Osc133 => "\x1b]133;A\x1b\\",
        }
    }

    /// Written right after the heading line, before the section's lines.
    pub fn after_heading(self) -> &'static str {
        match self {
            Self::Iterm2 => "",
            Self::Osc133 => "\x1b]133;C\x1b\\",
        }
    }

    /// `heading` (one line, with its newline) wrapped in the marks.
    pub fn section(self, heading: &str) -> String {
        format!("{}{heading}{}", self.before(), self.after_heading())
    }
}

/// `heading` with marks when `marks` is on, else unchanged.
pub fn marked(marks: Option<Marks>, heading: &str) -> String {
    marks.map_or_else(|| heading.to_owned(), |marks| marks.section(heading))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn support(mode: Mode, tty: bool, pairs: &[(&str, &str)]) -> Option<Marks> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        marks_support(mode, tty, |key| env.get(key).cloned())
    }

    #[test]
    fn sections_are_marked_per_terminal() {
        assert_eq!(
            Marks::Osc133.section("── api ──\n"),
            "\x1b]133;A\x1b\\── api ──\n\x1b]133;C\x1b\\"
        );
        assert_eq!(
            Marks::Iterm2.section("✕ api failed\n"),
            "\x1b]1337;SetMark\x1b\\✕ api failed\n"
        );
        assert_eq!(marked(None, "api\n"), "api\n");
    }

    #[test]
    fn detection_matrix() {
        use Mode::{Always, Auto, Never};
        for (pairs, expected) in [
            (&[("TERM_PROGRAM", "iTerm.app")][..], Some(Marks::Iterm2)),
            (&[("TERM_PROGRAM", "WezTerm")][..], Some(Marks::Osc133)),
            (&[("TERM_PROGRAM", "ghostty")][..], Some(Marks::Osc133)),
            (&[("TERM", "xterm-kitty")][..], Some(Marks::Osc133)),
            (&[("TERM", "foot")][..], Some(Marks::Osc133)),
            (&[("WT_SESSION", "x")][..], Some(Marks::Osc133)),
            (&[("VTE_VERSION", "7800")][..], None),
            (&[("KONSOLE_VERSION", "250800")][..], None),
            (&[("TERM_PROGRAM", "Apple_Terminal")][..], None),
            (&[][..], None),
            (&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/t")][..], None),
            (&[("TERM_PROGRAM", "ghostty"), ("CI", "true")][..], None),
            (&[("TERM_PROGRAM", "ghostty"), (MARKS_ENV, "off")][..], None),
        ] {
            assert_eq!(support(Auto, true, pairs), expected, "{pairs:?}");
        }
        assert_eq!(
            support(Auto, false, &[("TERM_PROGRAM", "ghostty")]),
            None,
            "piped"
        );
        assert_eq!(support(Never, true, &[("TERM_PROGRAM", "ghostty")]), None);
        assert_eq!(support(Always, true, &[]), Some(Marks::Osc133));
    }
}

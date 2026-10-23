//! The terminal tab's title while a client runs: pushed onto the terminal's
//! title stack (`CSI 22;0t`), set with `OSC 0`, and popped back (`CSI 23;0t`)
//! when the guard drops, so the shell's own title returns.
//!
//! `OSC 0` rather than `OSC 2`: iTerm2 labels a tab with the icon name, which
//! only `OSC 0` and `OSC 1` set. Pushing and popping both titles (`;0`) puts
//! back exactly what `OSC 0` changed.

use std::io::{self, Write};

use crate::detect::{Emulator, Mode, gate};

/// Environment override for the config's `terminal.title`: `0` turns the
/// title off, `1` turns it on in any terminal, tmux included.
pub const TITLE_ENV: &str = "DEVLANES_TERM_TITLE";

/// Saves the icon and window titles on the terminal's title stack (XTWINOPS 22).
pub const PUSH_TITLE: &str = "\x1b[22;0t";

/// Restores the titles [`PUSH_TITLE`] saved (XTWINOPS 23).
pub const POP_TITLE: &str = "\x1b[23;0t";

/// Longest title written, in characters; longer ones end in `…`.
pub const MAX_TITLE_CHARS: usize = 80;

/// `text` as one line fit for a title: control characters (C0, DEL, C1, which
/// could end the sequence early or start another) and bidirectional overrides
/// become spaces, runs of spaces collapse, and the result is cut to
/// [`MAX_TITLE_CHARS`].
pub fn sanitize_title(text: &str) -> String {
    one_line(text, MAX_TITLE_CHARS)
}

/// `text` without control characters or bidirectional overrides, on one
/// line, cut to `max` characters with `…`. Shared by every sequence that
/// carries text (titles, notifications, link labels).
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let mut clean = String::with_capacity(text.len());
    for ch in text.chars() {
        let blank = ch.is_control()
            || ch.is_whitespace()
            || matches!(ch, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if !blank {
            clean.push(ch);
        } else if !clean.is_empty() && !clean.ends_with(' ') {
            clean.push(' ');
        }
    }
    let clean = clean.trim_end();
    if clean.chars().count() <= max {
        return clean.to_owned();
    }
    let mut cut: String = clean.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('…');
    cut
}

/// `ESC ] 0 ; <text> ESC \` with `text` sanitized.
pub fn title_sequence(text: &str) -> String {
    format!("\x1b]0;{}\x1b\\", sanitize_title(text))
}

/// Whether to set the title, for a client writing to a terminal (`tty`).
/// [`TITLE_ENV`] overrides `mode`. Auto sets it only where a title stack is
/// known to exist, so the pop on exit puts the shell's title back: xterm,
/// iTerm2, kitty, Ghostty and VTE terminals (GNOME Terminal, Ptyxis, Tilix).
/// It skips WezTerm and Konsole (which parse the stack sequences but ignore
/// them), Windows Terminal and Apple Terminal (no stack), unknown terminals,
/// tmux and screen (which manage titles themselves), CI and dumb terminals.
pub fn title_support(mode: Mode, tty: bool, env: impl Fn(&str) -> Option<String>) -> bool {
    let Some(host) = gate(mode, tty, TITLE_ENV, &env) else {
        return false;
    };
    // Terminals whose sources implement XTWINOPS 22 and 23 for the window title.
    host.mode == Mode::Always
        || matches!(
            host.emulator,
            Emulator::Iterm2
                | Emulator::Kitty
                | Emulator::Ghostty
                | Emulator::Vte(_)
                | Emulator::Xterm
        )
}

/// [`title_support`] with the process environment.
pub fn title_support_from_env(mode: Mode, tty: bool) -> bool {
    title_support(mode, tty, |key| std::env::var(key).ok())
}

/// Sets the terminal's title and puts the previous one back when dropped, so
/// an early return, Ctrl-C handled by the command, or an unwinding panic never
/// leaves it behind. Disabled, it writes nothing, so callers need no special
/// case. It passes other writes through ([`Write`]), so it can share the
/// writer with the progress bar.
pub struct TitleGuard<W: Write> {
    out: W,
    /// The previous title is on the stack and must be popped.
    pushed: bool,
    shown: Option<String>,
}

impl<W: Write> TitleGuard<W> {
    /// A guard on `out`; when `enabled` (from [`title_support`]) the current
    /// title is pushed at once. A failed push disables the guard, so nothing
    /// is set that could not be undone.
    pub fn new(mut out: W, enabled: bool) -> Self {
        let pushed = enabled
            && out
                .write_all(PUSH_TITLE.as_bytes())
                .and_then(|()| out.flush())
                .is_ok();
        Self {
            out,
            pushed,
            shown: None,
        }
    }

    /// Whether titles are written.
    pub fn enabled(&self) -> bool {
        self.pushed
    }

    /// The title set last, sanitized.
    pub fn shown(&self) -> Option<&str> {
        self.shown.as_deref()
    }

    /// Shows `text` when it differs from the title shown; true when written.
    /// An empty title is never set, as it would blank the tab.
    pub fn set(&mut self, text: &str) -> io::Result<bool> {
        let clean = sanitize_title(text);
        if !self.pushed || clean.is_empty() || self.shown.as_deref() == Some(clean.as_str()) {
            return Ok(false);
        }
        self.out
            .write_all(format!("\x1b]0;{clean}\x1b\\").as_bytes())?;
        self.out.flush()?;
        self.shown = Some(clean);
        Ok(true)
    }

    /// Puts the previous title back; later calls and [`TitleGuard::set`] do nothing.
    pub fn restore(&mut self) -> io::Result<()> {
        if !std::mem::take(&mut self.pushed) {
            return Ok(());
        }
        self.shown = None;
        self.out.write_all(POP_TITLE.as_bytes())?;
        self.out.flush()
    }

    /// The writer.
    pub fn get_mut(&mut self) -> &mut W {
        &mut self.out
    }
}

impl<W: Write> Write for TitleGuard<W> {
    /// Passes other output through to the writer.
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.out.write(buf)
    }

    /// Flushes the writer.
    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl<W: Write> Drop for TitleGuard<W> {
    /// Pops the title still pushed; a write error has nowhere to go here.
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::{Arc, Mutex},
    };

    #[test]
    fn sequences_push_set_and_pop_both_titles() {
        assert_eq!(PUSH_TITLE, "\x1b[22;0t");
        assert_eq!(POP_TITLE, "\x1b[23;0t");
        assert_eq!(
            title_sequence("devlanes · acme@0"),
            "\x1b]0;devlanes · acme@0\x1b\\"
        );
    }

    #[test]
    fn titles_lose_control_characters_and_long_tails() {
        // BEL or ST inside the text would end the sequence early; ESC could
        // start another one; C1 controls act like ESC sequences in some terminals.
        assert_eq!(
            sanitize_title("api\x07\x1b]0;evil\x1b\\ \u{9c}x"),
            "api ]0;evil \\ x"
        );
        assert_eq!(sanitize_title("  a\n\tb \r\n c  "), "a b c");
        assert_eq!(sanitize_title("a\u{202e}b\u{2066}c\u{7f}"), "a b c");
        assert_eq!(sanitize_title("\x1b\x07"), "");
        let long = "x".repeat(200);
        let cut = sanitize_title(&long);
        assert_eq!(cut.chars().count(), MAX_TITLE_CHARS);
        assert!(cut.ends_with('…'));
        // Characters, not bytes: a multibyte title is never split inside one.
        let wide = sanitize_title(&"é".repeat(200));
        assert_eq!(wide.chars().count(), MAX_TITLE_CHARS);
        assert_eq!(sanitize_title(&"a ".repeat(40)), "a ".repeat(40).trim_end());
    }

    /// Support for an environment given as `KEY=value` pairs.
    fn support(mode: Mode, tty: bool, pairs: &[(&str, &str)]) -> bool {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        title_support(mode, tty, |key| env.get(key).cloned())
    }

    #[test]
    fn detection_matrix() {
        use Mode::{Always, Auto, Never};
        let iterm = [("TERM_PROGRAM", "iTerm.app")];
        for (pairs, expected) in [
            // Terminals whose sources keep a title stack.
            (&iterm[..], true),
            (&[("TERM_PROGRAM", "ghostty")][..], true),
            (&[("TERM", "xterm-ghostty")][..], true),
            (
                &[("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")][..],
                true,
            ),
            (&[("KITTY_WINDOW_ID", "3")][..], true),
            (&[("VTE_VERSION", "7800")][..], true),
            (&[("XTERM_VERSION", "XTerm(397)")][..], true),
            // Parse the sequences but keep no stack, or have none at all.
            (&[("TERM_PROGRAM", "WezTerm")][..], false),
            (&[("TERM", "wezterm")][..], false),
            (&[("KONSOLE_VERSION", "250800")][..], false),
            (&[("WT_SESSION", "abc")][..], false),
            (&[("TERM_PROGRAM", "Apple_Terminal")][..], false),
            (&[("TERM_PROGRAM", "vscode")][..], false),
            (&[("TERM", "xterm-256color")][..], false),
            (&[][..], false),
            // A terminal started from another inherits its VTE_VERSION; its own
            // TERM_PROGRAM decides.
            (
                &[("TERM_PROGRAM", "WezTerm"), ("VTE_VERSION", "7800")][..],
                false,
            ),
            (&[("VTE_VERSION", "7800"), ("WT_SESSION", "x")][..], false),
            // tmux and screen manage titles themselves.
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("TMUX", "/tmp/tmux-1/default,1,0"),
                ][..],
                false,
            ),
            (
                &[("TERM_PROGRAM", "iTerm.app"), ("STY", "1.pts-0")][..],
                false,
            ),
            (&[("TERM_PROGRAM", "iTerm.app"), ("CI", "true")][..], false),
            (
                &[("TERM_PROGRAM", "iTerm.app"), ("GITHUB_ACTIONS", "true")][..],
                false,
            ),
            (&[("TERM_PROGRAM", "iTerm.app"), ("CI", "0")][..], true),
            (
                &[("XTERM_VERSION", "XTerm(397)"), ("TERM", "dumb")][..],
                false,
            ),
            // Colors are another matter.
            (
                &[("TERM_PROGRAM", "iTerm.app"), ("NO_COLOR", "1")][..],
                true,
            ),
        ] {
            assert_eq!(support(Auto, true, pairs), expected, "{pairs:?}");
        }
        // Piped output never gets it, whatever the settings.
        assert!(!support(Auto, false, &iterm));
        assert!(!support(Always, false, &[(TITLE_ENV, "1")]));
        // The setting and the override; always includes tmux, which keeps its
        // own title stack for the pane.
        assert!(!support(Never, true, &iterm));
        assert!(support(Always, true, &[]));
        assert!(support(Always, true, &[("TMUX", "/tmp/tmux")]));
        assert!(support(
            Auto,
            true,
            &[(TITLE_ENV, "1"), ("TMUX", "/tmp/tmux")]
        ));
        assert!(!support(Always, true, &[(TITLE_ENV, "0")]));
        assert!(!support(
            Auto,
            true,
            &[("TERM_PROGRAM", "iTerm.app"), (TITLE_ENV, "off")]
        ));
        assert!(support(
            Auto,
            true,
            &[("TERM_PROGRAM", "iTerm.app"), (TITLE_ENV, "")]
        ));
    }

    /// A writer tests can read after the guard that owns a clone is gone.
    #[derive(Clone, Default)]
    struct Shared(Arc<Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("poisoned"))?
                .extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            self.0
                .lock()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                .unwrap_or_default()
        }
    }

    #[test]
    fn the_guard_pushes_sets_changes_only_and_pops_on_drop() -> io::Result<()> {
        let out = Shared::default();
        {
            let mut title = TitleGuard::new(out.clone(), true);
            assert_eq!(out.text(), PUSH_TITLE);
            assert!(title.set("devlanes · acme@0 · starting 1/3")?);
            assert!(!title.set("devlanes · acme@0 · starting 1/3")?, "unchanged");
            assert!(title.set("devlanes · acme@0")?);
            assert!(!title.set(" \x07 ")?, "an empty title is never set");
            assert_eq!(title.shown(), Some("devlanes · acme@0"));
            // Other output passes through.
            title.write_all(b"copy")?;
        }
        assert_eq!(
            out.text(),
            format!(
                "{PUSH_TITLE}\x1b]0;devlanes · acme@0 · starting 1/3\x1b\\\x1b]0;devlanes · acme@0\x1b\\copy{POP_TITLE}"
            )
        );
        Ok(())
    }

    #[test]
    fn the_guard_pops_on_panic_and_once() -> io::Result<()> {
        let out = Shared::default();
        let writer = out.clone();
        let result = std::panic::catch_unwind(move || {
            let mut title = TitleGuard::new(writer, true);
            let _ = title.set("devlanes logs -f api");
            panic!("boom");
        });
        assert!(result.is_err());
        assert!(out.text().ends_with(POP_TITLE), "{:?}", out.text());

        // An explicit restore pops once; the drop after it writes nothing more.
        let out = Shared::default();
        let mut title = TitleGuard::new(out.clone(), true);
        title.restore()?;
        assert!(!title.set("late")?, "nothing is set after the pop");
        drop(title);
        assert_eq!(out.text(), format!("{PUSH_TITLE}{POP_TITLE}"));
        Ok(())
    }

    #[test]
    fn a_disabled_or_failed_guard_writes_nothing() -> io::Result<()> {
        let out = Shared::default();
        let mut off = TitleGuard::new(out.clone(), false);
        assert!(!off.enabled());
        assert!(!off.set("devlanes")?);
        drop(off);
        assert_eq!(out.text(), "");

        /// Refuses every write, as a closed terminal does.
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> io::Result<usize> {
                Err(io::Error::other("closed"))
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut failed = TitleGuard::new(Closed, true);
        assert!(!failed.enabled(), "no push, so nothing to undo");
        assert!(!failed.set("devlanes")?);
        Ok(())
    }
}

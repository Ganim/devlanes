//! Desktop notifications through the terminal: a long start finished or
//! failed, a running service crashed. Each terminal gets the sequence it
//! implements, and BEL where none is known.
//!
//! Clients notify only while the terminal is not focused. The TUI knows it
//! from focus reports; a command does not, so it notifies only where the
//! terminal itself holds a notification back from a focused window (kitty's
//! `o=unfocused`), unless the user chose `always`.

use std::{
    sync::atomic::{AtomicU32, Ordering},
    time::{Duration, Instant},
};

use base64::Engine;

use crate::{
    Wrap,
    detect::{Emulator, Mode, gate},
    title::one_line,
};

/// Environment override for the config's `terminal.notifications`: `0` turns
/// them off, `1` sends them in any terminal.
pub const NOTIFICATIONS_ENV: &str = "DEVLANES_TERM_NOTIFICATIONS";

/// Notifications closer together than this are dropped.
pub const NOTIFY_GAP: Duration = Duration::from_secs(10);

/// A start must run at least this long before its end is worth a notification.
pub const LONG_START: Duration = Duration::from_secs(10);

/// The sequence a terminal turns into a desktop notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyStyle {
    /// `OSC 9 ; message` (iTerm2, foot).
    Osc9,
    /// `OSC 99 ; metadata ; payload` (kitty), shown only while unfocused.
    Osc99,
    /// `OSC 777 ; notify ; title ; body` (Ghostty, WezTerm, VTE terminals that
    /// carry the notify patch, as Fedora's and Ubuntu's do).
    Osc777,
    /// The bell: a terminal without notifications still marks the tab or
    /// bounces the dock icon, per its own settings.
    Bel,
}

/// How notifications are sent here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Notifications {
    pub style: NotifyStyle,
    wrap: Wrap,
    /// The user chose `always` (or `DEVLANES_TERM_NOTIFICATIONS=1`).
    always: bool,
}

/// Whether and how to notify, for a client writing to a terminal (`tty`).
/// [`NOTIFICATIONS_ENV`] overrides `mode`. Any terminal gets at least BEL;
/// inside tmux (`always` only) the sequence goes through passthrough, which
/// tmux forwards only with `allow-passthrough on`.
pub fn notifications_support(
    mode: Mode,
    tty: bool,
    env: impl Fn(&str) -> Option<String>,
) -> Option<Notifications> {
    let host = gate(mode, tty, NOTIFICATIONS_ENV, &env)?;
    let style = match host.emulator {
        Emulator::Kitty => NotifyStyle::Osc99,
        Emulator::Iterm2 | Emulator::Foot => NotifyStyle::Osc9,
        // OSC 777 rather than 9: Ghostty reads `OSC 9 ; <digit> ;` as a ConEmu
        // command, and 777 carries a title.
        Emulator::Ghostty | Emulator::WezTerm | Emulator::Vte(_) => NotifyStyle::Osc777,
        _ => NotifyStyle::Bel,
    };
    let wrap = if host.tmux && style != NotifyStyle::Bel {
        Wrap::Tmux
    } else {
        Wrap::Bare
    };
    Some(Notifications {
        style,
        wrap,
        always: host.mode == Mode::Always,
    })
}

/// [`notifications_support`] with the process environment.
pub fn notifications_support_from_env(mode: Mode, tty: bool) -> Option<Notifications> {
    notifications_support(mode, tty, |key| std::env::var(key).ok())
}

impl Notifications {
    /// Notifications in `style`, written bare, as auto would send them.
    pub fn new(style: NotifyStyle) -> Self {
        Self {
            style,
            wrap: Wrap::Bare,
            always: false,
        }
    }

    /// Whether a client that cannot see focus (a command) may notify: where
    /// the terminal itself skips a focused window, or when the user asked for
    /// notifications always.
    pub fn without_focus(&self) -> bool {
        self.always || self.style == NotifyStyle::Osc99
    }

    /// The bytes that show `title` and `body`, both cut to one line.
    pub fn sequence(&self, title: &str, body: &str) -> String {
        let title = one_line(title, 80);
        let body = one_line(body, 200);
        let sequence = match self.style {
            NotifyStyle::Osc9 => {
                // Always `devlanes: …`, so the text can never read as `9;4`
                // progress or another ConEmu `9;N` command.
                let message = if body.is_empty() {
                    format!("devlanes: {title}")
                } else {
                    format!("devlanes: {title}: {body}")
                };
                format!("\x1b]9;{message}\x1b\\")
            }
            NotifyStyle::Osc99 => {
                static NEXT: AtomicU32 = AtomicU32::new(1);
                let id = NEXT.fetch_add(1, Ordering::Relaxed);
                let encode = |text: &str| base64::engine::general_purpose::STANDARD.encode(text);
                let app = encode("devlanes");
                let mut sequence = format!(
                    "\x1b]99;i=devlanes-{id}:d=0:o=unfocused:f={app}:e=1;{}\x1b\\",
                    encode(&title)
                );
                sequence.push_str(&format!(
                    "\x1b]99;i=devlanes-{id}:d=1:p=body:e=1;{}\x1b\\",
                    encode(&body)
                ));
                sequence
            }
            NotifyStyle::Osc777 => {
                // The title ends at the next `;`.
                let title = format!("devlanes: {}", title.replace(';', ","));
                format!("\x1b]777;notify;{title};{body}\x1b\\")
            }
            NotifyStyle::Bel => "\x07".to_owned(),
        };
        match self.style {
            // Each kitty sequence travels on its own through tmux.
            NotifyStyle::Osc99 if self.wrap == Wrap::Tmux => sequence
                .split_inclusive("\x1b\\")
                .map(|part| self.wrap.apply(part))
                .collect(),
            _ => self.wrap.apply(&sequence),
        }
    }
}

/// Drops notifications that follow the last one within [`NOTIFY_GAP`], so a
/// lane whose services crash together sends one, not ten.
#[derive(Debug, Clone)]
pub struct Notifier {
    gap: Duration,
    last: Option<Instant>,
}

impl Default for Notifier {
    /// A limiter with [`NOTIFY_GAP`].
    fn default() -> Self {
        Self::new(NOTIFY_GAP)
    }
}

impl Notifier {
    /// A limiter that allows one notification per `gap`.
    pub fn new(gap: Duration) -> Self {
        Self { gap, last: None }
    }

    /// Whether a notification at `now` may go out; records it when it may.
    pub fn allow(&mut self, now: Instant) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < self.gap)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn support(mode: Mode, tty: bool, pairs: &[(&str, &str)]) -> Option<Notifications> {
        let env: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        notifications_support(mode, tty, |key| env.get(key).cloned())
    }

    #[test]
    fn each_style_encodes_title_and_body() {
        assert_eq!(
            Notifications::new(NotifyStyle::Osc9).sequence("acme@0 is up", "8 of 8 ready"),
            "\x1b]9;devlanes: acme@0 is up: 8 of 8 ready\x1b\\"
        );
        // A lane named like a ConEmu command still reads as a message.
        assert!(
            Notifications::new(NotifyStyle::Osc9)
                .sequence("4;1;50", "")
                .starts_with("\x1b]9;devlanes: 4;1;50")
        );
        assert_eq!(
            Notifications::new(NotifyStyle::Osc777).sequence("api; crashed", "exit 1"),
            "\x1b]777;notify;devlanes: api, crashed;exit 1\x1b\\"
        );
        assert_eq!(
            Notifications::new(NotifyStyle::Bel).sequence("x", "y"),
            "\x07"
        );
        let kitty = Notifications::new(NotifyStyle::Osc99).sequence("api crashed", "exit 1");
        let parts: Vec<&str> = kitty.split_inclusive("\x1b\\").collect();
        assert_eq!(parts.len(), 2, "{kitty:?}");
        assert!(parts[0].contains(":d=0:o=unfocused:"), "{kitty:?}");
        assert!(parts[0].ends_with(";YXBpIGNyYXNoZWQ=\x1b\\"), "{kitty:?}");
        assert!(
            parts[1].contains(":d=1:p=body:e=1;ZXhpdCAx\x1b\\"),
            "{kitty:?}"
        );
        // Control characters never reach the terminal.
        let text = Notifications::new(NotifyStyle::Osc9).sequence("a\x1b]0;x\x07b", "");
        assert_eq!(text.matches('\x1b').count(), 2, "{text:?}");
        assert!(!text.contains('\x07'));
    }

    #[test]
    fn detection_matrix() {
        use Mode::{Always, Auto, Never};
        for (pairs, expected) in [
            (&[("TERM", "xterm-kitty")][..], Some(NotifyStyle::Osc99)),
            (
                &[("TERM_PROGRAM", "iTerm.app")][..],
                Some(NotifyStyle::Osc9),
            ),
            // Old iTerm2 too: OSC 9 has meant a notification there for years;
            // only 9;4 needs 3.6.6, and the message never starts with 4;.
            (
                &[
                    ("TERM_PROGRAM", "iTerm.app"),
                    ("TERM_PROGRAM_VERSION", "3.4.0"),
                ][..],
                Some(NotifyStyle::Osc9),
            ),
            (&[("TERM", "foot")][..], Some(NotifyStyle::Osc9)),
            (
                &[("TERM_PROGRAM", "ghostty")][..],
                Some(NotifyStyle::Osc777),
            ),
            (
                &[("TERM_PROGRAM", "WezTerm")][..],
                Some(NotifyStyle::Osc777),
            ),
            (&[("VTE_VERSION", "7800")][..], Some(NotifyStyle::Osc777)),
            (&[("WT_SESSION", "x")][..], Some(NotifyStyle::Bel)),
            (
                &[("TERM_PROGRAM", "Apple_Terminal")][..],
                Some(NotifyStyle::Bel),
            ),
            (&[][..], Some(NotifyStyle::Bel)),
            (&[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/t")][..], None),
            (&[("TERM_PROGRAM", "ghostty"), ("CI", "true")][..], None),
            (
                &[("TERM_PROGRAM", "ghostty"), (NOTIFICATIONS_ENV, "0")][..],
                None,
            ),
        ] {
            assert_eq!(
                support(Auto, true, pairs).map(|n| n.style),
                expected,
                "{pairs:?}"
            );
        }
        assert_eq!(
            support(Auto, false, &[("TERM", "xterm-kitty")]),
            None,
            "piped"
        );
        assert_eq!(support(Never, true, &[("TERM", "xterm-kitty")]), None);
        // Inside tmux only with always, and then through passthrough.
        let tmux = support(
            Always,
            true,
            &[("TERM_PROGRAM", "ghostty"), ("TMUX", "/tmp/t")],
        )
        .map(|n| n.sequence("x", ""));
        assert_eq!(
            tmux.as_deref(),
            Some("\x1bPtmux;\x1b\x1b]777;notify;devlanes: x;\x1b\x1b\\\x1b\\")
        );
        // Without focus reports only kitty (or always) may notify.
        assert!(support(Auto, true, &[("TERM", "xterm-kitty")]).is_some_and(|n| n.without_focus()));
        assert!(
            support(Auto, true, &[("TERM_PROGRAM", "ghostty")]).is_some_and(|n| !n.without_focus())
        );
        assert!(
            support(Always, true, &[("TERM_PROGRAM", "ghostty")])
                .is_some_and(|n| n.without_focus())
        );
    }

    #[test]
    fn the_notifier_drops_bursts() {
        let start = Instant::now();
        let mut notifier = Notifier::new(Duration::from_secs(10));
        assert!(notifier.allow(start));
        assert!(!notifier.allow(start + Duration::from_secs(3)));
        assert!(!notifier.allow(start + Duration::from_secs(9)));
        assert!(notifier.allow(start + Duration::from_secs(10)));
    }
}

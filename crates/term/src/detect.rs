//! Which terminal a client writes to, from its environment, and the gate every
//! terminal feature (title, links, notifications, marks) passes first: only a
//! terminal, then the user's setting and its environment override, then, in
//! auto mode, never inside tmux, screen, CI or a dumb terminal.
//!
//! What each terminal supports was read from its own documentation or source;
//! the README of this crate lists the sources.

/// The user's setting for one terminal feature (`terminal.title`,
/// `terminal.links`, `terminal.notifications`, `terminal.marks`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    /// Only in terminals known to support the feature, never inside tmux,
    /// screen, CI or a dumb terminal.
    #[default]
    Auto,
    /// In any terminal, tmux included.
    Always,
    /// Never.
    Never,
}

/// A terminal emulator recognized from the variables it exports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emulator {
    Iterm2,
    Kitty,
    WezTerm,
    Ghostty,
    WindowsTerminal,
    /// GNOME Terminal, Ptyxis, Tilix and other VTE terminals, with `VTE_VERSION`
    /// (`7800` is 0.78).
    Vte(u32),
    Konsole,
    Alacritty,
    Foot,
    Xterm,
    AppleTerminal,
    VsCode,
    Unknown,
}

/// The terminal behind `env`. A terminal that sets `TERM_PROGRAM` or its own
/// `TERM` wins over variables inherited from the terminal it was started from
/// (`VTE_VERSION`, `XTERM_VERSION`, `KONSOLE_VERSION`).
pub fn identify(env: &impl Fn(&str) -> Option<String>) -> Emulator {
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    match env("TERM_PROGRAM").as_deref() {
        Some("iTerm.app") => return Emulator::Iterm2,
        Some("ghostty") => return Emulator::Ghostty,
        Some("WezTerm") => return Emulator::WezTerm,
        Some("Apple_Terminal") => return Emulator::AppleTerminal,
        Some("vscode") => return Emulator::VsCode,
        _ => {}
    }
    match env("TERM").as_deref() {
        Some("xterm-kitty") => return Emulator::Kitty,
        Some("xterm-ghostty") => return Emulator::Ghostty,
        Some("wezterm") => return Emulator::WezTerm,
        Some("alacritty") => return Emulator::Alacritty,
        Some("foot" | "foot-extra") => return Emulator::Foot,
        _ => {}
    }
    if set("WT_SESSION") {
        Emulator::WindowsTerminal
    } else if set("KITTY_WINDOW_ID") {
        Emulator::Kitty
    } else if set("ALACRITTY_WINDOW_ID") {
        Emulator::Alacritty
    } else if set("KONSOLE_VERSION") {
        Emulator::Konsole
    } else if let Some(version) = env("VTE_VERSION").filter(|v| !v.is_empty()) {
        Emulator::Vte(version.trim().parse().unwrap_or(0))
    } else if set("XTERM_VERSION") {
        Emulator::Xterm
    } else {
        Emulator::Unknown
    }
}

/// What the gate found out about where the output goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Host {
    pub emulator: Emulator,
    /// The mode after the environment override.
    pub mode: Mode,
    /// Inside tmux (only reached with `always`).
    pub tmux: bool,
}

/// `mode` with `override_key` applied: `0`, `false`, `off`, `never` turn the
/// feature off, `1`, `true`, `on`, `always` turn it on in any terminal.
pub fn resolve_mode(mode: Mode, override_key: &str, env: &impl Fn(&str) -> Option<String>) -> Mode {
    match env(override_key).as_deref().map(str::trim) {
        Some("0" | "false" | "off" | "never") => Mode::Never,
        Some("1" | "true" | "on" | "always") => Mode::Always,
        _ => mode,
    }
}

/// The shared gate: `None` when the feature stays off for output to `tty`
/// under `mode` and `override_key`, else the terminal to decide the rest by.
pub fn gate(
    mode: Mode,
    tty: bool,
    override_key: &str,
    env: &impl Fn(&str) -> Option<String>,
) -> Option<Host> {
    if !tty {
        return None;
    }
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    let mode = resolve_mode(mode, override_key, env);
    let tmux = set("TMUX");
    match mode {
        Mode::Never => return None,
        Mode::Always => {}
        Mode::Auto => {
            let dumb = env("TERM").as_deref() == Some("dumb");
            if tmux || set("STY") || dumb || crate::progress::in_ci(env) {
                return None;
            }
        }
    }
    Some(Host {
        emulator: identify(env),
        mode,
        tmux,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |key| map.get(key).cloned()
    }

    #[test]
    fn terminals_are_told_apart() {
        for (pairs, expected) in [
            (&[("TERM_PROGRAM", "iTerm.app")][..], Emulator::Iterm2),
            (&[("TERM_PROGRAM", "ghostty")][..], Emulator::Ghostty),
            (&[("TERM", "xterm-ghostty")][..], Emulator::Ghostty),
            (&[("TERM_PROGRAM", "WezTerm")][..], Emulator::WezTerm),
            (&[("TERM", "xterm-kitty")][..], Emulator::Kitty),
            (&[("KITTY_WINDOW_ID", "1")][..], Emulator::Kitty),
            (&[("TERM", "alacritty")][..], Emulator::Alacritty),
            (&[("TERM", "foot")][..], Emulator::Foot),
            (&[("WT_SESSION", "x")][..], Emulator::WindowsTerminal),
            (&[("KONSOLE_VERSION", "250800")][..], Emulator::Konsole),
            (&[("VTE_VERSION", "7800")][..], Emulator::Vte(7800)),
            (&[("XTERM_VERSION", "XTerm(397)")][..], Emulator::Xterm),
            (
                &[("TERM_PROGRAM", "Apple_Terminal")][..],
                Emulator::AppleTerminal,
            ),
            (&[("TERM_PROGRAM", "vscode")][..], Emulator::VsCode),
            (&[("TERM", "xterm-256color")][..], Emulator::Unknown),
            // Started from GNOME Terminal, a terminal that names itself wins.
            (
                &[("VTE_VERSION", "7800"), ("TERM_PROGRAM", "WezTerm")][..],
                Emulator::WezTerm,
            ),
            (
                &[("VTE_VERSION", "7800"), ("TERM", "xterm-kitty")][..],
                Emulator::Kitty,
            ),
        ] {
            assert_eq!(identify(&env(pairs)), expected, "{pairs:?}");
        }
    }

    #[test]
    fn the_gate_needs_a_terminal_and_respects_the_setting() {
        let iterm = [("TERM_PROGRAM", "iTerm.app")];
        let key = "DEVLANES_TERM_X";
        assert!(gate(Mode::Auto, false, key, &env(&iterm)).is_none());
        assert!(gate(Mode::Always, false, key, &env(&[(key, "1")])).is_none());
        assert!(gate(Mode::Never, true, key, &env(&iterm)).is_none());
        assert!(gate(Mode::Auto, true, key, &env(&iterm)).is_some());
        for blocker in [
            ("TMUX", "/tmp/tmux-1/default,1,0"),
            ("STY", "1.pts-0"),
            ("TERM", "dumb"),
            ("CI", "true"),
            ("GITHUB_ACTIONS", "true"),
        ] {
            let pairs = [iterm[0], blocker];
            assert!(
                gate(Mode::Auto, true, key, &env(&pairs)).is_none(),
                "{blocker:?}"
            );
            assert!(
                gate(Mode::Always, true, key, &env(&pairs)).is_some(),
                "{blocker:?}"
            );
        }
        let tmux = gate(Mode::Always, true, key, &env(&[("TMUX", "/tmp/t")]));
        assert_eq!(tmux.map(|h| h.tmux), Some(true));
        assert!(gate(Mode::Auto, true, key, &env(&[(key, "0"), iterm[0]])).is_none());
        assert_eq!(
            gate(Mode::Never, true, key, &env(&[(key, "on")])).map(|h| h.mode),
            Some(Mode::Always)
        );
        // NO_COLOR is about colors, not about these sequences.
        assert!(gate(Mode::Auto, true, key, &env(&[iterm[0], ("NO_COLOR", "1")])).is_some());
    }
}

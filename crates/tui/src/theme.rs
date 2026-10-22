//! Colors and glyphs. One tier is picked at startup: no color (`NO_COLOR`),
//! the 16 ANSI colors (the default, which follows the user's terminal theme),
//! or true color for a known dark or light background. State has its own
//! glyph shape in every tier, so it survives no color at all.

use ratatui::style::{Color, Modifier, Style};

/// How much color the terminal gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tier {
    /// `NO_COLOR`: shapes, words, bold and reverse video only.
    Plain,
    /// ANSI names only; they follow the terminal's own theme.
    #[default]
    Ansi,
    /// True color on a dark background.
    Dark,
    /// True color on a light background.
    Light,
}

/// Which characters draw states, bars and markers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Glyphs {
    #[default]
    Unicode,
    /// For terminals and fonts without the Unicode shapes (`ui.glyphs: ascii`,
    /// a locale that is not UTF-8, or a classic Windows console window).
    Ascii,
}

/// What a color means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    /// Ready, running, up, done.
    Ok,
    /// Failed units and actions, exit codes.
    Failure,
    /// Orphaned processes, missing worktrees, warnings, version skew.
    Attention,
    /// Starting, stopping, running tasks: default text, the spinner shows motion.
    Busy,
    /// Stopped, not run, labels and secondary text: dimmed.
    Quiet,
}

/// The shapes the UI draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Glyph {
    Ready,
    /// Running without a ready check.
    Running,
    Queued,
    Failed,
    Exited,
    Stopped,
    Done,
    Orphaned,
    /// The unit's color, one cell at the left of its row.
    Bar,
    /// Focus and selection, column 0.
    Marker,
    Open,
    Folded,
    /// Before an attention note.
    Attention,
}

/// The tier and glyph set in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    pub tier: Tier,
    pub glyphs: Glyphs,
}

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const ASCII_SPINNER: [&str; 4] = ["-", "\\", "|", "/"];

impl Theme {
    /// Picks the tier and glyphs from the environment and the user config.
    /// `theme` is `tui.theme` (`auto`, `dark`, `light` or `ansi`), `glyphs` is
    /// `tui.glyphs` (`auto`, `unicode` or `ascii`). `classic_console` is
    /// [`classic_console`]'s answer; auto glyphs are ASCII there.
    pub fn detect(
        theme: Option<&str>,
        glyphs: Option<&str>,
        env: impl Fn(&str) -> Option<String>,
        classic_console: bool,
    ) -> Self {
        let set = |key: &str| env(key).filter(|value| !value.is_empty());
        let tier = if set("NO_COLOR").is_some() {
            Tier::Plain
        } else {
            match theme.map(str::trim) {
                Some("dark") => Tier::Dark,
                Some("light") => Tier::Light,
                Some("ansi") => Tier::Ansi,
                _ => {
                    let truecolor = set("COLORTERM")
                        .is_some_and(|value| matches!(value.as_str(), "truecolor" | "24bit"));
                    match set("COLORFGBG").as_deref().and_then(background) {
                        Some(dark) if truecolor => {
                            if dark {
                                Tier::Dark
                            } else {
                                Tier::Light
                            }
                        }
                        // The TUI never assumes a dark background.
                        _ => Tier::Ansi,
                    }
                }
            }
        };
        let glyphs = match glyphs.map(str::trim) {
            Some("ascii") => Glyphs::Ascii,
            Some("unicode") => Glyphs::Unicode,
            // Whatever the locale says, conhost's fonts lack the shapes.
            _ if classic_console => Glyphs::Ascii,
            _ => {
                let locale = ["LC_ALL", "LC_CTYPE", "LANG"].into_iter().find_map(set);
                match locale {
                    Some(locale) => {
                        let locale = locale.to_lowercase();
                        if locale.contains("utf-8") || locale.contains("utf8") {
                            Glyphs::Unicode
                        } else {
                            Glyphs::Ascii
                        }
                    }
                    None => Glyphs::Unicode,
                }
            }
        };
        Self { tier, glyphs }
    }

    fn rgb(&self, dark: (u8, u8, u8), light: (u8, u8, u8), ansi: Color) -> Option<Color> {
        match self.tier {
            Tier::Plain => None,
            Tier::Ansi => Some(ansi),
            Tier::Dark => Some(Color::Rgb(dark.0, dark.1, dark.2)),
            Tier::Light => Some(Color::Rgb(light.0, light.1, light.2)),
        }
    }

    /// The style for a state's glyph or word.
    pub fn tone(&self, tone: Tone) -> Style {
        let color = match tone {
            Tone::Ok => self.rgb((0x4c, 0xbf, 0x7f), (0x1d, 0x7a, 0x46), Color::Green),
            Tone::Failure => self.rgb((0xe5, 0x58, 0x4a), (0xc0, 0x39, 0x2b), Color::Red),
            Tone::Attention => self.rgb((0xe8, 0xa1, 0x3c), (0xa3, 0x5a, 0x00), Color::Yellow),
            Tone::Busy => None,
            Tone::Quiet => return self.quiet(),
        };
        color.map_or_else(Style::new, |color| Style::new().fg(color))
    }

    /// Secondary text: dimmed default text, so it follows either background.
    pub fn quiet(&self) -> Style {
        Style::new().add_modifier(Modifier::DIM)
    }

    /// The separator column and dashed rules.
    pub fn rule(&self) -> Style {
        self.rgb((0x6b, 0x67, 0x5e), (0x8f, 0x8a, 0x7f), Color::DarkGray)
            .map_or_else(|| self.quiet(), |color| Style::new().fg(color))
    }

    /// Focus: the panel marker, the active tab, a dialog's border. Paint in
    /// true color, bold default text otherwise.
    pub fn focus(&self) -> Style {
        match self.tier {
            Tier::Dark => Style::new().fg(Color::Rgb(0xf5, 0xc5, 0x42)),
            Tier::Light => Style::new().fg(Color::Rgb(0x9a, 0x6b, 0x00)),
            Tier::Plain | Tier::Ansi => Style::new().add_modifier(Modifier::BOLD),
        }
    }

    /// The selected row while the list has the keys: a paint background in
    /// true color, reverse video otherwise.
    pub fn selection(&self) -> Style {
        match self.tier {
            Tier::Dark => Style::new().bg(Color::Rgb(0x3b, 0x34, 0x23)),
            Tier::Light => Style::new().bg(Color::Rgb(0xf7, 0xe7, 0xb4)),
            Tier::Plain | Tier::Ansi => Style::new().add_modifier(Modifier::REVERSED),
        }
    }

    /// True when the selection is drawn in reverse video, which recolors
    /// every cell of the row anyway.
    pub fn reverse_selection(&self) -> bool {
        matches!(self.tier, Tier::Plain | Tier::Ansi)
    }

    /// `style` inside a selected row. In true color glyphs keep their color
    /// on the selection background; reverse video would turn a colored glyph
    /// into a colored block, so in 16 colors the row is default fg.
    pub fn selected(&self, style: Style) -> Style {
        let base = if self.reverse_selection() {
            let mut plain = style.remove_modifier(Modifier::DIM);
            plain.fg = None;
            plain.bg = None;
            plain
        } else {
            style
        };
        base.patch(self.selection())
    }

    /// The active project tab: paint behind dark text, reverse video
    /// without color.
    pub fn tab(&self) -> Style {
        let style = match self.tier {
            Tier::Dark | Tier::Light => Style::new()
                .bg(Color::Rgb(0xf5, 0xc5, 0x42))
                .fg(Color::Rgb(0x1a, 0x19, 0x17)),
            Tier::Ansi => Style::new().bg(Color::Yellow).fg(Color::Black),
            Tier::Plain => Style::new().add_modifier(Modifier::REVERSED),
        };
        style.add_modifier(Modifier::BOLD)
    }

    /// The color of letter `index` of `count` in the `devlanes` title: paint
    /// yellow to orange. 16-color terminals get the 256-color cube's yellows
    /// and oranges, which every terminal that draws this TUI supports.
    pub fn brand(&self, index: usize, count: usize) -> Style {
        let t = index as f64 / count.saturating_sub(1).max(1) as f64;
        let mix = |a: (u8, u8, u8), b: (u8, u8, u8)| {
            let channel =
                |x: u8, y: u8| (f64::from(x) + (f64::from(y) - f64::from(x)) * t).round() as u8;
            Color::Rgb(channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
        };
        let color = match self.tier {
            Tier::Plain => return Style::new().add_modifier(Modifier::BOLD),
            Tier::Dark => mix((0xf5, 0xc5, 0x42), (0xe8, 0x8a, 0x3c)),
            Tier::Light => mix((0x9a, 0x6b, 0x00), (0xb3, 0x5c, 0x00)),
            // 220 gold, 214 orange-yellow, 208 orange.
            Tier::Ansi => {
                Color::Indexed([220, 220, 221, 214, 214, 215, 208, 208][(t * 7.0).round() as usize])
            }
        };
        Style::new().fg(color).add_modifier(Modifier::BOLD)
    }

    /// Keys in the footer, dialogs and help.
    pub fn key(&self) -> Style {
        Style::new().add_modifier(Modifier::BOLD)
    }

    /// A unit's palette color (`devlanes_core::color::PALETTE`) as the
    /// terminal's own ANSI color; `None` without color or for unknown names.
    pub fn identity(&self, name: &str) -> Option<Color> {
        if self.tier == Tier::Plain {
            return None;
        }
        palette_color(name)
    }

    /// The characters for `glyph`.
    pub fn glyph(&self, glyph: Glyph) -> &'static str {
        let ascii = self.glyphs == Glyphs::Ascii;
        let pick = |unicode: &'static str, plain: &'static str| if ascii { plain } else { unicode };
        match glyph {
            Glyph::Ready => pick("●", "*"),
            Glyph::Running => pick("◉", "o"),
            Glyph::Queued => pick("…", "."),
            Glyph::Failed => pick("✕", "x"),
            Glyph::Exited => pick("■", "_"),
            Glyph::Stopped => pick("·", "-"),
            Glyph::Done => pick("✓", "+"),
            Glyph::Orphaned => pick("◆", "!"),
            Glyph::Bar => pick("▍", "|"),
            Glyph::Marker => pick("▌", ">"),
            Glyph::Open => pick("▾", "v"),
            Glyph::Folded => pick("▸", ">"),
            Glyph::Attention => "!",
        }
    }

    /// One frame of the spinner for starting and running work.
    pub fn spinner(&self, frame: u64) -> &'static str {
        if self.glyphs == Glyphs::Ascii {
            ASCII_SPINNER[(frame % 4) as usize]
        } else {
            SPINNER[(frame % 10) as usize]
        }
    }
}

/// Whether this process draws in a classic Windows console window (conhost),
/// whose default fonts (Consolas, Lucida Console) have neither the braille of
/// the monitor's graphs nor the quadrant blocks of the banner's logo. Windows
/// Terminal and other terminals host the console through a pseudoconsole and
/// answer false, also when Windows Terminal took over a console started
/// elsewhere, where it sets no `WT_SESSION`.
#[cfg(windows)]
pub fn classic_console() -> bool {
    devlanes_sys::windows::console::classic_window()
}

/// Always false: only Windows has classic console windows.
#[cfg(unix)]
pub fn classic_console() -> bool {
    false
}

/// Whether a `COLORFGBG` value (`15;0`, `0;default;15`) names a dark
/// background; `None` when it says nothing usable.
fn background(value: &str) -> Option<bool> {
    let last = value.rsplit(';').next()?.trim();
    let index: u8 = last.parse().ok()?;
    Some(!matches!(index, 7 | 15))
}

/// A unit color from the daemon's palette as the terminal's own ANSI color,
/// so it follows the user's theme. Unknown names fall back to none.
pub fn palette_color(name: &str) -> Option<Color> {
    Some(match name {
        "red" => Color::Red,
        "green" => Color::Green,
        "yellow" => Color::Yellow,
        "blue" => Color::Blue,
        "magenta" => Color::Magenta,
        "cyan" => Color::Cyan,
        // ANSI 7 and 8: ratatui calls them Gray and DarkGray.
        "white" => Color::Gray,
        "gray" => Color::DarkGray,
        "bright-red" => Color::LightRed,
        "bright-green" => Color::LightGreen,
        "bright-yellow" => Color::LightYellow,
        "bright-blue" => Color::LightBlue,
        "bright-magenta" => Color::LightMagenta,
        "bright-cyan" => Color::LightCyan,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect(theme: Option<&str>, glyphs: Option<&str>, pairs: &[(&str, &str)]) -> Theme {
        detect_in(theme, glyphs, pairs, false)
    }

    fn detect_in(
        theme: Option<&str>,
        glyphs: Option<&str>,
        pairs: &[(&str, &str)],
        classic_console: bool,
    ) -> Theme {
        let env = |key: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| (*v).to_owned())
        };
        Theme::detect(theme, glyphs, env, classic_console)
    }

    #[test]
    fn the_tier_follows_no_color_the_background_and_the_config() {
        let utf8 = ("LANG", "en_US.UTF-8");
        assert_eq!(detect(None, None, &[utf8]).tier, Tier::Ansi);
        // A known background only counts with true color.
        let dark = [utf8, ("COLORTERM", "truecolor"), ("COLORFGBG", "15;0")];
        assert_eq!(detect(None, None, &dark).tier, Tier::Dark);
        let light = [utf8, ("COLORTERM", "24bit"), ("COLORFGBG", "0;default;15")];
        assert_eq!(detect(None, None, &light).tier, Tier::Light);
        assert_eq!(
            detect(None, None, &[utf8, ("COLORFGBG", "15;0")]).tier,
            Tier::Ansi
        );
        assert_eq!(
            detect(None, None, &[utf8, ("COLORTERM", "truecolor")]).tier,
            Tier::Ansi,
            "an unknown background is never assumed dark"
        );
        // The config overrides detection; NO_COLOR overrides everything.
        assert_eq!(detect(Some("light"), None, &dark).tier, Tier::Light);
        assert_eq!(detect(Some("ansi"), None, &dark).tier, Tier::Ansi);
        assert_eq!(
            detect(Some("dark"), None, &[("NO_COLOR", "1")]).tier,
            Tier::Plain
        );
        assert_eq!(detect(None, None, &[("NO_COLOR", "")]).tier, Tier::Ansi);
    }

    #[test]
    fn glyphs_fall_back_to_ascii_outside_utf8() {
        assert_eq!(detect(None, None, &[]).glyphs, Glyphs::Unicode);
        assert_eq!(
            detect(None, None, &[("LANG", "en_US.UTF-8")]).glyphs,
            Glyphs::Unicode
        );
        assert_eq!(detect(None, None, &[("LANG", "C")]).glyphs, Glyphs::Ascii);
        // LC_ALL wins over LANG.
        assert_eq!(
            detect(None, None, &[("LC_ALL", "C"), ("LANG", "en_US.utf8")]).glyphs,
            Glyphs::Ascii
        );
        assert_eq!(
            detect(None, Some("ascii"), &[("LANG", "en_US.UTF-8")]).glyphs,
            Glyphs::Ascii
        );
        let ascii = Theme {
            tier: Tier::Plain,
            glyphs: Glyphs::Ascii,
        };
        assert_eq!(ascii.glyph(Glyph::Ready), "*");
        assert_eq!(ascii.spinner(5), "\\");
    }

    #[test]
    fn a_classic_console_window_gets_ascii_unless_the_config_says_unicode() {
        // No locale is the usual Windows environment.
        assert_eq!(detect_in(None, None, &[], true).glyphs, Glyphs::Ascii);
        let git_bash = [("LANG", "en_US.UTF-8"), ("TERM", "xterm-256color")];
        assert_eq!(detect_in(None, None, &git_bash, true).glyphs, Glyphs::Ascii);
        assert_eq!(
            detect_in(None, Some("auto"), &git_bash, true).glyphs,
            Glyphs::Ascii
        );
        assert_eq!(
            detect_in(None, Some("unicode"), &[], true).glyphs,
            Glyphs::Unicode
        );
        // Other terminals keep the upstream rule.
        let terminal = [("WT_SESSION", "abc")];
        assert_eq!(
            detect_in(None, None, &terminal, false).glyphs,
            Glyphs::Unicode
        );
        // Colors do not depend on the window.
        let dark = [("COLORTERM", "truecolor"), ("COLORFGBG", "15;0")];
        assert_eq!(detect_in(None, None, &dark, true).tier, Tier::Dark);
    }

    #[cfg(unix)]
    #[test]
    fn unix_terminals_are_never_classic_consoles() {
        assert!(!classic_console());
    }

    #[test]
    fn plain_has_no_colors_and_reverses_the_selection() {
        let plain = Theme {
            tier: Tier::Plain,
            glyphs: Glyphs::Unicode,
        };
        assert_eq!(plain.tone(Tone::Failure), Style::new());
        assert_eq!(plain.identity("blue"), None);
        assert!(plain.reverse_selection());
        let dark = Theme {
            tier: Tier::Dark,
            glyphs: Glyphs::Unicode,
        };
        assert_eq!(dark.identity("blue"), Some(Color::Blue));
        assert_eq!(
            dark.tone(Tone::Ok),
            Style::new().fg(Color::Rgb(0x4c, 0xbf, 0x7f))
        );
    }
}

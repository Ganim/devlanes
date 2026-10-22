//! The devlanes terminal UI: a daemon client like the CLI, with a lane tree,
//! a live terminal pane for one unit, a resource monitor and a daemon tab.
//!
//! Quitting only closes connections. Units keep running in the daemon.

pub mod api;
pub mod app;
pub mod doctor;
pub mod graph;
pub mod input;
pub mod logfile;
pub mod monitor;
mod runtime;
pub mod screen;
pub mod start;
pub mod text;
pub mod theme;
pub mod tree;
pub mod ui;
pub mod words;

use std::{io::IsTerminal, sync::Arc};

pub use api::{
    ApiError, AttachEvent, AttachInput, AttachSession, ClientApi, DaemonApi, EventStream,
    LaneMetrics, Metrics, MetricsQuery, SystemMetrics, UnitMetrics, Window,
};
pub use devlanes_client::Paths;
pub use devlanes_term::TerminalLauncher;
pub use runtime::{TerminalGuard, drive, drive_start, terminal_command};

/// Failures that end the TUI before or while it runs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the terminal UI needs a terminal; use devlanes status instead")]
    NotATerminal,
    #[error(transparent)]
    Client(#[from] devlanes_client::Error),
    #[error("terminal error: {0}")]
    Io(#[from] std::io::Error),
    /// A background task panicked; its message is already on stderr.
    #[error("the terminal UI stopped after an internal error; units keep running")]
    Panicked,
}

/// Integrations supplied by the composition root.
#[derive(Default, Clone)]
pub struct Options {
    /// Every available launcher; the binary registers them.
    pub launchers: Vec<Arc<dyn TerminalLauncher>>,
    /// The user config's terminal app, matched against [`TerminalLauncher::name`].
    pub terminal: Option<String>,
    /// How to draw start progress in the tab's progress bar, from
    /// [`devlanes_term::progress_support`]; `None` draws nothing.
    pub progress: Option<devlanes_term::Wrap>,
    /// `tui.theme` from the user config: `auto` (or unset), `dark`, `light` or `ansi`.
    pub theme: Option<String>,
    /// `tui.glyphs` from the user config: `auto` (or unset), `unicode` or `ascii`.
    pub glyphs: Option<String>,
}

impl Options {
    /// The colors and glyphs for this terminal, from the environment and
    /// the user config.
    pub fn theme(&self) -> theme::Theme {
        theme::Theme::detect(self.theme.as_deref(), self.glyphs.as_deref(), |key| {
            std::env::var(key).ok()
        })
    }

    /// [`Options::pick_launcher_with`] with the process environment.
    fn pick_launcher(&self) -> Result<Arc<dyn TerminalLauncher>, String> {
        self.pick_launcher_with(|key| std::env::var(key).ok())
    }

    /// The configured launcher; without one, the terminal this TUI runs in when
    /// a launcher for it is registered, else Terminal.app where it exists, else
    /// the first registered launcher that is installed. Checks the filesystem,
    /// so the runtime calls it from a blocking task.
    fn pick_launcher_with(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Arc<dyn TerminalLauncher>, String> {
        if self.launchers.is_empty() {
            return Err("this build can't open terminal windows".into());
        }
        let names: Vec<&str> = self.launchers.iter().map(|l| l.name()).collect();
        if let Some(wanted) = &self.terminal {
            return self
                .launchers
                .iter()
                .find(|launcher| launcher.name().eq_ignore_ascii_case(wanted))
                .cloned()
                .ok_or_else(|| {
                    format!(
                        "unknown terminal {wanted}; pick one of {}",
                        names.join(", ")
                    )
                });
        }
        let named = |name: &str| self.launchers.iter().find(|l| l.name() == name);
        // The terminal the user is in now is the one they expect a new window in.
        let current = current_terminal(&env).and_then(named);
        let preferred = named("terminal").filter(|launcher| launcher.available());
        current
            .or(preferred)
            .or_else(|| self.launchers.iter().find(|launcher| launcher.available()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no supported terminal app found; set terminal in ~/.devlanes/config.yaml to one of {}",
                    names.join(", ")
                )
            })
    }
}

/// The launcher name of the terminal this process runs in, from the variables
/// each terminal sets. `LC_TERMINAL` and the terminals' own variables survive
/// tmux and ssh, where `TERM_PROGRAM` names tmux or nothing.
fn current_terminal(env: &impl Fn(&str) -> Option<String>) -> Option<&'static str> {
    let set = |key: &str| env(key).is_some_and(|value| !value.is_empty());
    let program = env("TERM_PROGRAM").unwrap_or_default();
    Some(match program.as_str() {
        "iTerm.app" => "iterm2",
        "ghostty" => "ghostty",
        "WezTerm" => "wezterm",
        "Apple_Terminal" => "terminal",
        _ if env("LC_TERMINAL").as_deref() == Some("iTerm2") => "iterm2",
        _ if set("KITTY_WINDOW_ID") || env("TERM").as_deref() == Some("xterm-kitty") => "kitty",
        _ if set("GHOSTTY_RESOURCES_DIR") => "ghostty",
        _ if set("WEZTERM_EXECUTABLE") || set("WEZTERM_PANE") => "wezterm",
        _ => return None,
    })
}

/// Opens the TUI for bare `devlanes` and `devlanes ui`, without terminal launchers.
pub async fn run(paths: Paths) -> Result<(), Error> {
    run_with(paths, Options::default()).await
}

/// Opens the TUI with launchers and the configured terminal.
///
/// Connects (spawning the daemon when needed) before touching the terminal,
/// so connection errors print normally.
pub async fn run_with(paths: Paths, options: Options) -> Result<(), Error> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(Error::NotATerminal);
    }
    let api: Arc<dyn DaemonApi> = ClientApi::connect(paths).await?;
    let guard = TerminalGuard::enter()?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;
    let events = crossterm::event::EventStream::new();
    let result = drive(
        &mut terminal,
        api,
        &options,
        events,
        Box::new(std::io::stdout()),
    )
    .await;
    drop(guard);
    result
}

/// Opens the TUI on a start the daemon already accepted (see [`start`]) and
/// follows it until the user quits. Quitting never stops it.
pub async fn run_start(
    paths: Paths,
    options: Options,
    start: start::StartHandoff,
) -> Result<start::StartExit, Error> {
    if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
        return Err(Error::NotATerminal);
    }
    let api: Arc<dyn DaemonApi> = ClientApi::connect(paths).await?;
    let guard = TerminalGuard::enter()?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;
    let events = crossterm::event::EventStream::new();
    let result = drive_start(
        &mut terminal,
        api,
        &options,
        events,
        Box::new(std::io::stdout()),
        start,
    )
    .await;
    drop(guard);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use devlanes_term::{Invocation, LaunchError, LaunchRequest};

    /// A launcher that is installed or not, by construction.
    struct Fake(&'static str, bool);

    impl TerminalLauncher for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn invocation(&self, _: &LaunchRequest) -> Result<Invocation, LaunchError> {
            Err(LaunchError::EmptyCommand)
        }
        fn available(&self) -> bool {
            self.1
        }
    }

    fn options(terminal: Option<&str>, launchers: &[(&'static str, bool)]) -> Options {
        Options {
            launchers: launchers
                .iter()
                .map(|(name, there)| Arc::new(Fake(name, *there)) as Arc<dyn TerminalLauncher>)
                .collect(),
            terminal: terminal.map(str::to_owned),
            ..Options::default()
        }
    }

    #[test]
    fn default_launcher_is_terminal_app_else_the_first_installed() {
        // Outside any known terminal, whatever runs the tests.
        let picked = |o: Options| o.pick_launcher_with(|_| None).map(|l| l.name());
        // macOS: Terminal.app is always there and wins over other installed apps.
        let mac = [("iterm2", true), ("kitty", true), ("terminal", true)];
        assert_eq!(picked(options(None, &mac)), Ok("terminal"));
        // Linux: Terminal.app and iTerm2 are absent; the first installed one wins.
        let linux = [
            ("iterm2", false),
            ("ghostty", false),
            ("kitty", true),
            ("terminal", false),
        ];
        assert_eq!(picked(options(None, &linux)), Ok("kitty"));
        // A configured terminal is used as named, installed or not.
        assert_eq!(picked(options(Some("Ghostty"), &linux)), Ok("ghostty"));
        assert!(picked(options(Some("xterm"), &linux)).is_err());
        assert!(picked(options(None, &[("terminal", false)])).is_err());
    }

    #[test]
    fn the_terminal_the_tui_runs_in_is_preferred() {
        let mac = [
            ("iterm2", true),
            ("ghostty", true),
            ("wezterm", true),
            ("kitty", true),
            ("terminal", true),
        ];
        let picked = |terminal: Option<&str>, pairs: &[(&str, &str)]| {
            let env: Vec<(String, String)> = pairs
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
            options(terminal, &mac)
                .pick_launcher_with(|key| {
                    env.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
                })
                .map(|l| l.name())
        };
        for (pairs, expected) in [
            (&[("TERM_PROGRAM", "iTerm.app")][..], "iterm2"),
            (&[("TERM_PROGRAM", "ghostty")][..], "ghostty"),
            (&[("TERM_PROGRAM", "WezTerm")][..], "wezterm"),
            (&[("TERM_PROGRAM", "Apple_Terminal")][..], "terminal"),
            (
                &[("KITTY_WINDOW_ID", "3"), ("TERM", "xterm-kitty")][..],
                "kitty",
            ),
            // Inside tmux TERM_PROGRAM says tmux; iTerm2 still sets LC_TERMINAL.
            (
                &[("TERM_PROGRAM", "tmux"), ("LC_TERMINAL", "iTerm2")][..],
                "iterm2",
            ),
            (
                &[("TERM_PROGRAM", "tmux"), ("GHOSTTY_RESOURCES_DIR", "/x")][..],
                "ghostty",
            ),
            // Unknown terminals fall back to the default.
            (&[("TERM_PROGRAM", "vscode")][..], "terminal"),
            (&[("WT_SESSION", "abc")][..], "terminal"),
        ] {
            assert_eq!(picked(None, pairs), Ok(expected), "{pairs:?}");
        }
        // An explicit choice in the user config still wins.
        assert_eq!(
            picked(Some("kitty"), &[("TERM_PROGRAM", "iTerm.app")]),
            Ok("kitty")
        );
    }
}

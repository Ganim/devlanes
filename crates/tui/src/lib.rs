//! The devlanes terminal UI: a daemon client like the CLI, with a lane tree,
//! a live terminal pane for one unit, a resource monitor and a daemon tab.
//!
//! Quitting only closes connections. Units keep running in the daemon.

pub mod api;
pub mod app;
pub mod doctor;
pub mod input;
pub mod logfile;
mod runtime;
pub mod screen;
pub mod text;
pub mod ui;
pub mod words;

use std::{io::IsTerminal, sync::Arc};

pub use api::{
    ApiError, AttachEvent, AttachInput, AttachSession, ClientApi, DaemonApi, EventStream,
    UnitMetrics,
};
pub use devlanes_client::Paths;
pub use devlanes_term::TerminalLauncher;
pub use runtime::{TerminalGuard, drive, terminal_command};

/// Failures that end the TUI before or while it runs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the terminal UI needs a terminal; use devlanes status instead")]
    NotATerminal,
    #[error(transparent)]
    Client(#[from] devlanes_client::Error),
    #[error("terminal I/O: {0}")]
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
}

impl Options {
    /// The configured launcher; without one, Terminal.app where it exists, else
    /// the first registered launcher that is installed. Checks the filesystem,
    /// so the runtime calls it from a blocking task.
    fn pick_launcher(&self) -> Result<Arc<dyn TerminalLauncher>, String> {
        if self.launchers.is_empty() {
            return Err("no terminal launchers are available in this build".into());
        }
        let names: Vec<&str> = self.launchers.iter().map(|l| l.name()).collect();
        if let Some(wanted) = &self.terminal {
            return self
                .launchers
                .iter()
                .find(|launcher| launcher.name().eq_ignore_ascii_case(wanted))
                .cloned()
                .ok_or_else(|| {
                    format!("unknown terminal {wanted}; use one of {}", names.join(", "))
                });
        }
        let preferred = self
            .launchers
            .iter()
            .find(|launcher| launcher.name() == "terminal" && launcher.available());
        preferred
            .or_else(|| self.launchers.iter().find(|launcher| launcher.available()))
            .cloned()
            .ok_or_else(|| {
                format!(
                    "no terminal app found; set terminal in the user config to one of {}",
                    names.join(", ")
                )
            })
    }
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
            progress: None,
        }
    }

    #[test]
    fn default_launcher_is_terminal_app_else_the_first_installed() {
        let picked = |o: Options| o.pick_launcher().map(|l| l.name());
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
}

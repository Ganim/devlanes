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
pub use runtime::{TerminalGuard, drive};

/// Failures that end the TUI before or while it runs.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("the terminal UI needs a terminal; use devlanes status instead")]
    NotATerminal,
    #[error(transparent)]
    Client(#[from] devlanes_client::Error),
    #[error("terminal I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// Integrations supplied by the composition root.
#[derive(Default, Clone)]
pub struct Options {
    /// Every available launcher; the binary registers them.
    pub launchers: Vec<Arc<dyn TerminalLauncher>>,
    /// The user config's `terminal:` value, matched against [`TerminalLauncher::name`].
    pub terminal: Option<String>,
}

impl Options {
    /// The launcher for `terminal`, falling back to one named `terminal` (Terminal.app).
    fn launcher(&self) -> Result<Arc<dyn TerminalLauncher>, String> {
        let wanted = self.terminal.as_deref().unwrap_or("terminal");
        if let Some(launcher) = self
            .launchers
            .iter()
            .find(|launcher| launcher.name().eq_ignore_ascii_case(wanted))
        {
            return Ok(Arc::clone(launcher));
        }
        if self.launchers.is_empty() {
            return Err("no terminal launchers are available in this build".into());
        }
        let names: Vec<&str> = self.launchers.iter().map(|l| l.name()).collect();
        Err(match &self.terminal {
            Some(name) => format!("unknown terminal {name}; use one of {}", names.join(", ")),
            None => format!(
                "set terminal in the user config to one of {}",
                names.join(", ")
            ),
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

//! The devlanes terminal UI: a daemon client like the CLI, with a lane tree,
//! a live terminal pane for one unit, a resource monitor and a daemon tab.
//!
//! Quitting closes connections and units keep running in the daemon, unless
//! the user chooses to stop the project's lanes on the way out.

pub mod api;
pub mod app;
pub mod banner;
pub mod daemonlog;
pub mod doctor;
pub mod graph;
#[cfg(windows)]
mod home;
pub mod hyperlink;
pub mod input;
pub mod logfile;
pub mod menu;
pub mod monitor;
mod runtime;
pub mod screen;
pub mod settings;
pub mod start;
pub mod text;
pub mod theme;
pub mod tree;
pub mod ui;
pub mod welcome;
pub mod words;

use std::{io::IsTerminal, sync::Arc};

pub use api::{
    ApiError, AttachEvent, AttachInput, AttachSession, ClientApi, DaemonApi, EventStream,
    LaneMetrics, Metrics, MetricsQuery, SystemMetrics, UnitMetrics, Window,
};
pub use devlanes_client::Paths;
pub use runtime::{TerminalGuard, drive, drive_start};

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

/// What the composition root reads from the user config.
#[derive(Default, Clone)]
pub struct Options {
    /// How to draw start progress in the tab's progress bar, from
    /// [`devlanes_term::progress_support`]; `None` draws nothing.
    pub progress: Option<devlanes_term::Wrap>,
    /// Whether the tab title says which lane is shown and how it stands
    /// ([`devlanes_term::title_support`]); the previous title returns on exit.
    pub title: bool,
    /// Clickable service URLs in the pane, from [`devlanes_term::links_support`].
    pub links: Option<devlanes_term::Links>,
    /// Desktop notifications while the terminal is not focused, from
    /// [`devlanes_term::notifications_support`].
    pub notifications: Option<devlanes_term::Notifications>,
    /// Wraps each frame in synchronized output (`CSI ? 2026 h` / `l`), so the
    /// terminal shows it at once; terminals without it ignore the mode.
    pub sync_output: bool,
    /// `tui.theme` from the user config: `auto` (or unset), `dark`, `light` or `ansi`.
    pub theme: Option<String>,
    /// `tui.glyphs` from the user config: `auto` (or unset), `unicode` or `ascii`.
    pub glyphs: Option<String>,
    /// `tui.on_quit` from the user config: `ask` (or unset), `keep` or `stop`.
    pub on_quit: Option<String>,
    /// The per-user service definition as `devlanes doctor` reads it
    /// (`{manager, path, installed, stop_window_s, ...}`), for the daemon
    /// page's stop-window check; `None` leaves that check out.
    pub service: Option<serde_json::Value>,
    /// The environment the TUI reads (`NO_COLOR`, `COLORTERM`, the locale,
    /// `TMUX`, `HOME`); `None` reads the process's. Tests pass their own so
    /// the machine they run on does not show in what they draw.
    pub env: Option<std::collections::BTreeMap<String, String>>,
    /// Where daemon.log times are shown; `None` is the system's zone.
    pub time_zone: Option<jiff::tz::TimeZone>,
    /// The directory whose lane opens first; `None` is the process's.
    pub cwd: Option<std::path::PathBuf>,
}

impl Options {
    /// The colors and glyphs for this terminal, from the environment and
    /// the user config.
    pub fn theme(&self) -> theme::Theme {
        theme::Theme::detect(self.theme.as_deref(), self.glyphs.as_deref(), |key| {
            self.var(key)
        })
    }

    /// An environment variable from [`Options::env`], else the process's.
    pub fn var(&self, key: &str) -> Option<String> {
        match &self.env {
            Some(env) => env.get(key).cloned(),
            None => std::env::var(key).ok(),
        }
    }
}

/// Opens the TUI for bare `devlanes` and `devlanes ui` with default options.
pub async fn run(paths: Paths) -> Result<(), Error> {
    run_with(paths, Options::default()).await
}

/// Opens the TUI with the user config's options.
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

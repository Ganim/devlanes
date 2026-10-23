//! Terminal integrations for devlanes clients: the terminal tab's progress
//! bar during starts ([`progress`]), with detection of terminals that draw it,
//! and opening URLs in the Windows browser on Windows and inside WSL
//! ([`open_in_windows_browser`]).

mod open;
pub mod progress;

pub use open::{OpenError, open_in_windows_browser};

pub use progress::{
    PROGRESS_ENV, Progress, ProgressBar, ProgressMode, StartTracker, Wrap, phase_detail,
    progress_support, progress_support_from_env,
};

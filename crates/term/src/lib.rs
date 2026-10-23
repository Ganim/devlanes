//! Terminal integrations for devlanes clients: the tab's progress bar during
//! starts ([`progress`]), the tab title ([`title`]), clickable links
//! ([`links`]), desktop notifications ([`notify`]) and section marks
//! ([`marks`]). Each passes the same gate ([`detect`]): human output on a
//! terminal, the user's `terminal.*` setting and its environment override,
//! and in auto mode only terminals known to support it, never tmux, screen,
//! CI or a dumb terminal.

pub mod detect;
pub mod links;
pub mod marks;
pub mod notify;
pub mod progress;
pub mod title;

pub use detect::{Emulator, Host, Mode, gate, identify};
pub use links::{LINKS_ENV, Links, file_url, link, links_support, links_support_from_env};
pub use marks::{MARKS_ENV, Marks, marked, marks_support, marks_support_from_env};
pub use notify::{
    LONG_START, NOTIFICATIONS_ENV, NOTIFY_GAP, Notifications, Notifier, NotifyStyle,
    notifications_support, notifications_support_from_env,
};
pub use progress::{
    PROGRESS_ENV, Progress, ProgressBar, ProgressMode, StartCounts, StartTracker, Wrap,
    phase_detail, progress_support, progress_support_from_env,
};
pub use title::{
    MAX_TITLE_CHARS, POP_TITLE, PUSH_TITLE, TITLE_ENV, TitleGuard, sanitize_title, title_sequence,
    title_support, title_support_from_env,
};

/// The user's `terminal` settings, as the config words them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Settings {
    pub progress: ProgressMode,
    pub title: Mode,
    pub links: Mode,
    pub notifications: Mode,
    pub marks: Mode,
}

/// What this invocation writes to its terminal, decided once.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    pub progress: Option<Wrap>,
    pub title: bool,
    pub links: Option<Links>,
    pub notifications: Option<Notifications>,
    pub marks: Option<Marks>,
}

impl Features {
    /// Each feature for output to `tty` under `settings` and `env`. Callers
    /// pass `tty: false` for anything but human output (agent, JSON, plain).
    pub fn detect(settings: &Settings, tty: bool, env: impl Fn(&str) -> Option<String>) -> Self {
        Self {
            progress: progress_support(settings.progress, tty, &env),
            title: title_support(settings.title, tty, &env),
            links: links_support(settings.links, tty, &env),
            notifications: notifications_support(settings.notifications, tty, &env),
            marks: marks_support(settings.marks, tty, &env),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_detected_for_a_pipe_whatever_the_settings() {
        let always = Settings {
            progress: ProgressMode::Always,
            title: Mode::Always,
            links: Mode::Always,
            notifications: Mode::Always,
            marks: Mode::Always,
        };
        let env = |key: &str| (key == "TERM_PROGRAM").then(|| "iTerm.app".to_owned());
        assert_eq!(Features::detect(&always, false, env), Features::default());
        let on = Features::detect(&Settings::default(), true, env);
        assert!(on.title && on.links.is_some() && on.notifications.is_some() && on.marks.is_some());
    }
}

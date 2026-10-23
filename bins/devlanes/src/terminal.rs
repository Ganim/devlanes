//! The user's `terminal` settings, and the tab title and end-of-start
//! notification of long commands. What each command shows is worded in
//! `devlanes-cli` ([`CommandTitle`]); this module writes it to stdout.

use std::{
    io::{Stdout, Write},
    time::Instant,
};

use devlanes_cli::{CommandTitle, OutputMode, Runtime};
use devlanes_client::Paths;
use devlanes_config::{TerminalFeature, TerminalProgress};
use devlanes_proto::Event;
use devlanes_term::{Features, Mode, ProgressMode, Settings, TitleGuard};

/// The `terminal` settings from the user config; auto when unset or unreadable.
pub fn settings(paths: &Paths) -> Settings {
    let terminal = devlanes_config::load_user(paths.home.join("config.yaml"))
        .ok()
        .and_then(|config| config.terminal)
        .map(|terminal| terminal.settings())
        .unwrap_or_default();
    let mode = |feature: TerminalFeature| match feature {
        TerminalFeature::Auto => Mode::Auto,
        TerminalFeature::Always => Mode::Always,
        TerminalFeature::Never => Mode::Never,
    };
    Settings {
        progress: match terminal.progress {
            TerminalProgress::Auto => ProgressMode::Auto,
            TerminalProgress::Always => ProgressMode::Always,
            TerminalProgress::Never => ProgressMode::Never,
        },
        title: mode(terminal.title),
        links: mode(terminal.links),
        notifications: mode(terminal.notifications),
        marks: mode(terminal.marks),
    }
}

/// This invocation's terminal features. The config is read only for human
/// output on a terminal; everything else gets none.
pub fn detect(runtime: &Runtime) -> Features {
    if runtime.mode != OutputMode::Human || !runtime.stdout_tty || runtime.ci.is_some() {
        return Features::default();
    }
    let settings = Paths::discover()
        .map(|paths| settings(&paths))
        .unwrap_or_default();
    runtime.detect_terminal(&settings, |key| std::env::var(key).ok())
}

/// A long command's tab title, put back when dropped.
pub struct Title {
    guard: TitleGuard<Stdout>,
    title: CommandTitle,
    started: Instant,
}

impl Title {
    /// Sets `title` when this invocation sets titles, and follows the command
    /// for its end-of-start notification either way; `None` when it does
    /// neither, or for a command that runs too briefly to have a title.
    pub fn start(runtime: &Runtime, title: Option<CommandTitle>) -> Option<Self> {
        let title = title?;
        let shown = runtime.terminal.title;
        let notifies = runtime
            .terminal
            .notifications
            .is_some_and(|n| n.without_focus());
        if !shown && !notifies {
            return None;
        }
        // Disabled when titles are off: it then writes nothing at all.
        let mut guard = TitleGuard::new(std::io::stdout(), shown);
        // The title is decoration; a terminal that refuses it is no error.
        let _ = guard.set(&title.text());
        Some(Self {
            guard,
            title,
            started: Instant::now(),
        })
    }

    /// One event of the command; the title changes only when its text does.
    pub fn event(&mut self, event: &Event) {
        self.title.event(event);
        let _ = self.guard.set(&self.title.text());
    }

    /// The command ended: a start that ran long says so in a notification,
    /// where the terminal itself skips a focused window or the user asked
    /// for notifications always. The title is put back on drop.
    pub fn finish(&self, runtime: &Runtime, ok: bool) {
        let Some(notifications) = runtime.terminal.notifications.filter(|n| n.without_focus())
        else {
            return;
        };
        if let Some((title, body)) = self.title.finished(ok, self.started.elapsed()) {
            let mut stdout = std::io::stdout();
            let _ = stdout.write_all(notifications.sequence(&title, &body).as_bytes());
            let _ = stdout.flush();
        }
    }
}

/// [`Title::event`] on an optional title.
pub fn title_event(title: &mut Option<Title>, event: &Event) {
    if let Some(title) = title {
        title.event(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_come_from_the_user_config_and_default_to_auto() -> anyhow::Result<()> {
        let home = tempfile::tempdir()?;
        let paths = Paths::new(home.path())?;
        assert_eq!(settings(&paths), Settings::default(), "no config.yaml");
        std::fs::write(
            home.path().join("config.yaml"),
            "terminal:\n  progress: never\n  title: never\n  links: always\n  marks: never\n",
        )?;
        assert_eq!(
            settings(&paths),
            Settings {
                progress: ProgressMode::Never,
                title: Mode::Never,
                links: Mode::Always,
                notifications: Mode::Auto,
                marks: Mode::Never,
            }
        );
        // A config that does not load leaves everything on auto.
        std::fs::write(home.path().join("config.yaml"), "terminal: [\n")?;
        assert_eq!(settings(&paths), Settings::default());
        Ok(())
    }
}

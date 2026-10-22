//! Stop signals as `stop.signal` names them, and what each OS delivers for them.

use std::fmt;

/// A signal `stop.signal` accepts. Runners map it to what their OS can deliver:
/// the POSIX signal on Unix, a console event or termination on Windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StopSignal {
    Hangup,
    Interrupt,
    Quit,
    Kill,
    Terminate,
    User1,
    User2,
}

impl StopSignal {
    /// Every signal, in config-name order.
    pub const ALL: [Self; 7] = [
        Self::Hangup,
        Self::Interrupt,
        Self::Quit,
        Self::Kill,
        Self::Terminate,
        Self::User1,
        Self::User2,
    ];

    /// Parses a config name (`SIGTERM`); `None` for signals config does not accept.
    ///
    /// ```
    /// use devlanes_core::StopSignal;
    /// assert_eq!(StopSignal::from_config_name("SIGINT"), Some(StopSignal::Interrupt));
    /// assert_eq!(StopSignal::from_config_name("SIGWINCH"), None);
    /// ```
    pub fn from_config_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.config_name() == name)
    }

    /// The config name, such as `SIGTERM`.
    pub fn config_name(self) -> &'static str {
        match self {
            Self::Hangup => "SIGHUP",
            Self::Interrupt => "SIGINT",
            Self::Quit => "SIGQUIT",
            Self::Kill => "SIGKILL",
            Self::Terminate => "SIGTERM",
            Self::User1 => "SIGUSR1",
            Self::User2 => "SIGUSR2",
        }
    }

    /// The launch note when a stop on this OS does not deliver this signal;
    /// `None` when the signal is sent as is (always on Unix).
    pub fn launch_note(self) -> Option<String> {
        os::launch_note(self)
    }
}

impl fmt::Display for StopSignal {
    /// Writes the config name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.config_name())
    }
}

#[cfg(unix)]
mod os {
    use super::StopSignal;

    /// Every configurable signal exists on Unix and is sent as is.
    pub(super) fn launch_note(_signal: StopSignal) -> Option<String> {
        None
    }
}

#[cfg(windows)]
mod os {
    use super::StopSignal;

    /// What a Windows stop will send for a configured signal once graceful
    /// stops exist. Windows has no POSIX signals; console processes only see
    /// Ctrl-C and Ctrl-Break.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum WindowsStop {
        /// Ctrl-C, the closest to SIGINT from a terminal.
        CtrlC,
        /// Ctrl-Break, which ends a process with the default handler (Node sees
        /// `SIGBREAK`).
        CtrlBreak,
        /// Immediate termination of every unit process, like SIGKILL.
        Terminate,
    }

    impl StopSignal {
        /// The Windows stop this signal is planned to map to once graceful stops
        /// exist; until then the runner terminates every stop (see
        /// [`StopSignal::launch_note`]). SIGHUP, SIGQUIT, SIGUSR1 and SIGUSR2
        /// have no equivalent and will get Ctrl-Break like SIGTERM, so a config
        /// shared with Unix machines still starts here.
        pub fn windows_stop(self) -> WindowsStop {
            match self {
                Self::Interrupt => WindowsStop::CtrlC,
                Self::Kill => WindowsStop::Terminate,
                Self::Terminate | Self::Hangup | Self::Quit | Self::User1 | Self::User2 => {
                    WindowsStop::CtrlBreak
                }
            }
        }
    }

    /// The Windows runner does not send Ctrl-C or Ctrl-Break yet: every stop
    /// terminates the unit's processes at once, whatever the signal, so every
    /// service gets this note until graceful stops land.
    pub(super) fn launch_note(signal: StopSignal) -> Option<String> {
        Some(format!(
            "on Windows a stop ends the process tree immediately; stop.signal {signal} is not sent yet (graceful Ctrl-C and Ctrl-Break stops come later)"
        ))
    }
}

#[cfg(windows)]
pub use os::WindowsStop;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// Config names round-trip; other names are rejected rather than defaulted.
    fn config_names_round_trip() {
        for signal in StopSignal::ALL {
            assert_eq!(
                StopSignal::from_config_name(signal.config_name()),
                Some(signal)
            );
            assert_eq!(signal.to_string(), signal.config_name());
        }
        for name in ["SIGWINCH", "TERM", "sigterm", "15", ""] {
            assert_eq!(StopSignal::from_config_name(name), None, "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    /// Unix delivers every configurable signal as is.
    fn unix_notes_nothing() {
        assert!(StopSignal::ALL.iter().all(|s| s.launch_note().is_none()));
    }

    #[cfg(windows)]
    #[test]
    /// SIGINT is planned as Ctrl-C and SIGKILL terminates; everything else is
    /// planned as Ctrl-Break. Until then every signal notes that stops
    /// terminate at once.
    fn windows_mapping() {
        use StopSignal::*;
        assert_eq!(Interrupt.windows_stop(), WindowsStop::CtrlC);
        assert_eq!(Kill.windows_stop(), WindowsStop::Terminate);
        for signal in [Terminate, Hangup, Quit, User1, User2] {
            assert_eq!(signal.windows_stop(), WindowsStop::CtrlBreak, "{signal}");
        }
        for signal in StopSignal::ALL {
            let note = signal.launch_note().unwrap_or_default();
            assert!(note.contains(signal.config_name()), "{note}");
            assert!(note.contains("immediately"), "{note}");
        }
    }
}

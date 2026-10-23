//! Stop signals as `stop.signal` names them. Runners map them to what their OS
//! delivers: the POSIX signal on Unix, a console event or termination on Windows.

/// A signal `stop.signal` accepts.
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
}

/// What a Windows stop sends for a configured signal. Windows has no POSIX
/// signals; console processes only see Ctrl-C and Ctrl-Break.
#[cfg(windows)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowsStop {
    /// Ctrl-C to the unit's console.
    CtrlC,
    /// Ctrl-Break to the unit's console; the default handler ends the process
    /// (Node reports `SIGBREAK`).
    CtrlBreak,
    /// Immediate termination of every unit process.
    Terminate,
}

#[cfg(windows)]
impl StopSignal {
    /// The Windows stop for this signal. `through_shell` says whether the unit
    /// runs through Git's `sh` (a `run:` string): programs `sh` starts ignore
    /// Ctrl-C, so SIGINT becomes Ctrl-Break there. SIGHUP, SIGQUIT, SIGUSR1 and
    /// SIGUSR2 have no equivalent and get Ctrl-Break like SIGTERM, so a config
    /// shared with Unix machines still works.
    pub fn windows_stop(self, through_shell: bool) -> WindowsStop {
        match self {
            Self::Interrupt if !through_shell => WindowsStop::CtrlC,
            Self::Kill => WindowsStop::Terminate,
            Self::Interrupt
            | Self::Terminate
            | Self::Hangup
            | Self::Quit
            | Self::User1
            | Self::User2 => WindowsStop::CtrlBreak,
        }
    }

    /// The launch note (`stop_signal_mapped`) when a Windows stop does not send
    /// what the config names; `None` for SIGTERM, SIGKILL and a SIGINT sent as
    /// Ctrl-C, which map the way a reader expects.
    pub fn windows_launch_note(self, through_shell: bool) -> Option<String> {
        let name = self.config_name();
        match self.windows_stop(through_shell) {
            WindowsStop::CtrlBreak if self == Self::Interrupt => Some(format!(
                "on Windows stop.signal {name} is sent as Ctrl-Break, since programs a run string starts ignore Ctrl-C"
            )),
            WindowsStop::CtrlBreak if self != Self::Terminate => Some(format!(
                "on Windows stop.signal {name} is sent as Ctrl-Break"
            )),
            _ => None,
        }
    }
}

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
        }
        for name in ["SIGWINCH", "TERM", "sigterm", "15", ""] {
            assert_eq!(StopSignal::from_config_name(name), None, "{name}");
        }
    }

    #[cfg(windows)]
    #[test]
    /// SIGINT is Ctrl-C unless the shell runs the unit, SIGKILL terminates and
    /// everything else is Ctrl-Break, with a note unless it is SIGTERM.
    fn windows_mapping() {
        use StopSignal::*;
        assert_eq!(Interrupt.windows_stop(false), WindowsStop::CtrlC);
        assert_eq!(Interrupt.windows_stop(true), WindowsStop::CtrlBreak);
        for shell in [false, true] {
            assert_eq!(Kill.windows_stop(shell), WindowsStop::Terminate);
            for signal in [Terminate, Hangup, Quit, User1, User2] {
                assert_eq!(signal.windows_stop(shell), WindowsStop::CtrlBreak);
            }
            for signal in [Terminate, Kill] {
                assert_eq!(signal.windows_launch_note(shell), None, "{signal:?}");
            }
            for signal in [Hangup, Quit, User1, User2] {
                let note = signal.windows_launch_note(shell).unwrap_or_default();
                assert!(note.contains(signal.config_name()), "{note}");
                assert!(note.contains("Ctrl-Break"), "{note}");
            }
        }
        assert_eq!(Interrupt.windows_launch_note(false), None);
        let note = Interrupt.windows_launch_note(true).unwrap_or_default();
        assert!(
            note.contains("SIGINT") && note.contains("Ctrl-Break"),
            "{note}"
        );
    }
}

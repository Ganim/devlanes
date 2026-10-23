//! Starting helper programs (git, docker, mise, curl, ...) without console windows.

/// Keeps a helper program from opening a console window on Windows.
///
/// A process without a console, such as the detached daemon, that starts a
/// console program makes Windows give the child a new console with a visible
/// window: every `git` or `docker` call would flash one. Call this on every
/// command devlanes runs for itself. Not for units, which the runners start, nor
/// for programs that should show a window (terminals) or share the user's
/// terminal (`env shell`, `devlanes with`).
pub trait NoWindow {
    /// On Windows, when this process has no console, sets `CREATE_NO_WINDOW`:
    /// the child still gets a console, just without a window. It replaces
    /// creation flags set earlier. With a console the child shares it and no
    /// window appears, so nothing is set: a hidden console would cost a console
    /// host process per call and cut the child off from the terminal's Ctrl-C.
    /// Does nothing on Unix.
    fn no_window(&mut self) -> &mut Self;
}

#[cfg(unix)]
mod os {
    use super::NoWindow;

    impl NoWindow for std::process::Command {
        /// Unix has no console windows.
        fn no_window(&mut self) -> &mut Self {
            self
        }
    }

    impl NoWindow for tokio::process::Command {
        /// Unix has no console windows.
        fn no_window(&mut self) -> &mut Self {
            self
        }
    }
}

#[cfg(windows)]
mod os {
    use std::os::windows::process::CommandExt;

    use super::NoWindow;
    use crate::windows::process::{CREATE_NO_WINDOW, has_console};

    /// The creation flags [`NoWindow`] sets: `CREATE_NO_WINDOW` unless this
    /// process has a console to share, else none.
    pub(crate) fn window_flags() -> u32 {
        if has_console() { 0 } else { CREATE_NO_WINDOW }
    }

    impl NoWindow for std::process::Command {
        /// `CREATE_NO_WINDOW` unless this process has a console to share.
        fn no_window(&mut self) -> &mut Self {
            match window_flags() {
                0 => self,
                flags => self.creation_flags(flags),
            }
        }
    }

    impl NoWindow for tokio::process::Command {
        /// `CREATE_NO_WINDOW` unless this process has a console to share.
        fn no_window(&mut self) -> &mut Self {
            match window_flags() {
                0 => self,
                flags => self.creation_flags(flags),
            }
        }
    }
}

#[cfg(windows)]
pub(crate) use os::window_flags;

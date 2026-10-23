//! Logon start of the devlanes daemon on Windows: `devlanes daemon install`
//! writes the current user's `Run` value `devlanes` = `"<exe>" daemon run`,
//! `uninstall` removes it, and `doctor` reads it back, including whether Task
//! Manager's Startup apps page turned it off.
//!
//! The Run key starts the program at logon without a console; the binary's
//! manifest and `daemon run` handle that. The key carries no environment, so
//! the daemon it starts uses the default home. Everything lives under
//! `HKEY_CURRENT_USER`, so nothing needs elevation. Calls block.
//!
//! Task Manager's Startup apps page keeps its on/off switch in
//! `...\Explorer\StartupApproved\Run`, as a binary record whose odd first
//! byte means off.

use std::{io, path::Path};

use devlanes_sys_windows::registry;

/// The current user's logon programs, under `HKEY_CURRENT_USER`.
pub const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// Task Manager's on/off state of each [`RUN_KEY`] value, by value name.
pub const APPROVED_KEY: &str =
    r"Software\Microsoft\Windows\CurrentVersion\Explorer\StartupApproved\Run";

/// The value name devlanes uses in both keys.
pub const VALUE_NAME: &str = "devlanes";

/// The [`APPROVED_KEY`] record Task Manager writes when it turns an entry on:
/// an even first byte, no time stamp.
const ENABLED: [u8; 12] = [2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

/// Where the logon entry lives: a `Run` key, its `StartupApproved` key and
/// the value name, all under `HKEY_CURRENT_USER`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logon {
    run_key: String,
    approved_key: String,
    name: String,
}

/// The logon entry as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    /// The command the Run value holds; `None` when there is no value.
    pub command: Option<String>,
    /// `false` when Task Manager's Startup apps page turned the entry off.
    /// A missing record counts as on, as it does for Windows.
    pub enabled: bool,
}

/// What [`Logon::install`] changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Installed {
    /// What happened to the Run value.
    pub change: Change,
    /// The entry had been turned off in Task Manager and is on again.
    pub re_enabled: bool,
}

/// What happened to the Run value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// There was none; it was written.
    Installed,
    /// It held another command (another `devlanes.exe`); it was rewritten.
    Updated,
    /// It already held this command.
    UpToDate,
}

impl Logon {
    /// The real entry: [`RUN_KEY`] and [`APPROVED_KEY`] with [`VALUE_NAME`].
    pub fn user() -> Self {
        Self {
            run_key: RUN_KEY.into(),
            approved_key: APPROVED_KEY.into(),
            name: VALUE_NAME.into(),
        }
    }

    /// An entry under other `HKEY_CURRENT_USER` subkeys, so tests never touch
    /// the real Run key.
    #[cfg(test)]
    fn at(
        run_key: impl Into<String>,
        approved_key: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        Self {
            run_key: run_key.into(),
            approved_key: approved_key.into(),
            name: name.into(),
        }
    }

    /// The Run value's full name for messages, `HKCU\...\Run\devlanes`.
    pub fn location(&self) -> String {
        format!(r"HKCU\{}\{}", self.run_key, self.name)
    }

    /// Reads the Run value and Task Manager's record of it.
    pub fn status(&self) -> io::Result<Status> {
        Ok(Status {
            command: registry::read_string(&self.run_key, &self.name)?,
            enabled: !self.disabled(),
        })
    }

    /// Makes the logon start run `exe daemon run`: writes the Run value when
    /// it is missing or holds another command, and turns the entry back on
    /// when Task Manager turned it off.
    pub fn install(&self, exe: &Path) -> io::Result<Installed> {
        let wanted = command(exe)?;
        // A value of another type than text is replaced like a different one.
        let previous = registry::read_string(&self.run_key, &self.name).unwrap_or(None);
        let change = match previous {
            Some(previous) if previous == wanted => Change::UpToDate,
            Some(_) => Change::Updated,
            None => Change::Installed,
        };
        if change != Change::UpToDate {
            registry::write_string(&self.run_key, &self.name, &wanted)?;
        }
        let re_enabled = self.disabled();
        if re_enabled {
            registry::write_binary(&self.approved_key, &self.name, &ENABLED)?;
        }
        Ok(Installed { change, re_enabled })
    }

    /// Removes the Run value and Task Manager's record of it; `false` when
    /// there was no Run value. A running daemon keeps running.
    pub fn uninstall(&self) -> io::Result<bool> {
        let removed = registry::delete_value(&self.run_key, &self.name)?;
        registry::delete_value(&self.approved_key, &self.name)?;
        Ok(removed)
    }

    /// Whether Task Manager's record turns the entry off: its first byte is
    /// odd (`03` or `07`, followed by when it was turned off). A record that
    /// cannot be read does not count.
    fn disabled(&self) -> bool {
        registry::read_binary(&self.approved_key, &self.name)
            .ok()
            .flatten()
            .and_then(|record| record.first().copied())
            .is_some_and(|first| first % 2 == 1)
    }
}

/// The Run value for `exe`: `"<exe>" daemon run`. Windows paths cannot hold
/// `"`, so quoting the path is enough; a path that is not Unicode is refused.
pub fn command(exe: &Path) -> io::Result<String> {
    let exe = exe.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a Unicode path", exe.display()),
        )
    })?;
    Ok(format!("\"{exe}\" daemon run"))
}

/// Install, status, Task Manager's off switch and uninstall against scratch
/// keys under `HKCU\Software`; the real Run key is never touched.
#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch `Run` and `StartupApproved` pair, deleted with everything under
    /// it when dropped, pass or fail.
    struct Scratch {
        root: String,
        logon: Logon,
    }

    impl Scratch {
        /// Keys named after this process and the clock, so parallel and earlier
        /// runs never share them.
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_nanos())
                .unwrap_or_default();
            let root = format!(r"Software\devlanes-test-{}-{nanos}", std::process::id());
            let logon = Logon::at(
                format!(r"{root}\Run"),
                format!(r"{root}\Approved"),
                "devlanes",
            );
            Self { root, logon }
        }

        /// The raw Task Manager record.
        fn approved(&self) -> io::Result<Option<Vec<u8>>> {
            registry::read_binary(&format!(r"{}\Approved", self.root), "devlanes")
        }

        /// Writes a Task Manager record, as its Startup apps page does.
        fn set_approved(&self, record: &[u8]) -> io::Result<()> {
            registry::write_binary(&format!(r"{}\Approved", self.root), "devlanes", record)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = registry::delete_tree(&self.root);
        }
    }

    /// A Task Manager record that turns the entry off, with the time it did.
    const DISABLED: [u8; 12] = [3, 0, 0, 0, 0x2c, 0xf1, 0x65, 0x7f, 0x99, 0xed, 0xd9, 0x01];

    /// The full cycle: nothing, installed, up to date, updated for another
    /// binary, uninstalled with Task Manager's record, then nothing to remove.
    #[test]
    fn install_status_uninstall() -> io::Result<()> {
        let scratch = Scratch::new();
        let logon = &scratch.logon;
        let exe = Path::new(r"C:\Program Files\acme tools\devlanes.exe");
        assert_eq!(
            logon.status()?,
            Status {
                command: None,
                enabled: true
            }
        );
        assert_eq!(
            logon.install(exe)?,
            Installed {
                change: Change::Installed,
                re_enabled: false
            }
        );
        let expected = r#""C:\Program Files\acme tools\devlanes.exe" daemon run"#;
        assert_eq!(
            logon.status()?,
            Status {
                command: Some(expected.into()),
                enabled: true
            }
        );
        assert_eq!(logon.install(exe)?.change, Change::UpToDate);
        let other = Path::new(r"D:\tools\devlanes.exe");
        assert_eq!(logon.install(other)?.change, Change::Updated);
        assert_eq!(
            logon.status()?.command.as_deref(),
            Some(command(other)?.as_str())
        );
        // An enabled record is left alone and does not count as turned off.
        scratch.set_approved(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])?;
        assert!(logon.status()?.enabled);
        assert!(logon.uninstall()?);
        assert_eq!(scratch.approved()?, None);
        assert_eq!(logon.status()?.command, None);
        assert!(!logon.uninstall()?);
        Ok(())
    }

    /// Task Manager's off switch (an odd first byte) shows in the status, and
    /// installing again turns it back on without rewriting the Run value.
    #[test]
    fn install_turns_a_disabled_entry_back_on() -> io::Result<()> {
        let scratch = Scratch::new();
        let logon = &scratch.logon;
        let exe = Path::new(r"C:\tools\devlanes.exe");
        logon.install(exe)?;
        scratch.set_approved(&DISABLED)?;
        assert!(!logon.status()?.enabled);
        // Any odd first byte is off, not only Task Manager's usual `03`.
        let mut also_off = DISABLED;
        also_off[0] = 7;
        scratch.set_approved(&also_off)?;
        assert!(!logon.status()?.enabled);
        assert_eq!(
            logon.install(exe)?,
            Installed {
                change: Change::UpToDate,
                re_enabled: true
            }
        );
        let status = logon.status()?;
        assert!(status.enabled, "{status:?}");
        assert_eq!(
            scratch.approved()?.and_then(|r| r.first().copied()),
            Some(2)
        );
        Ok(())
    }

    /// The location names the value under `HKCU`.
    #[test]
    fn location_names_the_value() {
        assert_eq!(
            Logon::user().location(),
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run\devlanes"
        );
    }
}

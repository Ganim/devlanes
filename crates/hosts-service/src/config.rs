//! Names and paths. The service takes nothing from the environment or the
//! caller: the hosts file is the one in System32, and the user it serves is the
//! `--user-sid` argument in its service configuration, which only
//! administrators can change.

use std::io;
use std::path::PathBuf;

pub use devlanes_hosts_helper::protocol::{PIPE_NAME, SERVICE_BINARY, SERVICE_NAME};

/// Shown in the Services console.
pub const DISPLAY_NAME: &str = "devlanes hosts file writer";

/// Shown in the Services console under the display name.
pub const DESCRIPTION: &str = "Keeps the devlanes block in the hosts file up to date for one user's devlanes daemon. Installed by `devlanes certs trust --hosts`.";

/// The service's argument naming the user it answers.
pub const USER_SID_ARG: &str = "--user-sid";

/// The installer's staged copy of a new binary, in the install directory.
pub const STAGED_BINARY: &str = ".devlanes-hosts-service.new.exe";

/// Where the installer keeps the previous binary until the new one runs.
pub const BACKUP_BINARY: &str = ".devlanes-hosts-service.old.exe";

/// `%ProgramFiles%\devlanes`, from the known-folder API.
pub fn install_dir() -> io::Result<PathBuf> {
    Ok(devlanes_sys::windows::dirs::program_files()?.join("devlanes"))
}

/// `<System32>\drivers\etc\hosts`, with System32 from the API.
pub fn hosts_path() -> io::Result<PathBuf> {
    Ok(devlanes_sys::windows::dirs::system_directory()?.join(r"drivers\etc\hosts"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_come_from_the_api() {
        let hosts = hosts_path().expect("hosts path");
        assert!(hosts.is_absolute(), "{}", hosts.display());
        let lower = hosts.display().to_string().to_ascii_lowercase();
        assert!(lower.ends_with(r"\system32\drivers\etc\hosts"), "{lower}");
        assert!(install_dir().expect("install dir").ends_with("devlanes"));
    }
}

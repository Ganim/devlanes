//! Names, paths and the two registry values the service reads at start.
//!
//! Both values live under `HKEY_LOCAL_MACHINE`, which only administrators and
//! SYSTEM can change: the hosts file's directory (Windows' own `DataBasePath`) and
//! the SID of the user the service answers (`Parameters\UserSid`, written by the
//! installer). Nothing comes from the environment or the caller.

use std::io;
use std::path::PathBuf;

use devlanes_sys::windows::registry::machine;
use devlanes_sys::windows::token::Sid;

pub use devlanes_hosts_helper::protocol::{PIPE_NAME, SERVICE_BINARY, SERVICE_NAME};

/// Shown in the Services console.
pub const DISPLAY_NAME: &str = "devlanes hosts file writer";

/// Shown in the Services console under the display name.
pub const DESCRIPTION: &str = "Keeps the devlanes block in the hosts file up to date for one user's devlanes daemon. Installed by `devlanes certs trust --hosts`.";

/// The service's registry key.
pub const SERVICE_KEY: &str = r"SYSTEM\CurrentControlSet\Services\devlanes-hosts";

/// Where the installer writes the service's settings.
pub const PARAMETERS_KEY: &str = r"SYSTEM\CurrentControlSet\Services\devlanes-hosts\Parameters";

/// The value holding the SID of the user the service answers.
pub const USER_SID_VALUE: &str = "UserSid";

/// The installer's staged copy of a new binary, in the install directory.
pub const STAGED_BINARY: &str = ".devlanes-hosts-service.new.exe";

/// Where the installer keeps the previous binary until the new one runs.
pub const BACKUP_BINARY: &str = ".devlanes-hosts-service.old.exe";

/// Everything the install directory may hold. Anything else (a DLL above all,
/// which the service would load before System32's) is refused.
pub const INSTALL_ENTRIES: [&str; 3] = [SERVICE_BINARY, STAGED_BINARY, BACKUP_BINARY];

/// Windows' TCP/IP settings, where `DataBasePath` names the hosts file's directory.
const TCPIP_KEY: &str = r"SYSTEM\CurrentControlSet\Services\Tcpip\Parameters";

/// A missing or unusable registry setting.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// Reading the registry failed.
    #[error("reading HKLM\\{key}\\{value}: {source}")]
    Registry {
        /// The key.
        key: &'static str,
        /// The value.
        value: &'static str,
        /// The OS error.
        #[source]
        source: io::Error,
    },
    /// The value does not exist.
    #[error("HKLM\\{key}\\{value} is not set; run `devlanes certs trust --hosts` again")]
    Missing {
        /// The key.
        key: &'static str,
        /// The value.
        value: &'static str,
    },
    /// `UserSid` is not a SID.
    #[error("HKLM\\{PARAMETERS_KEY}\\{USER_SID_VALUE} is not a SID ({value:?}): {source}")]
    InvalidSid {
        /// What the value holds.
        value: String,
        /// Why it was refused.
        #[source]
        source: io::Error,
    },
    /// `UserSid` is a SID, but not of a person's account.
    #[error(
        "HKLM\\{PARAMETERS_KEY}\\{USER_SID_VALUE} is {0}, not a user account's SID (S-1-5-21-... or S-1-12-1-...)"
    )]
    NotAnAccount(String),
    /// `DataBasePath` is not an absolute, expanded path.
    #[error("the hosts file directory {0:?} (DataBasePath) is not an absolute path")]
    HostsDirectory(String),
    /// The Program Files directory could not be found.
    #[error("finding the Program Files directory: {0}")]
    ProgramFiles(#[source] io::Error),
}

/// `%ProgramFiles%\devlanes`, from the known-folder API.
pub fn install_dir() -> Result<PathBuf, ConfigError> {
    Ok(devlanes_sys::windows::dirs::program_files()
        .map_err(ConfigError::ProgramFiles)?
        .join("devlanes"))
}

/// The hosts file: `hosts` in the expanded `DataBasePath` directory
/// (`%SystemRoot%\System32\drivers\etc` on a stock system).
pub fn hosts_path() -> Result<PathBuf, ConfigError> {
    const VALUE: &str = "DataBasePath";
    let directory = machine::read_string(TCPIP_KEY, VALUE)
        .map_err(|source| ConfigError::Registry {
            key: TCPIP_KEY,
            value: VALUE,
            source,
        })?
        .ok_or(ConfigError::Missing {
            key: TCPIP_KEY,
            value: VALUE,
        })?;
    let path = PathBuf::from(&directory);
    // An unexpanded variable or a relative path would resolve against whatever the
    // service's current directory is.
    if directory.contains('%') || !path.is_absolute() {
        return Err(ConfigError::HostsDirectory(directory));
    }
    Ok(path.join("hosts"))
}

/// The user the service answers, from `Parameters\UserSid`. Only the SID's shape
/// is checked: the installer also asked the account database, which may not
/// answer for a domain account at boot.
pub fn configured_user() -> Result<Sid, ConfigError> {
    let value = machine::read_string(PARAMETERS_KEY, USER_SID_VALUE)
        .map_err(|source| ConfigError::Registry {
            key: PARAMETERS_KEY,
            value: USER_SID_VALUE,
            source,
        })?
        .ok_or(ConfigError::Missing {
            key: PARAMETERS_KEY,
            value: USER_SID_VALUE,
        })?;
    let user = Sid::parse(&value).map_err(|source| ConfigError::InvalidSid {
        value: value.clone(),
        source,
    })?;
    if !user.has_account_shape() {
        return Err(ConfigError::NotAnAccount(value));
    }
    Ok(user)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only: the hosts file path from this machine's registry.
    #[test]
    fn the_hosts_file_comes_from_the_registry() {
        let path = hosts_path().expect("hosts path");
        assert!(path.is_absolute(), "{}", path.display());
        let lower = path.display().to_string().to_ascii_lowercase();
        assert!(
            lower.ends_with(r"\system32\drivers\etc\hosts"),
            "{}",
            path.display()
        );
        assert!(install_dir().expect("install dir").ends_with("devlanes"));
    }
}

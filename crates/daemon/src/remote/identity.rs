//! Client side: the id and name this machine sends to every box, so a box
//! lists the same client under the same id across reconnects and restarts.

use std::{
    io::{self, Write},
    path::Path,
};

/// File under `$DEVLANES_HOME` that holds the id.
const ID_FILE: &str = "remote-client-id";

/// Hex digits of an id: 128 random bits.
const ID_LEN: usize = 32;

/// What a client daemon tells boxes about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientIdentity {
    /// Random id kept in [`ID_FILE`].
    pub id: String,
    /// This machine's host name.
    pub name: String,
}

/// Reads the id from `home`, creating it the first time (mode 0600 on Unix)
/// or replacing a damaged one, plus the host name. Blocking.
pub(crate) fn load(home: &Path) -> io::Result<ClientIdentity> {
    Ok(ClientIdentity {
        id: stored_id(home)?,
        name: host_name(),
    })
}

/// The id in `home`, written first when missing or not an id.
fn stored_id(home: &Path) -> io::Result<String> {
    let path = home.join(ID_FILE);
    loop {
        match std::fs::read_to_string(&path) {
            Ok(text) if valid(text.trim()) => return Ok(text.trim().to_owned()),
            // Something else wrote there; a fresh id beats failing every connect.
            Ok(_) => std::fs::remove_file(&path)?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::create_dir_all(home)?;
        let id = random_id()?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                writeln!(file, "{id}")?;
                file.sync_all()?;
                return Ok(id);
            }
            // Another connect created it first; read that one.
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// Whether `text` is an id this module writes.
fn valid(text: &str) -> bool {
    text.len() == ID_LEN
        && text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// 128 bits from the OS random source as lowercase hex.
fn random_id() -> io::Result<String> {
    let mut bytes = [0u8; ID_LEN / 2];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// The machine's host name, empty when unknown.
#[cfg(unix)]
fn host_name() -> String {
    rustix::system::uname()
        .nodename()
        .to_string_lossy()
        .into_owned()
}

/// The machine's host name, empty when unknown.
#[cfg(windows)]
fn host_name() -> String {
    std::env::var("COMPUTERNAME").unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_id_is_created_once_and_kept() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let first = load(dir.path())?;
        assert!(valid(&first.id), "{}", first.id);
        assert_eq!(load(dir.path())?, first, "a second read keeps the id");
        let stored = std::fs::read_to_string(dir.path().join(ID_FILE))?;
        assert_eq!(stored.trim(), first.id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(ID_FILE))?
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let other = tempfile::tempdir()?;
        assert_ne!(
            load(other.path())?.id,
            first.id,
            "each home gets its own id"
        );
        Ok(())
    }

    #[test]
    fn a_damaged_id_is_replaced() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        std::fs::write(dir.path().join(ID_FILE), b"not an id\n")?;
        let identity = load(dir.path())?;
        assert!(valid(&identity.id), "{}", identity.id);
        assert_eq!(load(dir.path())?.id, identity.id);
        Ok(())
    }
}

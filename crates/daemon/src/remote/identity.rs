//! Client side: the id and name this machine sends to every box, so a box
//! lists the same client under the same id across reconnects and restarts.

use std::{
    io::{self, Write},
    path::Path,
};

/// File under `$DEVLANES_HOME` that holds the id.
pub(crate) const ID_FILE: &str = "remote-client-id";

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
///
/// A new id is written whole to a temporary file and then hard-linked into
/// place, which fails when the file exists: a daemon reading at the same time
/// never sees a half-written id, and an id another daemon of this home just
/// stored is read instead of replaced.
fn stored_id(home: &Path) -> io::Result<String> {
    let path = home.join(ID_FILE);
    loop {
        match std::fs::read_to_string(&path) {
            Ok(text) if valid(text.trim()) => return Ok(text.trim().to_owned()),
            // Something else wrote there; a fresh id beats failing every connect.
            Ok(_) => {
                if let Err(error) = std::fs::remove_file(&path)
                    && error.kind() != io::ErrorKind::NotFound
                {
                    return Err(error);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        std::fs::create_dir_all(home)?;
        let id = random_id()?;
        let temporary = home.join(format!("{ID_FILE}.{id}.tmp"));
        write_new(&temporary, &id)?;
        let linked = std::fs::hard_link(&temporary, &path);
        let _ = std::fs::remove_file(&temporary);
        match linked {
            Ok(()) => return Ok(id),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
}

/// Creates `path` (mode 0600 on Unix) holding `id` and syncs it.
fn write_new(path: &Path, id: &str) -> io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    writeln!(file, "{id}")?;
    file.sync_all()
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

    /// Daemons creating the id at the same moment agree on one, and leave no
    /// temporary files behind.
    #[test]
    fn concurrent_creation_agrees_on_one_id() -> io::Result<()> {
        let dir = tempfile::tempdir()?;
        let ids = std::thread::scope(|scope| {
            let readers: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| stored_id(dir.path())))
                .collect();
            readers
                .into_iter()
                .map(|reader| {
                    reader
                        .join()
                        .map_err(|_| io::Error::other("reader panicked"))?
                })
                .collect::<io::Result<Vec<String>>>()
        })?;
        assert!(ids.iter().all(|id| *id == ids[0]), "{ids:?}");
        let files: Vec<_> = std::fs::read_dir(dir.path())?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<io::Result<_>>()?;
        assert_eq!(files, [ID_FILE], "{files:?}");
        Ok(())
    }
}

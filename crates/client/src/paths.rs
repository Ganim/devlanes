//! Path discovery is read-only so shell completion never creates daemon state.

use std::{
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, MetadataExt},
    },
};

pub use devlanes_core::paths::user_home;

use crate::{Error, Result};

/// Persistent state belongs to one home; sockets may use a private runtime directory.
/// On Windows `socket` holds the daemon's named pipe (`\\.\pipe\...`), which is
/// not a filesystem path.
#[derive(Clone, Debug)]
pub struct Paths {
    pub home: PathBuf,
    pub run: PathBuf,
    pub socket: PathBuf,
    pub db: PathBuf,
    pub logs: PathBuf,
}

impl Paths {
    /// Resolves overrides without creating directories, including in completion clients.
    pub fn discover() -> Result<Self> {
        Self::new(home_directory(
            env::var_os("DEVLANES_HOME"),
            user_home().map(PathBuf::into_os_string),
        )?)
    }

    /// Derives an absolute home and a socket that fits macOS's 104-byte sun_path.
    pub fn new(home: impl AsRef<Path>) -> Result<Self> {
        #[cfg(target_os = "linux")]
        let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        #[cfg(not(target_os = "linux"))]
        let runtime: Option<PathBuf> = None;
        Self::with_environment(home.as_ref(), runtime.as_deref(), &env::temp_dir())
    }

    /// Injects runtime inputs so path tests never mutate the process environment.
    fn with_environment(home: &Path, _runtime: Option<&Path>, temporary: &Path) -> Result<Self> {
        if home.as_os_str().is_empty() {
            return Err(Error::MissingHome);
        }
        #[cfg(unix)]
        let home = if home.is_absolute() {
            home.to_owned()
        } else {
            env::current_dir()?.join(home)
        };
        // Resolves `C:foo` and `\foo` the way Windows opens them; joining those
        // onto the current directory would leave them relative.
        #[cfg(windows)]
        let home = std::path::absolute(home)?;
        let run = home.join("run");
        #[cfg(target_os = "linux")]
        let mut socket = _runtime
            .and_then(|runtime| runtime_socket(runtime, &home))
            .unwrap_or_else(|| run.join("daemon.sock"));
        #[cfg(all(unix, not(target_os = "linux")))]
        let mut socket = run.join("daemon.sock");
        #[cfg(windows)]
        let socket = {
            let _ = temporary;
            pipe_name(&home)?
        };
        #[cfg(unix)]
        if socket.as_os_str().as_bytes().len() >= 104 {
            // A stable home-specific name prevents isolated homes sharing a fallback daemon.
            let name = format!("devlanes-{}", rustix::process::geteuid().as_raw());
            let file = socket_name(&home);
            socket = temporary.join(&name).join(&file);
            if socket.as_os_str().as_bytes().len() >= 104 {
                socket = Path::new("/tmp").join(name).join(file);
            }
        }
        Ok(Self {
            db: home.join("state.db"),
            logs: home.join("logs"),
            home,
            run,
            socket,
        })
    }

    /// Creates private directories before daemon startup; call off async executor threads.
    pub fn prepare(&self) -> Result<()> {
        for directory in [&self.home, &self.run, &self.logs] {
            private_directory(directory)?;
        }
        #[cfg(unix)]
        if self.socket.parent() != Some(self.run.as_path()) {
            private_directory(self.socket.parent().ok_or(Error::InvalidSocketPath)?)?;
            let mut pointer = tempfile::NamedTempFile::new_in(&self.run)?;
            pointer.write_all(self.socket.as_os_str().as_bytes())?;
            pointer.write_all(b"\n")?;
            pointer
                .persist(self.run.join("daemon.socket"))
                .map_err(|error| error.error)?;
        }
        Ok(())
    }
}

/// Picks `DEVLANES_HOME`, else `<user home>/.devlanes`; empty values count as unset.
fn home_directory(devlanes_home: Option<OsString>, home: Option<OsString>) -> Result<PathBuf> {
    match (devlanes_home, home) {
        (Some(devlanes_home), _) if !devlanes_home.is_empty() => Ok(PathBuf::from(devlanes_home)),
        // An empty HOME would make `.devlanes` relative, giving each directory its own daemon.
        (_, Some(home)) if !home.is_empty() => Ok(PathBuf::from(home).join(".devlanes")),
        _ => Err(Error::MissingHome),
    }
}

/// The daemon's pipe for `home`. Pipe names are machine-wide, so the name
/// hashes (FNV-1a) the user's SID with the home, which keeps isolated homes
/// and other users' daemons apart. The home is hashed as UTF-16 with ASCII
/// folded to lowercase and no trailing separator: Windows opens `C:\Acme\` and
/// `c:\acme` as one directory, and two spellings must not get two daemons.
#[cfg(windows)]
fn pipe_name(home: &Path) -> Result<PathBuf> {
    use std::os::windows::ffi::OsStrExt;
    let user = devlanes_sys_windows::token::current_user()?.to_string();
    let home: PathBuf = home.components().collect();
    let units = home
        .as_os_str()
        .encode_wide()
        .map(|unit| match u8::try_from(unit) {
            Ok(byte) => u16::from(byte.to_ascii_lowercase()),
            Err(_) => unit,
        });
    let hash = user
        .bytes()
        .chain(units.flat_map(u16::to_le_bytes))
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
        });
    Ok(PathBuf::from(format!(r"\\.\pipe\devlanes-{hash:016x}")))
}

/// Keeps isolated homes distinct in shared per-user runtime directories.
#[cfg(unix)]
fn socket_name(home: &Path) -> String {
    let hash = home
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf29ce484222325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
        });
    format!("daemon-{hash:016x}.sock")
}

/// Invalid XDG directories are ignored without creating or repairing their contents.
#[cfg(all(unix, any(target_os = "linux", test)))]
fn runtime_socket(runtime: &Path, home: &Path) -> Option<PathBuf> {
    if !runtime.is_absolute() {
        return None;
    }
    let metadata = fs::symlink_metadata(runtime).ok()?;
    if !metadata.is_dir()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o7777 != 0o700
    {
        return None;
    }
    let socket = runtime.join("devlanes").join(socket_name(home));
    // An unusably long runtime path must retain the existing home/temp fallback behavior.
    (socket.as_os_str().as_bytes().len() < 104).then_some(socket)
}

/// Creates a missing state directory private and refuses a symlinked one, a
/// non-directory or another user's directory. An own directory that others can
/// read passes: the daemon tightens it to 0700 when it starts and logs that once.
#[cfg(unix)]
fn private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(Error::UnsafeDirectory(path.to_owned()));
    }
    Ok(())
}

/// Creates a missing state directory private (owned by this user, access for
/// this user, SYSTEM and Administrators) and refuses a symlink or junction, a
/// non-directory or another account's directory. An own directory that others
/// can read is tightened here while it is still empty, because `prepare`
/// creates entries in the home next and Windows does not carry a changed access
/// list over to existing entries; a non-empty one passes and the daemon
/// reports it.
#[cfg(windows)]
fn private_directory(path: &Path) -> Result<()> {
    use devlanes_sys_windows::security::{Problem, ensure_private_dir};
    let checked = match fs::symlink_metadata(path) {
        Ok(_) => ensure_private_dir(path)?.map(drop),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            ensure_private_dir(path)?.map(drop)
        }
        Err(error) => return Err(error.into()),
    };
    match checked {
        Ok(()) | Err(Problem::Acl(_)) => Ok(()),
        Err(_) => Err(Error::UnsafeDirectory(path.to_owned())),
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// Each home gets its own pipe; spellings of one home share it.
    #[test]
    fn pipe_names_follow_the_home() -> Result<()> {
        let temporary = env::temp_dir();
        let first = Paths::with_environment(Path::new(r"C:\Users\acme\one"), None, &temporary)?;
        let second = Paths::with_environment(Path::new(r"C:\Users\acme\two"), None, &temporary)?;
        let socket = first.socket.to_string_lossy().into_owned();
        assert!(socket.starts_with(r"\\.\pipe\devlanes-"), "{socket}");
        assert_eq!(socket.len(), r"\\.\pipe\devlanes-".len() + 16, "{socket}");
        assert_ne!(first.socket, second.socket);
        assert_eq!(first.run, Path::new(r"C:\Users\acme\one\run"));
        assert_eq!(first.db, Path::new(r"C:\Users\acme\one\state.db"));
        for spelling in [
            r"C:\USERS\ACME\ONE",
            r"C:/Users/acme/one",
            r"C:\Users\acme\one\",
        ] {
            let other = Paths::with_environment(Path::new(spelling), None, &temporary)?;
            assert_eq!(other.socket, first.socket, "{spelling}");
        }
        Ok(())
    }

    /// A relative home resolves against the current directory, and discovery
    /// creates nothing.
    #[test]
    fn relative_homes_become_absolute() -> Result<()> {
        let paths = Paths::with_environment(Path::new("rel-home"), None, &env::temp_dir())?;
        assert!(paths.home.is_absolute(), "{}", paths.home.display());
        assert_eq!(paths.home, env::current_dir()?.join("rel-home"));
        assert!(!paths.home.exists());
        Ok(())
    }

    /// Prepared directories are private, an own empty directory others can
    /// read is tightened before anything is created in it, and a junction is
    /// refused.
    #[test]
    fn prepare_creates_private_directories() -> Result<()> {
        use devlanes_sys_windows::security::check_private_dir;
        let temp = tempfile::tempdir()?;
        let paths = Paths::new(temp.path().join("deep").join("home"))?;
        paths.prepare()?;
        for directory in [&paths.home, &paths.run, &paths.logs] {
            assert_eq!(
                check_private_dir(directory)?,
                Ok(()),
                "{}",
                directory.display()
            );
        }
        assert!(!paths.run.join("daemon.socket").exists());
        let open = temp.path().join("open");
        fs::create_dir(&open)?;
        let everyone = std::process::Command::new(
            devlanes_sys_windows::file::system_directory()?.join("icacls.exe"),
        )
        .arg(&open)
        .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
        .stdout(std::process::Stdio::null())
        .status()?;
        assert!(everyone.success());
        Paths::new(&open)?.prepare()?;
        assert_eq!(check_private_dir(&open)?, Ok(()));
        let link = temp.path().join("link");
        let junction = std::process::Command::new(
            devlanes_sys_windows::file::system_directory()?.join("cmd.exe"),
        )
        .args(["/d", "/c", "mklink", "/J"])
        .arg(&link)
        .arg(&paths.home)
        .stdout(std::process::Stdio::null())
        .status()?;
        assert!(junction.success());
        assert!(matches!(
            Paths::new(&link)?.prepare(),
            Err(Error::UnsafeDirectory(_))
        ));
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Uses a short test-owned runtime path so sun_path length cannot mask permission checks.
    fn runtime_directory() -> std::io::Result<tempfile::TempDir> {
        let directory = tempfile::Builder::new().prefix("xdg").tempdir_in("/tmp")?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))?;
        Ok(directory)
    }

    /// Validation remains testable on macOS without enabling Linux path selection there.
    #[test]
    fn runtime_validation_is_read_only_and_home_specific() -> Result<()> {
        let runtime = runtime_directory()?;
        let first = runtime_socket(runtime.path(), Path::new("/home/first"));
        let second = runtime_socket(runtime.path(), Path::new("/home/second"));
        assert_eq!(
            first,
            Some(
                runtime
                    .path()
                    .join("devlanes")
                    .join(socket_name(Path::new("/home/first")))
            )
        );
        assert_ne!(first, second);
        assert!(!runtime.path().join("devlanes").exists());
        assert!(runtime_socket(Path::new("relative"), Path::new("/home/first")).is_none());
        for mode in [0o755, 0o770, 0o600, 0o1700] {
            fs::set_permissions(runtime.path(), fs::Permissions::from_mode(mode))?;
            assert!(runtime_socket(runtime.path(), Path::new("/home/first")).is_none());
        }
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
        let link = runtime.path().join("link");
        std::os::unix::fs::symlink(runtime.path(), &link)?;
        assert!(runtime_socket(&link, Path::new("/home/first")).is_none());
        Ok(())
    }

    /// Empty variables count as unset, so an empty HOME never yields a relative home.
    #[test]
    fn empty_home_is_missing() {
        let set = |value: &str| Some(OsString::from(value));
        assert!(matches!(
            home_directory(None, set("")),
            Err(Error::MissingHome)
        ));
        assert!(matches!(
            home_directory(set(""), None),
            Err(Error::MissingHome)
        ));
        assert_eq!(
            home_directory(set(""), set("/home/acme")).ok(),
            Some(PathBuf::from("/home/acme/.devlanes"))
        );
        assert_eq!(
            home_directory(set("/srv/devlanes"), set("")).ok(),
            Some(PathBuf::from("/srv/devlanes"))
        );
    }

    /// Missing or invalid runtime settings preserve the home-local socket on every platform.
    #[test]
    fn invalid_or_unset_runtime_uses_home() -> Result<()> {
        let runtime = runtime_directory()?;
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o755))?;
        let home = runtime.path().join("home");
        for setting in [
            None,
            Some(Path::new("relative")),
            Some(runtime.path()),
            Some(runtime.path().join("missing").as_path()),
        ] {
            let paths = Paths::with_environment(&home, setting, runtime.path())?;
            assert_eq!(paths.socket, home.join("run/daemon.sock"));
            assert_eq!(paths.run, home.join("run"));
            assert_eq!(paths.db, home.join("state.db"));
            assert_eq!(paths.logs, home.join("logs"));
        }
        assert!(!home.exists());
        Ok(())
    }

    /// Linux relocates only the socket and publishes its pointer when preparing the daemon.
    #[cfg(target_os = "linux")]
    #[test]
    fn linux_runtime_socket_and_pointer() -> Result<()> {
        let runtime = runtime_directory()?;
        let home = runtime.path().join("home");
        let paths = Paths::with_environment(&home, Some(runtime.path()), runtime.path())?;
        assert_eq!(
            paths.socket,
            runtime.path().join("devlanes").join(socket_name(&home))
        );
        assert_eq!(paths.run, home.join("run"));
        assert_eq!(paths.db, home.join("state.db"));
        assert_eq!(paths.logs, home.join("logs"));
        assert!(!home.exists());
        assert!(!runtime.path().join("devlanes").exists());
        let other = Paths::with_environment(
            &runtime.path().join("other"),
            Some(runtime.path()),
            runtime.path(),
        )?;
        assert_ne!(paths.socket, other.socket);
        paths.prepare()?;
        assert_eq!(
            fs::read_to_string(paths.run.join("daemon.socket"))?,
            format!("{}\n", paths.socket.display())
        );
        assert_eq!(
            fs::metadata(runtime.path().join("devlanes"))?.mode() & 0o7777,
            0o700
        );
        Ok(())
    }

    /// macOS ignores even valid XDG settings and retains its original overlong-path fallback.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_ignores_runtime() -> Result<()> {
        let runtime = runtime_directory()?;
        let home = runtime.path().join("home");
        let paths = Paths::with_environment(&home, Some(runtime.path()), runtime.path())?;
        assert_eq!(paths.socket, home.join("run/daemon.sock"));
        let long_home = home.join("a".repeat(120));
        let long_paths = Paths::with_environment(&long_home, Some(runtime.path()), runtime.path())?;
        assert_eq!(
            long_paths.socket,
            runtime
                .path()
                .join(format!("devlanes-{}", rustix::process::geteuid().as_raw()))
                .join(socket_name(&long_home))
        );
        assert!(!home.exists());
        Ok(())
    }
}

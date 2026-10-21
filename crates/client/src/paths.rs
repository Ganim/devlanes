//! Path discovery is read-only so shell completion never creates daemon state.

#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{DirBuilderExt, MetadataExt},
};
use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
};

use crate::{Error, Result};

/// Persistent state belongs to one home; sockets may use a private runtime directory.
/// On Windows `socket` holds the daemon's named-pipe name (`\\.\pipe\...`).
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
        let home = match env::var_os("DEVLANES_HOME") {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => user_home().ok_or(Error::MissingHome)?.join(".devlanes"),
        };
        Self::new(home)
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
        let home = if home.is_absolute() {
            home.to_owned()
        } else {
            env::current_dir()?.join(home)
        };
        let run = home.join("run");
        let socket = socket_path(&home, &run, _runtime, temporary);
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
        publish_socket(self)
    }
}

/// The user's home directory: `HOME` on Unix, the profile directory on Windows
/// (`std::env::home_dir` there ignores `HOME`, which Git Bash sets to a POSIX path).
pub fn user_home() -> Option<PathBuf> {
    #[cfg(unix)]
    let home = env::var_os("HOME").map(PathBuf::from);
    #[cfg(windows)]
    let home = env::home_dir();
    home.filter(|home| !home.as_os_str().is_empty())
}

/// Picks the socket: the XDG runtime directory on Linux, else `run/daemon.sock`,
/// moved to a temporary directory when the path exceeds macOS's 104-byte sun_path.
#[cfg(unix)]
fn socket_path(home: &Path, run: &Path, _runtime: Option<&Path>, temporary: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    let mut socket = _runtime
        .and_then(|runtime| runtime_socket(runtime, home))
        .unwrap_or_else(|| run.join("daemon.sock"));
    #[cfg(not(target_os = "linux"))]
    let mut socket = run.join("daemon.sock");
    if socket.as_os_str().as_bytes().len() >= 104 {
        // A stable home-specific name prevents isolated homes sharing a fallback daemon.
        let name = format!("devlanes-{}", rustix::process::geteuid().as_raw());
        let file = socket_name(home);
        socket = temporary.join(&name).join(&file);
        if socket.as_os_str().as_bytes().len() >= 104 {
            socket = Path::new("/tmp").join(name).join(file);
        }
    }
    socket
}

/// A named pipe per home, so isolated homes (tests, `DEVLANES_HOME`) never share a
/// daemon. Pipe names live in a machine-wide namespace; the pipe's security
/// descriptor, not its name, keeps other users out.
#[cfg(windows)]
fn socket_path(home: &Path, _run: &Path, _runtime: Option<&Path>, _temporary: &Path) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\devlanes-{:016x}", home_hash(home)))
}

/// Publishes a relocated socket's path in `run/daemon.socket` so tools can find it.
#[cfg(unix)]
fn publish_socket(paths: &Paths) -> Result<()> {
    if paths.socket.parent() != Some(paths.run.as_path()) {
        private_directory(paths.socket.parent().ok_or(Error::InvalidSocketPath)?)?;
        write_pointer(&paths.run, paths.socket.as_os_str().as_bytes())?;
    }
    Ok(())
}

/// Publishes the pipe name in `run/daemon.socket`; a pipe never lives under `run`.
#[cfg(windows)]
fn publish_socket(paths: &Paths) -> Result<()> {
    write_pointer(&paths.run, paths.socket.to_string_lossy().as_bytes())
}

/// Replaces `run/daemon.socket` atomically with `target` and a newline.
fn write_pointer(run: &Path, target: &[u8]) -> Result<()> {
    let mut pointer = tempfile::NamedTempFile::new_in(run)?;
    pointer.write_all(target)?;
    pointer.write_all(b"\n")?;
    pointer
        .persist(run.join("daemon.socket"))
        .map_err(|error| error.error)?;
    Ok(())
}

/// Keeps isolated homes distinct in shared per-user runtime directories.
#[cfg(unix)]
fn socket_name(home: &Path) -> String {
    format!("daemon-{:016x}.sock", home_hash(home))
}

/// FNV-1a over the home path. Windows hashes the ASCII-lowercased UTF-16 form,
/// because two spellings that differ only in case name the same directory there.
fn home_hash(home: &Path) -> u64 {
    #[cfg(unix)]
    let bytes = home.as_os_str().as_bytes().to_vec();
    #[cfg(windows)]
    let bytes: Vec<u8> = {
        use std::os::windows::ffi::OsStrExt;
        home.as_os_str()
            .encode_wide()
            .map(|unit| match u8::try_from(unit) {
                Ok(byte) => u16::from(byte.to_ascii_lowercase()),
                Err(_) => unit,
            })
            .flat_map(u16::to_le_bytes)
            .collect()
    };
    bytes.iter().fold(0xcbf29ce484222325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
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

/// Creates missing directories and refuses symlinks, junctions and files.
/// Ownership and ACL checks are not implemented on Windows yet.
#[cfg(windows)]
fn private_directory(path: &Path) -> Result<()> {
    use std::os::windows::fs::MetadataExt;
    // FILE_ATTRIBUTE_REPARSE_POINT: symlinks and junctions both carry it.
    const REPARSE_POINT: u32 = 0x400;
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(path)?;
        }
        Err(error) => return Err(error.into()),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_attributes() & REPARSE_POINT != 0 {
        return Err(Error::UnsafeDirectory(path.to_owned()));
    }
    Ok(())
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    /// Each home gets its own pipe; case differences name the same home.
    #[test]
    fn pipe_names_follow_the_home() -> Result<()> {
        let temporary = env::temp_dir();
        let first = Paths::with_environment(Path::new(r"C:\Users\acme\one"), None, &temporary)?;
        let second = Paths::with_environment(Path::new(r"C:\Users\acme\two"), None, &temporary)?;
        let upper = Paths::with_environment(Path::new(r"C:\USERS\ACME\ONE"), None, &temporary)?;
        assert!(
            first
                .socket
                .to_string_lossy()
                .starts_with(r"\\.\pipe\devlanes-")
        );
        assert_ne!(first.socket, second.socket);
        assert_eq!(first.socket, upper.socket);
        assert_eq!(first.run, Path::new(r"C:\Users\acme\one\run"));
        Ok(())
    }

    /// Preparing creates the private directories and publishes the pipe name.
    #[test]
    fn prepare_writes_the_pointer() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let paths =
            Paths::with_environment(&directory.path().join("home"), None, directory.path())?;
        paths.prepare()?;
        assert!(paths.logs.is_dir());
        assert_eq!(
            fs::read_to_string(paths.run.join("daemon.socket"))?,
            format!("{}\n", paths.socket.display())
        );
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

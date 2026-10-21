//! Test-owned homes and Git repositories. Helpers never mutate the caller's environment.
//!
//! ```
//! let home = devlanes_testing::TestHome::new()?;
//! let mut command = std::process::Command::new("devlanes");
//! home.configure(&mut command);
//! assert!(home.path().join("run").is_dir());
//! # Ok::<(), devlanes_testing::Error>(())
//! ```

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::Path;
use std::process::{Command, Output};

/// Failures creating isolated test resources or invoking Git.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// An operating-system operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Git rejected a command; stderr is retained for test diagnostics.
    #[error("git failed: {0}")]
    Git(String),
}

/// A short, private home removed on drop. Stop owned processes before dropping it.
#[derive(Debug)]
pub struct TestHome(tempfile::TempDir);

impl TestHome {
    /// Creates a home with a run directory short enough for macOS Unix sockets.
    ///
    /// This blocking filesystem helper should run outside async executor threads.
    #[cfg(unix)]
    pub fn new() -> Result<Self, Error> {
        // macOS TMPDIR is often too long for a nested daemon socket path.
        let dir = tempfile::Builder::new()
            .prefix("dl-")
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir_in("/tmp")?;
        // The client and daemon refuse a home or run directory that others can read.
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(dir.path().join("run"))?;
        Ok(Self(dir))
    }

    /// Creates a home with a run directory in the user's temp directory.
    ///
    /// There are no mode bits to set; the home inherits the ACL of the temp directory
    /// in the user profile. This blocking filesystem helper should run outside async
    /// executor threads.
    #[cfg(windows)]
    pub fn new() -> Result<Self, Error> {
        let dir = tempfile::Builder::new().prefix("dl-").tempdir()?;
        std::fs::create_dir(dir.path().join("run"))?;
        Ok(Self(dir))
    }

    /// Returns the home path while this owner is alive.
    pub fn path(&self) -> &Path {
        self.0.path()
    }

    /// Applies isolation to one command without changing process-global variables.
    pub fn configure(&self, command: &mut Command) {
        command.env("DEVLANES_HOME", self.path());
        command.env("HOME", self.path());
        // `std::env::home_dir` and most Windows tools read the profile, not HOME.
        #[cfg(windows)]
        command.env("USERPROFILE", self.path());
        command.env("XDG_CONFIG_HOME", self.path().join("config"));
        command.env("XDG_DATA_HOME", self.path().join("data"));
        command.env("XDG_CACHE_HOME", self.path().join("cache"));
    }
}

/// A temporary Git repository with a committed `main` branch and local identity.
#[derive(Debug)]
pub struct TestRepo {
    dir: tempfile::TempDir,
    /// Empty file used as the global Git config. It lives outside the repository so
    /// it never shows up as an untracked file.
    global_config: tempfile::TempPath,
    /// Empty directory used as `core.hooksPath`, outside the repository for the
    /// same reason.
    hooks: tempfile::TempDir,
}

impl TestRepo {
    /// Initializes a repository using only test-local Git settings.
    ///
    /// Requires Git on PATH and performs blocking subprocess work.
    pub fn new() -> Result<Self, Error> {
        let repo = Self {
            dir: tempfile::Builder::new().prefix("devlanes-git-").tempdir()?,
            global_config: empty_git_config()?,
            hooks: tempfile::Builder::new()
                .prefix("devlanes-hooks-")
                .tempdir()?,
        };
        repo.git(&["init", "--initial-branch=main"])?;
        repo.git(&["config", "user.name", "Fixture Author"])?;
        repo.git(&["config", "user.email", "fixture@example.invalid"])?;
        repo.git(&[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "--allow-empty",
            "-m",
            "fixture",
        ])?;
        Ok(repo)
    }

    /// Returns the repository root, which is removed on drop.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Runs Git in this repository, returning a typed error on unsuccessful exit.
    ///
    /// Global configuration, hooks and inherited repository selectors are disabled.
    pub fn git(&self, args: &[&str]) -> Result<Output, Error> {
        let mut command = Command::new("git");
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        // `/dev/null` would resolve to `<drive>:\dev\null` on Windows.
        let mut hooks = std::ffi::OsString::from("core.hooksPath=");
        hooks.push(self.hooks.path());
        let output = command
            .current_dir(self.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &self.global_config)
            .arg("-c")
            .arg(hooks)
            .args(["-c", "init.templateDir="])
            .args(args)
            .output()?;
        if !output.status.success() {
            return Err(Error::Git(
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ));
        }
        Ok(output)
    }
}

/// Creates an empty file to point `GIT_CONFIG_GLOBAL` at, removed on drop.
///
/// Git reads it like `/dev/null`, which is not a file path on Windows.
pub fn empty_git_config() -> Result<tempfile::TempPath, Error> {
    Ok(tempfile::Builder::new()
        .prefix("devlanes-gitconfig-")
        .tempfile()?
        .into_temp_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Home paths support Unix sockets and are removed with their owner.
    #[cfg(unix)]
    #[test]
    fn home_lifetime_and_socket() -> Result<(), Error> {
        let home = TestHome::new()?;
        let path = home.path().to_owned();
        let socket = std::os::unix::net::UnixListener::bind(path.join("run/daemon.sock"))?;
        let mut command = Command::new("unused");
        home.configure(&mut command);
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "DEVLANES_HOME" && value == Some(path.as_os_str()))
        );
        drop(socket);
        drop(home);
        assert!(!path.exists());
        Ok(())
    }

    /// Homes point both HOME and USERPROFILE at themselves and are removed with
    /// their owner.
    #[cfg(windows)]
    #[test]
    fn home_lifetime_and_profile() -> Result<(), Error> {
        let home = TestHome::new()?;
        let path = home.path().to_owned();
        assert!(path.join("run").is_dir());
        let mut command = Command::new("unused");
        home.configure(&mut command);
        for name in ["DEVLANES_HOME", "HOME", "USERPROFILE"] {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == name && value == Some(path.as_os_str())),
                "{name}"
            );
        }
        drop(home);
        assert!(!path.exists());
        Ok(())
    }

    /// A fresh repository can create linked worktrees without user Git config.
    #[test]
    fn repo_has_identity_and_worktree() -> Result<(), Error> {
        let repo = TestRepo::new()?;
        repo.git(&["worktree", "add", "-b", "feature", "linked"])?;
        let output = repo.git(&["worktree", "list", "--porcelain"])?;
        assert!(String::from_utf8_lossy(&output.stdout).contains("refs/heads/feature"));
        assert!(repo.git(&["not-a-command"]).is_err());
        Ok(())
    }
}

//! The shell that runs `run:` strings and `ready.cmd` probes.
//!
//! Config strings are written for a POSIX `sh`. Unix uses `/bin/sh`; Windows
//! uses Git for Windows' `sh.exe` when the daemon finds it and `cmd.exe`
//! otherwise. Lane resolution builds launch commands from a [`Shell`] and the
//! daemon runs ready probes with the same one. Finding Git's `sh.exe` needs a
//! process, so the daemon does that once at startup and passes the result in.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Variable that stops Git for Windows' MSYS from rewriting arguments and
/// environment values that look like POSIX paths when it starts a native
/// program (`/api` would arrive as `C:/Program Files/Git/api`, `/a:/b` as a
/// `;` list). Measured with Git for Windows 2.x: without it, `X=/api` reaches
/// `cmd.exe` started by `sh.exe` as `C:/Program Files/Git/api`; with it, as
/// `/api`.
pub const MSYS_NO_PATHCONV: &str = "MSYS_NO_PATHCONV";

/// Upstream MSYS2's switch for the same argument conversion, set as `*` so an
/// `sh.exe` built on the plain MSYS2 runtime also leaves arguments alone. It
/// does not cover environment values; [`MSYS_NO_PATHCONV`] does in Git for
/// Windows.
pub const MSYS2_ARG_CONV_EXCL: &str = "MSYS2_ARG_CONV_EXCL";

/// Syntax and quoting rules of a [`Shell`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    /// A POSIX `sh`: `/bin/sh` on Unix, Git for Windows' `sh.exe` on Windows.
    Posix,
    /// `cmd.exe /d /s /c`, the Windows fallback without Git for Windows. Config
    /// strings use POSIX syntax, so some of them fail here. The text must reach
    /// `cmd.exe` verbatim inside one pair of quotes (`/s` strips them), not with
    /// the usual `\"` escaping, which `cmd.exe` does not understand.
    Cmd,
}

/// A shell command prefix: `program`, then `args`, then the command text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub kind: ShellKind,
    pub program: String,
    /// Arguments before the command text, such as `-c`.
    pub args: Vec<String>,
    /// Variables the shell needs on top of the unit's environment.
    pub env: BTreeMap<String, String>,
}

impl Shell {
    /// `/bin/sh -c`, the Unix shell.
    pub fn posix() -> Self {
        Self {
            kind: ShellKind::Posix,
            program: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned()],
            env: BTreeMap::new(),
        }
    }

    /// Git for Windows' `sh.exe` at `program`, with MSYS path conversion off so
    /// arguments and variables reach native programs as written.
    pub fn git_sh(program: &Path) -> Self {
        Self {
            kind: ShellKind::Posix,
            program: program.display().to_string(),
            args: vec!["-c".to_owned()],
            env: BTreeMap::from([
                (MSYS_NO_PATHCONV.to_owned(), "1".to_owned()),
                (MSYS2_ARG_CONV_EXCL.to_owned(), "*".to_owned()),
            ]),
        }
    }

    /// `<system_root>\System32\cmd.exe /d /s /c`. `/d` skips AutoRun commands
    /// from the registry; the path is absolute so `PATH` cannot substitute it.
    pub fn cmd(system_root: &Path) -> Self {
        Self {
            kind: ShellKind::Cmd,
            program: system_root
                .join("System32")
                .join("cmd.exe")
                .display()
                .to_string(),
            args: vec!["/d".to_owned(), "/s".to_owned(), "/c".to_owned()],
            env: BTreeMap::new(),
        }
    }

    /// The full argv that runs `text`.
    pub fn argv(&self, text: &str) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 2);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv.push(text.to_owned());
        argv
    }

    /// Whether this is the `cmd.exe` fallback, which `config check` and launch
    /// notes warn about.
    pub fn is_fallback(&self) -> bool {
        self.kind == ShellKind::Cmd
    }
}

impl Default for Shell {
    /// `/bin/sh -c` on Unix; `cmd.exe` on Windows until the daemon finds Git's
    /// `sh.exe` (see [`git_sh_candidates`]).
    fn default() -> Self {
        os::default_shell()
    }
}

/// Directories under a Git for Windows root that hold the native toolchain's
/// `bin` (`clangarm64` on ARM64, `mingw32` in 32-bit installs).
const GIT_TOOLCHAIN_DIRS: [&str; 3] = ["mingw64", "clangarm64", "mingw32"];

/// The Git for Windows install root for a `git.exe` found on `PATH`, from the
/// layouts the installer and MinGit create: `<root>\cmd\git.exe`,
/// `<root>\bin\git.exe` and `<root>\mingw64\bin\git.exe`. `None` for anything
/// else, such as a scoop shim or MSYS2's own `usr\bin\git.exe`, whose `sh.exe`
/// does not know [`MSYS_NO_PATHCONV`]. Names compare ignoring ASCII case, as
/// Windows does.
pub fn git_root_from_exe(git: &Path) -> Option<PathBuf> {
    let named = |path: &Path, name: &str| {
        path.file_name()
            .is_some_and(|found| found.eq_ignore_ascii_case(name))
    };
    if !named(git, "git.exe") {
        return None;
    }
    let dir = git.parent()?;
    let up = dir.parent()?;
    let root = if named(dir, "cmd") {
        up
    } else if named(dir, "bin") && GIT_TOOLCHAIN_DIRS.iter().any(|name| named(up, name)) {
        up.parent()?
    } else if named(dir, "bin") && !named(up, "usr") {
        up
    } else {
        return None;
    };
    (!root.as_os_str().is_empty()).then(|| root.to_path_buf())
}

/// The Git for Windows install root for what `git --exec-path` printed
/// (`<root>\mingw64\libexec\git-core`). Only an absolute path counts, which on
/// Windows means one with a drive or UNC prefix: MSYS2's and Cygwin's Git print
/// `/usr/lib/git-core`, which is not a Windows path.
pub fn git_root_from_exec_path(exec_path: &Path) -> Option<PathBuf> {
    if !exec_path.is_absolute() {
        return None;
    }
    exec_path
        .ancestors()
        .nth(3)
        .filter(|root| !root.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

/// Where `sh.exe` can be under a Git for Windows `root`, in preference order:
/// `<root>\bin\sh.exe` (a launcher that puts Git's `usr\bin` tools on `PATH`),
/// then `<root>\usr\bin\sh.exe` (MinGit ships only this one).
pub fn git_sh_candidates(root: &Path) -> [PathBuf; 2] {
    [
        root.join("bin").join("sh.exe"),
        root.join("usr").join("bin").join("sh.exe"),
    ]
}

#[cfg(unix)]
mod os {
    use super::Shell;

    /// `/bin/sh -c`.
    pub(super) fn default_shell() -> Shell {
        Shell::posix()
    }
}

#[cfg(windows)]
mod os {
    use super::Shell;
    use std::path::PathBuf;

    /// `cmd.exe` under `%SystemRoot%`, or under `C:\Windows` when the variable
    /// is missing from this process.
    pub(super) fn default_shell() -> Shell {
        let root = std::env::var_os("SystemRoot")
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        Shell::cmd(&root)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    /// The text is the last argument after the shell's own arguments.
    fn argv_appends_the_text() {
        assert_eq!(
            Shell::posix().argv("echo hi && exit 0"),
            ["/bin/sh", "-c", "echo hi && exit 0"]
        );
        let cmd = Shell::cmd(Path::new("W"));
        let program = Path::new("W").join("System32").join("cmd.exe");
        assert_eq!(
            cmd.argv("echo hi"),
            [&program.display().to_string(), "/d", "/s", "/c", "echo hi"]
        );
        assert!(cmd.is_fallback());
        assert!(!Shell::posix().is_fallback());
    }

    #[test]
    /// Git's sh turns MSYS path conversion off; the others add no variables.
    fn git_sh_disables_path_conversion() {
        let sh = Shell::git_sh(Path::new("sh.exe"));
        assert_eq!(sh.kind, ShellKind::Posix);
        assert_eq!(sh.args, ["-c"]);
        assert_eq!(sh.env[MSYS_NO_PATHCONV], "1");
        assert_eq!(sh.env[MSYS2_ARG_CONV_EXCL], "*");
        assert!(Shell::posix().env.is_empty());
        assert!(Shell::cmd(Path::new("W")).env.is_empty());
    }

    /// An absolute directory to build install layouts under.
    fn install_root() -> PathBuf {
        std::env::temp_dir().join("Git")
    }

    #[test]
    /// The installer's `cmd` and `bin` and the toolchain `bin` all lead to the
    /// install root, ignoring case; other layouts do not.
    fn git_root_comes_from_the_git_exe_layout() {
        let root = install_root();
        for git in [
            root.join("cmd").join("git.exe"),
            root.join("bin").join("git.exe"),
            root.join("mingw64").join("bin").join("git.exe"),
            root.join("clangarm64").join("bin").join("git.exe"),
            root.join("CMD").join("Git.EXE"),
            root.join("MinGW64").join("BIN").join("git.exe"),
        ] {
            assert_eq!(git_root_from_exe(&git), Some(root.clone()), "{git:?}");
        }
        for git in [
            root.join("usr").join("bin").join("git.exe"),
            root.join("shims").join("git.exe"),
            root.join("cmd").join("git"),
            root.join("cmd").join("sh.exe"),
            PathBuf::from("cmd").join("git.exe"),
            PathBuf::from("git.exe"),
        ] {
            assert_eq!(git_root_from_exe(&git), None, "{git:?}");
        }
    }

    #[test]
    /// The root is three levels above an absolute `git --exec-path`, and
    /// `sh.exe` is looked up under it, the launcher first.
    fn git_root_comes_from_the_exec_path() {
        let root = install_root();
        let exec = root.join("mingw64").join("libexec").join("git-core");
        assert_eq!(git_root_from_exec_path(&exec), Some(root.clone()));
        assert_eq!(
            git_sh_candidates(&root),
            [
                root.join("bin").join("sh.exe"),
                root.join("usr").join("bin").join("sh.exe")
            ]
        );
        let relative = Path::new("Git")
            .join("mingw64")
            .join("libexec")
            .join("git-core");
        assert_eq!(git_root_from_exec_path(&relative), None);
        // Too short to have a root three levels up: `C:\git-core` or `/git-core`.
        let top = root.ancestors().last().unwrap_or(&root).join("git-core");
        assert_eq!(git_root_from_exec_path(&top), None);
    }

    #[cfg(windows)]
    #[test]
    /// MSYS2's and Cygwin's Git print a POSIX exec path, which has no drive and
    /// is not a Windows path; drive and UNC exec paths are.
    fn windows_exec_path_needs_a_drive_or_share() {
        assert_eq!(
            git_root_from_exec_path(Path::new("/usr/lib/git-core")),
            None
        );
        assert_eq!(
            git_root_from_exec_path(Path::new(r"\mingw64\libexec\git-core")),
            None
        );
        assert_eq!(
            git_root_from_exec_path(Path::new("C:/Program Files/Git/mingw64/libexec/git-core")),
            Some(PathBuf::from("C:/Program Files/Git"))
        );
        assert_eq!(
            git_root_from_exec_path(Path::new(r"\\host\tools\Git\mingw64\libexec\git-core")),
            Some(PathBuf::from(r"\\host\tools\Git"))
        );
    }

    #[cfg(unix)]
    #[test]
    /// Unix always runs `/bin/sh`.
    fn unix_default_is_bin_sh() {
        assert_eq!(Shell::default(), Shell::posix());
    }

    #[cfg(windows)]
    #[test]
    /// Without detection Windows falls back to an absolute `cmd.exe`.
    fn windows_default_is_cmd() {
        let shell = Shell::default();
        assert_eq!(shell.kind, ShellKind::Cmd);
        assert!(Path::new(&shell.program).is_absolute(), "{}", shell.program);
        assert!(
            shell
                .program
                .to_ascii_lowercase()
                .ends_with(r"\system32\cmd.exe")
        );
    }
}

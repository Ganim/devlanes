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

/// Variable that stops MSYS from rewriting arguments that look like POSIX
/// paths (`/api` would reach a native program as `C:/Program Files/Git/api`).
/// Git for Windows honours it.
pub const MSYS_NO_PATHCONV: &str = "MSYS_NO_PATHCONV";

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
    /// arguments reach native programs as written.
    pub fn git_sh(program: &Path) -> Self {
        Self {
            kind: ShellKind::Posix,
            program: program.display().to_string(),
            args: vec!["-c".to_owned()],
            env: BTreeMap::from([(MSYS_NO_PATHCONV.to_owned(), "1".to_owned())]),
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

/// Where `sh.exe` can be for a Git for Windows install whose `git --exec-path`
/// printed `exec_path` (`<root>/mingw64/libexec/git-core`, or `clangarm64`
/// on ARM64), in preference order: `<root>/bin/sh.exe` (a launcher that puts
/// Git's `usr/bin` tools on `PATH`), then `<root>/usr/bin/sh.exe` (MinGit
/// ships only this one).
pub fn git_sh_candidates(exec_path: &Path) -> Vec<PathBuf> {
    let Some(root) = exec_path
        .ancestors()
        .nth(3)
        .filter(|root| !root.as_os_str().is_empty())
    else {
        return Vec::new();
    };
    vec![
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
        assert!(Shell::posix().env.is_empty());
    }

    #[test]
    /// `sh.exe` is looked up three levels above `git --exec-path`.
    fn candidates_come_from_the_git_root() {
        let root = Path::new("Git");
        let exec = root.join("mingw64").join("libexec").join("git-core");
        assert_eq!(
            git_sh_candidates(&exec),
            [
                root.join("bin").join("sh.exe"),
                root.join("usr").join("bin").join("sh.exe")
            ]
        );
        assert!(git_sh_candidates(Path::new("git-core")).is_empty());
        assert!(git_sh_candidates(Path::new("libexec/git-core/x")).is_empty());
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

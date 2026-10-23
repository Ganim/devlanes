//! Git for Windows' `sh.exe`, which runs `run:` strings and `ready.cmd` on
//! Windows in place of `/bin/sh`, and how it is started.
//!
//! ```no_run
//! # #[cfg(windows)]
//! # async fn demo() {
//! use std::collections::BTreeMap;
//!
//! if let Some(shell) = devlanes_shell_windows::windows_shell().await {
//!     let mut env = BTreeMap::from([("PATH".to_owned(), r"C:\Windows".to_owned())]);
//!     devlanes_shell_windows::shell_env(&mut env, &shell);
//!     let line = devlanes_shell_windows::shell_command_line(&shell, "echo ready");
//!     # let _ = line;
//! }
//! # }
//! ```

// macOS and Linux run `/bin/sh` directly.
#![cfg(windows)]
#![deny(unsafe_code)]

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

/// Bound on `git --exec-path`, which answers at once unless something is
/// badly wrong (a hung network drive on `PATH`).
const EXEC_PATH_TIMEOUT: Duration = Duration::from_secs(10);

/// Variables every run of the shell gets, so Git's MSYS2 runtime passes
/// arguments and environment values to Windows programs unchanged instead of
/// rewriting what looks like a POSIX path. `MSYS_NO_PATHCONV` is Git for
/// Windows' switch; `MSYS2_ARG_CONV_EXCL` is upstream MSYS2's for arguments.
const SHELL_ENV: [(&str, &str); 2] = [("MSYS_NO_PATHCONV", "1"), ("MSYS2_ARG_CONV_EXCL", "*")];

/// Prepares `env` for Git's `sh.exe` at `shell`: sets the MSYS variables over
/// any spelling of them, and appends the shell's own directory
/// (`<root>\usr\bin`) to the end of `PATH`. Scripts then find Git's `sleep`,
/// `grep` and `sed`, while programs found earlier on `PATH` still win. `PATH`
/// keeps its spelling; without one, `PATH` is that directory alone.
pub fn shell_env(env: &mut BTreeMap<String, String>, shell: &Path) {
    for (name, value) in SHELL_ENV {
        env.retain(|key, _| !key.eq_ignore_ascii_case(name));
        env.insert(name.to_owned(), value.to_owned());
    }
    let Some(tools) = shell.parent().map(|dir| dir.to_string_lossy().into_owned()) else {
        return;
    };
    let key = env
        .keys()
        .find(|key| key.eq_ignore_ascii_case("PATH"))
        .cloned()
        .unwrap_or_else(|| "PATH".to_owned());
    let path = env.entry(key).or_default();
    if path.split(';').any(|dir| dir.eq_ignore_ascii_case(&tools)) {
        return;
    }
    if !path.is_empty() && !path.ends_with(';') {
        path.push(';');
    }
    path.push_str(&tools);
}

/// The command line that runs `script` with the shell at `shell`:
/// `"<shell>" "-c" "<script>"`, each part quoted by [`quote_msys`].
pub fn shell_command_line(shell: &Path, script: &str) -> OsString {
    format!(
        "{} {} {}",
        quote_msys(&shell.to_string_lossy()),
        quote_msys("-c"),
        quote_msys(script)
    )
    .into()
}

/// `arg` quoted for the MSYS2 runtime's command-line parser, which Git's
/// `sh.exe` uses. Inside double quotes it reads `\\` as `\` and `\"` as `"`,
/// unlike the C runtime, where backslashes only count before a quote; an
/// unquoted argument would also be glob-expanded (`*.ts`) and could name a
/// response file (`@file`). So every argument is quoted.
pub fn quote_msys(arg: &str) -> String {
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    for character in arg.chars() {
        if character == '\\' || character == '"' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

/// Directories under a Git for Windows root that hold the native `git.exe`
/// (`clangarm64` on ARM64, `mingw32` in 32-bit installs).
const TOOLCHAIN_DIRS: [&str; 3] = ["mingw64", "clangarm64", "mingw32"];

/// Git for Windows' `<root>\usr\bin\sh.exe`, for the `git` this process
/// finds on `PATH`; `None` when there is none.
///
/// The root comes from `git --exec-path` (`<root>/mingw64/libexec/git-core`),
/// else from where `git.exe` sits on `PATH` (`<root>\cmd`, `<root>\bin` or
/// `<root>\mingw64\bin`). MSYS2's and Cygwin's own Git print POSIX paths and
/// are not used: their `sh.exe` ignores `MSYS_NO_PATHCONV`.
pub async fn windows_shell() -> Option<PathBuf> {
    if let Some(root) = exec_path_root().await
        && let Some(shell) = shell_in(&root).await
    {
        return Some(shell);
    }
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let git = dir.join("git.exe");
        if !tokio::fs::metadata(&git).await.is_ok_and(|m| m.is_file()) {
            continue;
        }
        if let Some(root) = root_from_exe(&git)
            && let Some(shell) = shell_in(&root).await
        {
            return Some(shell);
        }
    }
    None
}

/// The install root from `git --exec-path`.
async fn exec_path_root() -> Option<PathBuf> {
    let output = tokio::time::timeout(
        EXEC_PATH_TIMEOUT,
        Command::new("git")
            .arg("--exec-path")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    // Git prints `/` separators; logs and messages read better with native ones.
    let text = String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .replace('/', "\\");
    root_from_exec_path(Path::new(&text))
}

/// `<root>\usr\bin\sh.exe` when it exists.
async fn shell_in(root: &Path) -> Option<PathBuf> {
    let shell = root.join("usr").join("bin").join("sh.exe");
    tokio::fs::metadata(&shell)
        .await
        .is_ok_and(|m| m.is_file())
        .then_some(shell)
}

/// The install root three levels above `<root>/mingw64/libexec/git-core`.
/// Only an absolute Windows path counts: MSYS2's and Cygwin's Git print
/// `/usr/lib/git-core`.
fn root_from_exec_path(exec_path: &Path) -> Option<PathBuf> {
    if !exec_path.is_absolute() {
        return None;
    }
    exec_path
        .ancestors()
        .nth(3)
        .filter(|root| root.parent().is_some())
        .map(Path::to_path_buf)
}

/// The install root for a `git.exe` in one of the layouts the Git for
/// Windows installer and MinGit create; `None` for anything else, such as a
/// shim or MSYS2's `usr\bin\git.exe`. Names compare ignoring ASCII case.
fn root_from_exe(git: &Path) -> Option<PathBuf> {
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
    } else if named(dir, "bin") && TOOLCHAIN_DIRS.iter().any(|name| named(up, name)) {
        up.parent()?
    } else if named(dir, "bin") && !named(up, "usr") {
        up
    } else {
        return None;
    };
    Some(root.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `--exec-path` output, with either separator, gives the install root;
    /// POSIX paths and paths too short to have one give none.
    #[test]
    fn roots_from_exec_paths() {
        for (exec_path, root) in [
            (
                "C:/Program Files/Git/mingw64/libexec/git-core",
                Some("C:/Program Files/Git"),
            ),
            (
                r"D:\tools\git\clangarm64\libexec\git-core",
                Some(r"D:\tools\git"),
            ),
            ("/usr/lib/git-core", None),
            (r"C:\libexec\git-core", None),
        ] {
            assert_eq!(
                root_from_exec_path(Path::new(exec_path)),
                root.map(PathBuf::from),
                "{exec_path}"
            );
        }
    }

    /// The installer's and MinGit's layouts give the root; other places
    /// where a `git.exe` can be do not.
    #[test]
    fn roots_from_git_exe() {
        for (git, root) in [
            (r"C:\Git\cmd\git.exe", Some(r"C:\Git")),
            (r"C:\Git\CMD\GIT.EXE", Some(r"C:\Git")),
            (r"C:\Git\bin\git.exe", Some(r"C:\Git")),
            (r"C:\Git\mingw64\bin\git.exe", Some(r"C:\Git")),
            (r"C:\msys64\usr\bin\git.exe", None),
            (r"C:\scoop\shims\git.exe", None),
            (r"C:\Git\cmd\git-lfs.exe", None),
        ] {
            assert_eq!(
                root_from_exe(Path::new(git)),
                root.map(PathBuf::from),
                "{git}"
            );
        }
    }

    /// MSYS quoting always quotes and escapes every backslash and quote.
    #[test]
    fn msys_quoting() {
        for (arg, expected) in [
            ("plain", r#""plain""#),
            ("", r#""""#),
            ("*.ts", r#""*.ts""#),
            (r"a\b", r#""a\\b""#),
            (r"trailing\", r#""trailing\\""#),
            (r#"a"b"#, r#""a\"b""#),
            ("line one\nline two", "\"line one\nline two\""),
        ] {
            assert_eq!(quote_msys(arg), expected, "{arg:?}");
        }
        assert_eq!(
            shell_command_line(Path::new(r"C:\Git\usr\bin\sh.exe"), r#"echo "a\b""#),
            OsString::from(r#""C:\\Git\\usr\\bin\\sh.exe" "-c" "echo \"a\\b\"""#)
        );
    }

    /// The shell's directory goes to the end of `PATH` once, under the
    /// spelling `PATH` already has, and the MSYS variables replace any
    /// spelling of their own.
    #[test]
    fn shell_environment() {
        let shell = Path::new(r"C:\Git\usr\bin\sh.exe");
        let mut env = BTreeMap::from([
            ("Path".to_owned(), r"C:\node;C:\Windows".to_owned()),
            ("msys_no_pathconv".to_owned(), "0".to_owned()),
        ]);
        shell_env(&mut env, shell);
        assert_eq!(
            env,
            BTreeMap::from([
                ("MSYS2_ARG_CONV_EXCL".to_owned(), "*".to_owned()),
                ("MSYS_NO_PATHCONV".to_owned(), "1".to_owned()),
                (
                    "Path".to_owned(),
                    r"C:\node;C:\Windows;C:\Git\usr\bin".to_owned()
                ),
            ])
        );
        shell_env(&mut env, shell);
        assert_eq!(env["Path"], r"C:\node;C:\Windows;C:\Git\usr\bin");
        let mut trailing = BTreeMap::from([("PATH".to_owned(), r"C:\a;".to_owned())]);
        shell_env(&mut trailing, shell);
        assert_eq!(trailing["PATH"], r"C:\a;C:\Git\usr\bin");
        let mut none = BTreeMap::new();
        shell_env(&mut none, shell);
        assert_eq!(none["PATH"], r"C:\Git\usr\bin");
    }

    /// Git for Windows is a development requirement on Windows, so its
    /// `sh.exe` is found.
    #[tokio::test]
    async fn finds_git_sh() {
        let shell = windows_shell().await.expect("Git for Windows' sh.exe");
        assert!(shell.is_file(), "{}", shell.display());
        assert!(shell.ends_with(Path::new("usr").join("bin").join("sh.exe")));
    }
}

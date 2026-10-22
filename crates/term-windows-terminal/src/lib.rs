//! Windows Terminal launcher: a new tab in the most recent window,
//! `wt.exe -w 0 new-tab --inheritEnvironment --startingDirectory <dir> -- <command>`
//! on Windows, and from inside WSL a tab that runs the command in the same
//! distro through `wsl.exe`.

use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: windows-terminal` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsTerminal;

impl TerminalLauncher for WindowsTerminal {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "windows-terminal"
    }

    /// Plans [`native_invocation`].
    #[cfg(windows)]
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        native_invocation(request)
    }

    /// Inside WSL with interop, plans [`wsl_invocation`] for this distro and
    /// user; elsewhere on Linux there is no Windows Terminal.
    #[cfg(target_os = "linux")]
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        match devlanes_sys::wsl() {
            Some(wsl) if wsl.interop => {
                wsl_invocation(request, wsl.distro.as_deref(), user_name().as_deref())
            }
            _ => Err(LaunchError::Unsupported {
                terminal: "Windows Terminal",
            }),
        }
    }

    /// Windows Terminal runs only on Windows, and from WSL.
    #[cfg(not(any(windows, target_os = "linux")))]
    fn invocation(&self, _request: &LaunchRequest) -> Result<Invocation, LaunchError> {
        Err(LaunchError::Unsupported {
            terminal: "Windows Terminal",
        })
    }

    /// The `wt.exe` App Execution Alias on `PATH`, which Windows Terminal
    /// installs under `%LOCALAPPDATA%\Microsoft\WindowsApps`.
    #[cfg(windows)]
    fn available(&self) -> bool {
        devlanes_term::on_path("wt")
    }

    /// Inside WSL with interop, `wt.exe` on `PATH`: WSL appends the Windows
    /// `PATH`, alias directory included, unless `appendWindowsPath` is off in
    /// `/etc/wsl.conf`.
    #[cfg(target_os = "linux")]
    fn available(&self) -> bool {
        devlanes_sys::wsl().is_some_and(|wsl| wsl.interop) && devlanes_term::on_path("wt.exe")
    }

    /// Windows Terminal runs only on Windows, and from WSL.
    #[cfg(not(any(windows, target_os = "linux")))]
    fn available(&self) -> bool {
        false
    }
}

/// A tab in the most recently used window (`-w 0`), or a new window when none
/// is open. `wt.exe` hands its environment to the tab's command, so the
/// request's `env` is set on it. `--inheritEnvironment` is the default for a
/// tab with a command line since Windows Terminal 1.18, but a profile's
/// `compatibility.reloadEnvironmentVariables` can change the default; the flag
/// wins over the profile.
#[cfg(any(windows, test))]
fn native_invocation(request: &LaunchRequest) -> Result<Invocation, LaunchError> {
    let command = request.require_command()?;
    let mut args: Vec<String> = [
        "-w",
        "0",
        "new-tab",
        "--inheritEnvironment",
        "--startingDirectory",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(escape(&request.cwd_arg()));
    if let Some(title) = &request.title {
        args.extend(["--title".into(), escape(title)]);
    }
    // Everything after `--` is the tab's command line, even words that look
    // like wt options.
    args.push("--".into());
    args.extend(command.iter().map(|word| escape(word)));
    // wt.exe hands the request to the running Terminal and exits, so its
    // status is worth waiting for.
    Ok(Invocation::new("wt.exe", args, true).with_env(&request.env))
}

/// The tab for a request made inside WSL: `wt.exe -w 0 new-tab [--title <title>]
/// -- wsl.exe [-d <distro>] [-u <user>] --cd <cwd> --exec [env K=V...] <command>`.
///
/// `--exec` runs the command without the distro's shell, which would split
/// and expand the words again (`wsl.exe -- <command>` joins them for
/// `$SHELL -c`). The working directory is the Linux path, which `--cd` takes as
/// is. Without a distro name (`WSL_DISTRO_NAME` unset) the tab opens the
/// default distro. `-u` makes the tab run as the caller rather than the
/// distro's default user, so it reaches the caller's daemon and files. The
/// request's `env` reaches the command through `env`, because Windows does not
/// pass its environment into WSL.
#[cfg(any(target_os = "linux", test))]
fn wsl_invocation(
    request: &LaunchRequest,
    distro: Option<&str>,
    user: Option<&str>,
) -> Result<Invocation, LaunchError> {
    let command = request.require_command()?;
    let mut args: Vec<String> = ["-w", "0", "new-tab"].map(str::to_owned).to_vec();
    if let Some(title) = &request.title {
        args.extend(["--title".into(), escape(title)]);
    }
    // wt would read `-d` and `--cd` as its own options without `--`.
    args.extend(["--".into(), "wsl.exe".into()]);
    if let Some(distro) = distro {
        args.extend(["-d".into(), escape(distro)]);
    }
    if let Some(user) = user {
        args.extend(["-u".into(), escape(user)]);
    }
    args.extend(["--cd".into(), escape(&request.cwd_arg()), "--exec".into()]);
    if !request.env.is_empty() {
        args.push("env".into());
        args.extend(
            request
                .env
                .iter()
                .map(|(key, value)| escape(&format!("{key}={value}"))),
        );
    }
    args.extend(command.iter().map(|word| escape(word)));
    Ok(Invocation::new("wt.exe", args, true))
}

/// The name of the user this process runs as: the `/etc/passwd` entry for the
/// owner of `/proc/self` (the effective uid), else `$USER`. `None` when neither
/// says, and the tab then opens as the distro's default user. Blocking, like
/// every launch.
#[cfg(target_os = "linux")]
fn user_name() -> Option<String> {
    use std::os::unix::fs::MetadataExt as _;
    let from_passwd = || {
        let uid = std::fs::metadata("/proc/self").ok()?.uid();
        let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
        passwd_name(&passwd, uid)
    };
    from_passwd().or_else(|| {
        std::env::var("USER")
            .ok()
            .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
    })
}

/// The name of `uid` in `/etc/passwd` text (`name:x:uid:gid:...`).
#[cfg(any(target_os = "linux", test))]
fn passwd_name(passwd: &str, uid: u32) -> Option<String> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let found = fields.nth(1)?.parse::<u32>().ok()? == uid;
        (found && !name.is_empty()).then(|| name.to_owned())
    })
}

/// Escapes `;` as `\;`. wt.exe splits its arguments into subcommands at every
/// `;`, also inside `--` and quoted words, and turns `\;` back into `;`.
#[cfg(any(windows, target_os = "linux", test))]
fn escape(word: &str) -> String {
    word.replace(';', r"\;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> LaunchRequest {
        LaunchRequest {
            cwd: r"C:\src\acme lanes\checkout".into(),
            command: vec![
                r"C:\Program Files\devlanes\devlanes.exe".into(),
                "attach".into(),
                "acme@2:api".into(),
            ],
            title: Some("acme@2:api".into()),
            env: vec![(
                "DEVLANES_HOME".to_owned(),
                r"C:\Users\acme\.devlanes".to_owned(),
            )],
        }
    }

    /// What the TUI asks for inside WSL: `env DEVLANES_HOME=...` already leads
    /// the command, and `env` stays empty.
    fn wsl_request() -> LaunchRequest {
        LaunchRequest {
            cwd: "/home/acme/src/acme lanes".into(),
            command: vec![
                "env".into(),
                "DEVLANES_HOME=/home/acme/.devlanes".into(),
                "/home/acme/.local/bin/devlanes".into(),
                "attach".into(),
                "acme@2:api".into(),
            ],
            title: Some("acme@2:api".into()),
            env: Vec::new(),
        }
    }

    /// The tab runs `wsl.exe` in this distro as this user at the Linux
    /// directory, without a shell, after `--` so wt does not take `-d`, `-u`
    /// and `--cd` as its own.
    #[test]
    fn wsl_plans_a_tab_in_the_same_distro() -> Result<(), LaunchError> {
        let invocation = wsl_invocation(&wsl_request(), Some("Ubuntu"), Some("acme"))?;
        assert_eq!(invocation.program, "wt.exe");
        assert!(invocation.wait);
        assert!(invocation.env.is_empty());
        assert_eq!(
            invocation.args,
            [
                "-w",
                "0",
                "new-tab",
                "--title",
                "acme@2:api",
                "--",
                "wsl.exe",
                "-d",
                "Ubuntu",
                "-u",
                "acme",
                "--cd",
                "/home/acme/src/acme lanes",
                "--exec",
                "env",
                "DEVLANES_HOME=/home/acme/.devlanes",
                "/home/acme/.local/bin/devlanes",
                "attach",
                "acme@2:api",
            ]
        );
        Ok(())
    }

    /// Without a distro or user name the defaults open; variables in `env` go
    /// through `env`, and `;` is escaped everywhere.
    #[test]
    fn wsl_without_distro_with_env_and_semicolons() -> Result<(), LaunchError> {
        let invocation = wsl_invocation(
            &LaunchRequest {
                cwd: "/src/a;b".into(),
                command: vec!["sh".into(), "-c".into(), "echo one;two".into()],
                title: None,
                env: vec![("DEVLANES_HOME".into(), "/x;y".into())],
            },
            None,
            None,
        )?;
        assert_eq!(
            invocation.args,
            [
                "-w",
                "0",
                "new-tab",
                "--",
                "wsl.exe",
                "--cd",
                r"/src/a\;b",
                "--exec",
                "env",
                r"DEVLANES_HOME=/x\;y",
                "sh",
                "-c",
                r"echo one\;two",
            ]
        );
        assert!(matches!(
            wsl_invocation(
                &LaunchRequest {
                    command: Vec::new(),
                    ..wsl_request()
                },
                None,
                None
            ),
            Err(LaunchError::EmptyCommand)
        ));
        Ok(())
    }

    /// The user name comes from the passwd line whose uid matches.
    #[test]
    fn passwd_names_by_uid() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\n\
                      broken line\n\
                      acme:x:1000:1000:Acme,,,:/home/acme:/bin/bash\n";
        assert_eq!(passwd_name(passwd, 0).as_deref(), Some("root"));
        assert_eq!(passwd_name(passwd, 1000).as_deref(), Some("acme"));
        assert_eq!(passwd_name(passwd, 1001), None);
    }

    #[test]
    fn plans_a_new_tab_in_the_recent_window() -> Result<(), LaunchError> {
        let invocation = native_invocation(&request())?;
        assert_eq!(invocation.program, "wt.exe");
        assert!(invocation.wait);
        assert_eq!(
            invocation.args,
            [
                "-w",
                "0",
                "new-tab",
                "--inheritEnvironment",
                "--startingDirectory",
                r"C:\src\acme lanes\checkout",
                "--title",
                "acme@2:api",
                "--",
                r"C:\Program Files\devlanes\devlanes.exe",
                "attach",
                "acme@2:api",
            ]
        );
        assert_eq!(invocation.env, request().env);
        Ok(())
    }

    /// A `;` anywhere would start a second wt subcommand.
    #[test]
    fn escapes_semicolons_in_every_word() -> Result<(), LaunchError> {
        let invocation = native_invocation(&LaunchRequest {
            cwd: r"C:\src\a;b".into(),
            command: vec!["cmd.exe".into(), "/c".into(), "echo one;two".into()],
            title: Some("x;y".into()),
            env: Vec::new(),
        })?;
        assert_eq!(invocation.args[5], r"C:\src\a\;b");
        assert_eq!(invocation.args[7], r"x\;y");
        assert_eq!(
            invocation.args.last().map(String::as_str),
            Some(r"echo one\;two")
        );
        assert!(
            invocation
                .args
                .iter()
                .all(|arg| !arg.replace(r"\;", "").contains(';')),
            "{:?}",
            invocation.args
        );
        Ok(())
    }

    #[test]
    fn untitled_requests_omit_the_title_flag() -> Result<(), LaunchError> {
        let invocation = native_invocation(&LaunchRequest {
            title: None,
            ..request()
        })?;
        assert!(!invocation.args.iter().any(|arg| arg == "--title"));
        Ok(())
    }

    #[test]
    fn empty_command_is_rejected() {
        let result = native_invocation(&LaunchRequest {
            command: Vec::new(),
            ..request()
        });
        assert!(matches!(result, Err(LaunchError::EmptyCommand)));
    }

    /// Outside Windows and WSL there is nothing to launch.
    #[cfg(target_os = "macos")]
    #[test]
    fn unsupported_on_macos() {
        assert!(!WindowsTerminal.available());
        assert!(matches!(
            WindowsTerminal.invocation(&wsl_request()),
            Err(LaunchError::Unsupported { .. })
        ));
    }

    /// Opens a real tab whose command writes the variable and a `;` to a file:
    /// the environment reaches the tab and the escape survives wt's parsing.
    #[cfg(windows)]
    #[test]
    #[ignore = "needs Windows Terminal and a desktop session; opens a tab"]
    fn opened_tab_gets_env_and_semicolons() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let out = dir.path().join("out.txt");
        WindowsTerminal.launch(&LaunchRequest {
            cwd: dir.path().to_owned(),
            command: vec![
                "cmd.exe".into(),
                "/d".into(),
                "/c".into(),
                format!("echo [%DEVLANES_WT_TEST%] one;two> {}", out.display()),
            ],
            title: Some("devlanes;test".into()),
            env: vec![("DEVLANES_WT_TEST".into(), "from-test".into())],
        })?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !out.exists() {
            if std::time::Instant::now() > deadline {
                return Err("the tab did not run its command within 15 seconds".into());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        // cmd may still be writing; give it a moment to close the file.
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(
            std::fs::read_to_string(&out)?.trim_end(),
            "[from-test] one;two"
        );
        Ok(())
    }
}

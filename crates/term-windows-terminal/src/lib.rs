//! Windows Terminal launcher: a new tab in the most recent window,
//! `wt.exe -w 0 new-tab --inheritEnvironment --startingDirectory <dir> -- <command>`.

use devlanes_term::{Invocation, LaunchError, LaunchRequest, TerminalLauncher};

/// Selected by `terminal: windows-terminal` in the user config.
#[derive(Debug, Clone, Copy, Default)]
pub struct WindowsTerminal;

impl TerminalLauncher for WindowsTerminal {
    /// Returns the user config value.
    fn name(&self) -> &'static str {
        "windows-terminal"
    }

    /// Plans a tab in the most recently used window (`-w 0`), or a new window
    /// when none is open. `wt.exe` hands its environment to the tab's command,
    /// so the request's `env` is set on it. `--inheritEnvironment` is the
    /// default for a tab with a command line since Windows Terminal 1.18, but
    /// a profile's `compatibility.reloadEnvironmentVariables` can change the
    /// default; the flag wins over the profile.
    fn invocation(&self, request: &LaunchRequest) -> Result<Invocation, LaunchError> {
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
        // Everything after `--` is the tab's command line, even words that
        // look like wt options.
        args.push("--".into());
        args.extend(command.iter().map(|word| escape(word)));
        // wt.exe hands the request to the running Terminal and exits, so its
        // status is worth waiting for.
        Ok(Invocation::new("wt.exe", args, true).with_env(&request.env))
    }

    /// The `wt.exe` App Execution Alias on `PATH`, which Windows Terminal
    /// installs under `%LOCALAPPDATA%\Microsoft\WindowsApps`.
    #[cfg(windows)]
    fn available(&self) -> bool {
        devlanes_term::on_path("wt")
    }

    /// Windows Terminal exists only on Windows.
    #[cfg(not(windows))]
    fn available(&self) -> bool {
        false
    }
}

/// Escapes `;` as `\;`. wt.exe splits its arguments into subcommands at every
/// `;`, also inside `--` and quoted words, and turns `\;` back into `;`.
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

    #[test]
    fn plans_a_new_tab_in_the_recent_window() -> Result<(), LaunchError> {
        let invocation = WindowsTerminal.invocation(&request())?;
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
        let invocation = WindowsTerminal.invocation(&LaunchRequest {
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
        let invocation = WindowsTerminal.invocation(&LaunchRequest {
            title: None,
            ..request()
        })?;
        assert!(!invocation.args.iter().any(|arg| arg == "--title"));
        Ok(())
    }

    #[test]
    fn empty_command_is_rejected() {
        let result = WindowsTerminal.invocation(&LaunchRequest {
            command: Vec::new(),
            ..request()
        });
        assert!(matches!(result, Err(LaunchError::EmptyCommand)));
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
